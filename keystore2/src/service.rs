// Copyright 2020, The Android Open Source Project
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// TODO remove when fully implemented.
#![allow(unused_variables)]

//! This crate implement the core Keystore 2.0 service API as defined by the Keystore 2.0
//! AIDL spec.

use std::collections::HashMap;

use crate::permission::{KeyPerm, KeystorePerm};
use crate::security_level::KeystoreSecurityLevel;
use crate::utils::{
    check_grant_permission, check_key_permission, check_keystore_permission,
    key_parameters_to_authorizations, Asp,
};
use crate::{database::Uuid, globals::DB};
use crate::{database::KEYSTORE_UUID, permission};
use crate::{
    database::{KeyEntryLoadBits, KeyType, SubComponentType},
    error::ResponseCode,
};
use crate::{
    error::{self, map_or_log_err, ErrorCode},
    gc::Gc,
};
use android_hardware_security_keymint::aidl::android::hardware::security::keymint::SecurityLevel::SecurityLevel;
use android_system_keystore2::aidl::android::system::keystore2::{
    Domain::Domain, IKeystoreSecurityLevel::IKeystoreSecurityLevel,
    IKeystoreService::BnKeystoreService, IKeystoreService::IKeystoreService,
    KeyDescriptor::KeyDescriptor, KeyEntryResponse::KeyEntryResponse, KeyMetadata::KeyMetadata,
};
use anyhow::{Context, Result};
use binder::{IBinder, Interface, ThreadState};
use error::Error;
use keystore2_selinux as selinux;

/// Implementation of the IKeystoreService.
#[derive(Default)]
pub struct KeystoreService {
    i_sec_level_by_uuid: HashMap<Uuid, Asp>,
    uuid_by_sec_level: HashMap<SecurityLevel, Uuid>,
}

impl KeystoreService {
    /// Create a new instance of the Keystore 2.0 service.
    pub fn new_native_binder() -> Result<impl IKeystoreService> {
        let mut result: Self = Default::default();
        let (dev, uuid) =
            KeystoreSecurityLevel::new_native_binder(SecurityLevel::TRUSTED_ENVIRONMENT)
                .context(concat!(
                    "In KeystoreService::new_native_binder: ",
                    "Trying to construct mandatory security level TEE."
                ))
                .map(|(dev, uuid)| (Asp::new(dev.as_binder()), uuid))?;
        result.i_sec_level_by_uuid.insert(uuid, dev);
        result.uuid_by_sec_level.insert(SecurityLevel::TRUSTED_ENVIRONMENT, uuid);

        // Strongbox is optional, so we ignore errors and turn the result into an Option.
        if let Ok((dev, uuid)) = KeystoreSecurityLevel::new_native_binder(SecurityLevel::STRONGBOX)
            .map(|(dev, uuid)| (Asp::new(dev.as_binder()), uuid))
        {
            result.i_sec_level_by_uuid.insert(uuid, dev);
            result.uuid_by_sec_level.insert(SecurityLevel::STRONGBOX, uuid);
        }

        let result = BnKeystoreService::new_binder(result);
        result.as_binder().set_requesting_sid(true);
        Ok(result)
    }

    fn uuid_to_sec_level(&self, uuid: &Uuid) -> SecurityLevel {
        self.uuid_by_sec_level
            .iter()
            .find(|(_, v)| **v == *uuid)
            .map(|(s, _)| *s)
            .unwrap_or(SecurityLevel::SOFTWARE)
    }

    fn get_i_sec_level_by_uuid(&self, uuid: &Uuid) -> Result<Box<dyn IKeystoreSecurityLevel>> {
        if let Some(dev) = self.i_sec_level_by_uuid.get(uuid) {
            dev.get_interface().context("In get_i_sec_level_by_uuid.")
        } else {
            Err(error::Error::sys())
                .context("In get_i_sec_level_by_uuid: KeyMint instance for key not found.")
        }
    }

    fn get_security_level(
        &self,
        sec_level: SecurityLevel,
    ) -> Result<Box<dyn IKeystoreSecurityLevel>> {
        if let Some(dev) = self
            .uuid_by_sec_level
            .get(&sec_level)
            .and_then(|uuid| self.i_sec_level_by_uuid.get(uuid))
        {
            dev.get_interface().context("In get_security_level.")
        } else {
            Err(error::Error::Km(ErrorCode::HARDWARE_TYPE_UNAVAILABLE))
                .context("In get_security_level: No such security level.")
        }
    }

    fn get_key_entry(&self, key: &KeyDescriptor) -> Result<KeyEntryResponse> {
        let (key_id_guard, mut key_entry) = DB
            .with(|db| {
                db.borrow_mut().load_key_entry(
                    &key,
                    KeyType::Client,
                    KeyEntryLoadBits::PUBLIC,
                    ThreadState::get_calling_uid(),
                    |k, av| check_key_permission(KeyPerm::get_info(), k, &av),
                )
            })
            .context("In get_key_entry, while trying to load key info.")?;

        let i_sec_level = if !key_entry.pure_cert() {
            Some(
                self.get_i_sec_level_by_uuid(key_entry.km_uuid())
                    .context("In get_key_entry: Trying to get security level proxy.")?,
            )
        } else {
            None
        };

        Ok(KeyEntryResponse {
            iSecurityLevel: i_sec_level,
            metadata: KeyMetadata {
                key: KeyDescriptor {
                    domain: Domain::KEY_ID,
                    nspace: key_id_guard.id(),
                    ..Default::default()
                },
                keySecurityLevel: self.uuid_to_sec_level(key_entry.km_uuid()),
                certificate: key_entry.take_cert(),
                certificateChain: key_entry.take_cert_chain(),
                modificationTimeMs: key_entry
                    .metadata()
                    .creation_date()
                    .map(|d| d.to_millis_epoch())
                    .ok_or(Error::Rc(ResponseCode::VALUE_CORRUPTED))
                    .context("In get_key_entry: Trying to get creation date.")?,
                authorizations: key_parameters_to_authorizations(key_entry.into_key_parameters()),
            },
        })
    }

    fn update_subcomponent(
        &self,
        key: &KeyDescriptor,
        public_cert: Option<&[u8]>,
        certificate_chain: Option<&[u8]>,
    ) -> Result<()> {
        DB.with::<_, Result<()>>(|db| {
            let mut db = db.borrow_mut();
            let entry = match db.load_key_entry(
                &key,
                KeyType::Client,
                KeyEntryLoadBits::NONE,
                ThreadState::get_calling_uid(),
                |k, av| {
                    check_key_permission(KeyPerm::update(), k, &av)
                        .context("In update_subcomponent.")
                },
            ) {
                Err(e) => match e.root_cause().downcast_ref::<Error>() {
                    Some(Error::Rc(ResponseCode::KEY_NOT_FOUND)) => Ok(None),
                    _ => Err(e),
                },
                Ok(v) => Ok(Some(v)),
            }
            .context("Failed to load key entry.")?;

            if let Some((key_id_guard, key_entry)) = entry {
                db.set_blob(&key_id_guard, SubComponentType::CERT, public_cert)
                    .context("Failed to update cert subcomponent.")?;

                db.set_blob(&key_id_guard, SubComponentType::CERT_CHAIN, certificate_chain)
                    .context("Failed to update cert chain subcomponent.")?;
                return Ok(());
            }

            // If we reach this point we have to check the special condition where a certificate
            // entry may be made.
            if !(public_cert.is_none() && certificate_chain.is_some()) {
                return Err(Error::Rc(ResponseCode::KEY_NOT_FOUND)).context("No key to update.");
            }

            // So we know that we have a certificate chain and no public cert.
            // Now check that we have everything we need to make a new certificate entry.
            let key = match (key.domain, &key.alias) {
                (Domain::APP, Some(ref alias)) => KeyDescriptor {
                    domain: Domain::APP,
                    nspace: ThreadState::get_calling_uid() as i64,
                    alias: Some(alias.clone()),
                    blob: None,
                },
                (Domain::SELINUX, Some(_)) => key.clone(),
                _ => {
                    return Err(Error::Rc(ResponseCode::INVALID_ARGUMENT))
                        .context("Domain must be APP or SELINUX to insert a certificate.")
                }
            };

            // Security critical: This must return on failure. Do not remove the `?`;
            check_key_permission(KeyPerm::rebind(), &key, &None)
                .context("Caller does not have permission to insert this certificate.")?;

            db.store_new_certificate(&key, certificate_chain.unwrap(), &KEYSTORE_UUID)
                .context("Failed to insert new certificate.")?;
            Ok(())
        })
        .context("In update_subcomponent.")
    }

    fn list_entries(&self, domain: Domain, namespace: i64) -> Result<Vec<KeyDescriptor>> {
        let mut k = match domain {
            Domain::APP => KeyDescriptor {
                domain,
                nspace: ThreadState::get_calling_uid() as u64 as i64,
                ..Default::default()
            },
            Domain::SELINUX => KeyDescriptor{domain, nspace: namespace, ..Default::default()},
            _ => return Err(Error::perm()).context(
                "In list_entries: List entries is only supported for Domain::APP and Domain::SELINUX."
            ),
        };

        // First we check if the caller has the info permission for the selected domain/namespace.
        // By default we use the calling uid as namespace if domain is Domain::APP.
        // If the first check fails we check if the caller has the list permission allowing to list
        // any namespace. In that case we also adjust the queried namespace if a specific uid was
        // selected.
        match check_key_permission(KeyPerm::get_info(), &k, &None) {
            Err(e) => {
                if let Some(selinux::Error::PermissionDenied) =
                    e.root_cause().downcast_ref::<selinux::Error>()
                {
                    check_keystore_permission(KeystorePerm::list())
                        .context("In list_entries: While checking keystore permission.")?;
                    if namespace != -1 {
                        k.nspace = namespace;
                    }
                } else {
                    return Err(e).context("In list_entries: While checking key permission.")?;
                }
            }
            Ok(()) => {}
        };

        DB.with(|db| {
            let mut db = db.borrow_mut();
            db.list(k.domain, k.nspace)
        })
    }

    fn delete_key(&self, key: &KeyDescriptor) -> Result<()> {
        let caller_uid = ThreadState::get_calling_uid();
        let need_gc = DB
            .with(|db| {
                db.borrow_mut().unbind_key(&key, KeyType::Client, caller_uid, |k, av| {
                    check_key_permission(KeyPerm::delete(), k, &av).context("During delete_key.")
                })
            })
            .context("In delete_key: Trying to unbind the key.")?;
        if need_gc {
            Gc::notify_gc();
        }
        Ok(())
    }

    fn grant(
        &self,
        key: &KeyDescriptor,
        grantee_uid: i32,
        access_vector: permission::KeyPermSet,
    ) -> Result<KeyDescriptor> {
        DB.with(|db| {
            db.borrow_mut().grant(
                &key,
                ThreadState::get_calling_uid(),
                grantee_uid as u32,
                access_vector,
                |k, av| check_grant_permission(*av, k).context("During grant."),
            )
        })
        .context("In KeystoreService::grant.")
    }

    fn ungrant(&self, key: &KeyDescriptor, grantee_uid: i32) -> Result<()> {
        DB.with(|db| {
            db.borrow_mut().ungrant(&key, ThreadState::get_calling_uid(), grantee_uid as u32, |k| {
                check_key_permission(KeyPerm::grant(), k, &None)
            })
        })
        .context("In KeystoreService::ungrant.")
    }
}

impl binder::Interface for KeystoreService {}

// Implementation of IKeystoreService. See AIDL spec at
// system/security/keystore2/binder/android/security/keystore2/IKeystoreService.aidl
impl IKeystoreService for KeystoreService {
    fn getSecurityLevel(
        &self,
        security_level: SecurityLevel,
    ) -> binder::public_api::Result<Box<dyn IKeystoreSecurityLevel>> {
        map_or_log_err(self.get_security_level(security_level), Ok)
    }
    fn getKeyEntry(&self, key: &KeyDescriptor) -> binder::public_api::Result<KeyEntryResponse> {
        map_or_log_err(self.get_key_entry(key), Ok)
    }
    fn updateSubcomponent(
        &self,
        key: &KeyDescriptor,
        public_cert: Option<&[u8]>,
        certificate_chain: Option<&[u8]>,
    ) -> binder::public_api::Result<()> {
        map_or_log_err(self.update_subcomponent(key, public_cert, certificate_chain), Ok)
    }
    fn listEntries(
        &self,
        domain: Domain,
        namespace: i64,
    ) -> binder::public_api::Result<Vec<KeyDescriptor>> {
        map_or_log_err(self.list_entries(domain, namespace), Ok)
    }
    fn deleteKey(&self, key: &KeyDescriptor) -> binder::public_api::Result<()> {
        map_or_log_err(self.delete_key(key), Ok)
    }
    fn grant(
        &self,
        key: &KeyDescriptor,
        grantee_uid: i32,
        access_vector: i32,
    ) -> binder::public_api::Result<KeyDescriptor> {
        map_or_log_err(self.grant(key, grantee_uid, access_vector.into()), Ok)
    }
    fn ungrant(&self, key: &KeyDescriptor, grantee_uid: i32) -> binder::public_api::Result<()> {
        map_or_log_err(self.ungrant(key, grantee_uid), Ok)
    }
}
