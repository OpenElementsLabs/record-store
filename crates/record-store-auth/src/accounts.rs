use std::{path::Path, sync::Arc};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use record_store_core::{OrganizationId, ServiceAccountId};
use redb::{Database, ReadableTable};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::crypto::{derive_encryption_key, encrypt_record, random_secret_bytes};
use crate::evaluate::enrich_info;
use crate::keys::{account_credential_key, policy_binding_key, store_backend};
use crate::manager::EncryptedCredentialRecord;
use crate::schema::{
    ACCESS_KEYS, ACCOUNT_CREDENTIALS, ACCOUNTS, POLICIES, POLICY_BINDINGS, POLICY_NAMES,
    ROTATED_CREDENTIALS,
};
use crate::validation::{validate_description, validate_service_account_name};
use crate::*;

impl CredentialManager {
    /// Opens the credential database. A dedicated master key is preferred; when
    /// absent the root secret derives the version-1 encryption key.
    pub async fn open(
        path: impl AsRef<Path>,
        root_access_key: impl Into<String>,
        root_secret: impl AsRef<[u8]>,
        credential_master_key: Option<&[u8]>,
    ) -> Result<Self, CredentialStoreError> {
        let path = path.as_ref().to_path_buf();
        let parent = path.parent().map(Path::to_path_buf);
        let root_access_key = root_access_key.into();
        let root_secret_bytes = Zeroizing::new(root_secret.as_ref().to_vec());
        let explicit_master_key = credential_master_key.is_some();
        let derivation_material = Zeroizing::new(
            credential_master_key.map_or_else(|| root_secret_bytes.to_vec(), <[u8]>::to_vec),
        );
        let encryption_key = derive_encryption_key(&derivation_material)?;
        let database = tokio::task::spawn_blocking(move || {
            if let Some(parent) = parent {
                std::fs::create_dir_all(parent).map_err(CredentialStoreError::Directory)?;
            }
            let database = Database::create(path).map_err(store_backend)?;
            let write = database.begin_write().map_err(store_backend)?;
            {
                write.open_table(ACCOUNTS).map_err(store_backend)?;
                write.open_table(ACCESS_KEYS).map_err(store_backend)?;
                write
                    .open_table(ROTATED_CREDENTIALS)
                    .map_err(store_backend)?;
                write
                    .open_table(ACCOUNT_CREDENTIALS)
                    .map_err(store_backend)?;
                write.open_table(POLICIES).map_err(store_backend)?;
                write.open_table(POLICY_NAMES).map_err(store_backend)?;
                write.open_table(POLICY_BINDINGS).map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)?;
            if !explicit_master_key {
                let read = database.begin_read().map_err(store_backend)?;
                let accounts = read.open_table(ACCOUNTS).map_err(store_backend)?;
                if accounts.iter().map_err(store_backend)?.next().is_some() {
                    return Err(CredentialStoreError::MasterKeyRequired);
                }
            }
            Ok::<_, CredentialStoreError>(database)
        })
        .await??;
        Ok(Self {
            database: Arc::new(database),
            root_access_key: Arc::from(root_access_key),
            root_secret: Arc::new(SigningSecret::new(&root_secret_bytes)),
            encryption_key: Arc::new(Zeroizing::new(encryption_key)),
            explicit_master_key,
        })
    }

    /// Creates and durably stores one encrypted service-account credential.
    pub async fn create_service_account(
        &self,
        name: impl Into<String>,
        organization_id: OrganizationId,
    ) -> Result<IssuedServiceAccount, CredentialStoreError> {
        if !self.explicit_master_key {
            return Err(CredentialStoreError::MasterKeyRequired);
        }
        self.create_service_account_with_description(name, "", organization_id)
            .await
    }

    /// Creates a service account with an operator-facing description.
    pub async fn create_service_account_with_description(
        &self,
        name: impl Into<String>,
        description: impl Into<String>,
        organization_id: OrganizationId,
    ) -> Result<IssuedServiceAccount, CredentialStoreError> {
        if !self.explicit_master_key {
            return Err(CredentialStoreError::MasterKeyRequired);
        }
        let name = name.into();
        validate_service_account_name(&name)?;
        let description = description.into();
        validate_description(&description)?;
        let account = ServiceAccount {
            id: ServiceAccountId::new(),
            organization_id,
            name,
            description,
            disabled: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let access_key = format!(
            "SA{}",
            Uuid::new_v4().simple().to_string()[..20].to_ascii_uppercase()
        );
        let secret_bytes = Zeroizing::new(random_secret_bytes());
        let secret_text = URL_SAFE_NO_PAD.encode(secret_bytes.as_ref());
        let credential = Credential {
            id: Uuid::new_v4(),
            service_account_id: account.id,
            key_id: access_key.clone(),
            disabled: false,
            created_at: Utc::now(),
            expires_at: None,
        };
        let info = ServiceAccountInfo {
            account,
            credential: credential.clone(),
            credentials: vec![credential],
            policy_bindings: Vec::new(),
        };
        let record = encrypt_record(info.clone(), secret_text.as_bytes(), &self.encryption_key)?;
        let encoded = serde_json::to_vec(&record)?;
        let id_key = info.account.id.as_uuid().as_bytes().as_slice().to_vec();
        let account_id = info.account.id;
        let credential_id = info.credential.id;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let mut keys = write.open_table(ACCESS_KEYS).map_err(store_backend)?;
                if keys
                    .get(access_key.as_str())
                    .map_err(store_backend)?
                    .is_some()
                {
                    return Err(CredentialStoreError::AccessKeyCollision);
                }
                keys.insert(access_key.as_str(), id_key.as_slice())
                    .map_err(store_backend)?;
            }
            {
                let mut accounts = write.open_table(ACCOUNTS).map_err(store_backend)?;
                accounts
                    .insert(id_key.as_slice(), encoded.as_slice())
                    .map_err(store_backend)?;
            }
            {
                let mut credentials = write
                    .open_table(ACCOUNT_CREDENTIALS)
                    .map_err(store_backend)?;
                credentials
                    .insert(
                        account_credential_key(account_id, credential_id).as_slice(),
                        &1,
                    )
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await??;
        Ok(IssuedServiceAccount {
            info,
            secret: SigningSecret::new(secret_text),
        })
    }

    /// Lists public descriptors without decrypting any secret.
    pub async fn list_service_accounts(
        &self,
    ) -> Result<Vec<ServiceAccountInfo>, CredentialStoreError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database.begin_read().map_err(store_backend)?;
            let table = read.open_table(ACCOUNTS).map_err(store_backend)?;
            let mut result = Vec::new();
            for entry in table.iter().map_err(store_backend)? {
                let (_, value) = entry.map_err(store_backend)?;
                let record: EncryptedCredentialRecord = serde_json::from_slice(value.value())?;
                result.push(enrich_info(&read, record.info)?);
            }
            result.sort_by(|left, right| left.account.name.cmp(&right.account.name));
            Ok(result)
        })
        .await?
    }

    /// Inspects one account without decrypting any credential secret.
    pub async fn get_service_account(
        &self,
        account_id: ServiceAccountId,
    ) -> Result<ServiceAccountInfo, CredentialStoreError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database.begin_read().map_err(store_backend)?;
            let table = read.open_table(ACCOUNTS).map_err(store_backend)?;
            let bytes = table
                .get(account_id.as_uuid().as_bytes().as_slice())
                .map_err(store_backend)?
                .map(|value| value.value().to_vec())
                .ok_or(CredentialStoreError::AccountNotFound)?;
            let record: EncryptedCredentialRecord = serde_json::from_slice(&bytes)?;
            enrich_info(&read, record.info)
        })
        .await?
    }

    /// Enables or disables an account without deleting its history.
    pub async fn set_service_account_enabled(
        &self,
        account_id: ServiceAccountId,
        enabled: bool,
    ) -> Result<ServiceAccountInfo, CredentialStoreError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            let info = {
                let mut table = write.open_table(ACCOUNTS).map_err(store_backend)?;
                let bytes = table
                    .get(account_id.as_uuid().as_bytes().as_slice())
                    .map_err(store_backend)?
                    .map(|value| value.value().to_vec())
                    .ok_or(CredentialStoreError::AccountNotFound)?;
                let mut record: EncryptedCredentialRecord = serde_json::from_slice(&bytes)?;
                record.info.account.disabled = !enabled;
                record.info.account.updated_at = Utc::now();
                let info = record.info.clone();
                let bytes = serde_json::to_vec(&record)?;
                table
                    .insert(account_id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
                    .map_err(store_backend)?;
                info
            };
            write.commit().map_err(store_backend)?;
            Ok(info)
        })
        .await?
    }

    /// Issues an additional independently revocable credential for safe rotation.
    pub async fn rotate_credential(
        &self,
        account_id: ServiceAccountId,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<IssuedServiceAccount, CredentialStoreError> {
        if !self.explicit_master_key {
            return Err(CredentialStoreError::MasterKeyRequired);
        }
        let account_info = self.get_service_account(account_id).await?;
        if account_info.account.disabled {
            return Err(CredentialStoreError::CredentialInactive);
        }
        let access_key = format!(
            "SA{}",
            Uuid::new_v4().simple().to_string()[..20].to_ascii_uppercase()
        );
        let secret_bytes = Zeroizing::new(random_secret_bytes());
        let secret_text = URL_SAFE_NO_PAD.encode(secret_bytes.as_ref());
        let credential = Credential {
            id: Uuid::new_v4(),
            service_account_id: account_id,
            key_id: access_key.clone(),
            disabled: false,
            created_at: Utc::now(),
            expires_at,
        };
        let info = ServiceAccountInfo {
            account: account_info.account,
            credential: credential.clone(),
            credentials: vec![credential.clone()],
            policy_bindings: account_info.policy_bindings,
        };
        let record = encrypt_record(info.clone(), secret_text.as_bytes(), &self.encryption_key)?;
        let bytes = serde_json::to_vec(&record)?;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            let mut index_value = account_id.as_uuid().as_bytes().as_slice().to_vec();
            index_value.extend_from_slice(credential.id.as_bytes().as_slice());
            {
                let mut keys = write.open_table(ACCESS_KEYS).map_err(store_backend)?;
                if keys
                    .get(access_key.as_str())
                    .map_err(store_backend)?
                    .is_some()
                {
                    return Err(CredentialStoreError::AccessKeyCollision);
                }
                keys.insert(access_key.as_str(), index_value.as_slice())
                    .map_err(store_backend)?;
            }
            {
                let mut records = write
                    .open_table(ROTATED_CREDENTIALS)
                    .map_err(store_backend)?;
                records
                    .insert(credential.id.as_bytes().as_slice(), bytes.as_slice())
                    .map_err(store_backend)?;
            }
            {
                let mut index = write
                    .open_table(ACCOUNT_CREDENTIALS)
                    .map_err(store_backend)?;
                index
                    .insert(
                        account_credential_key(account_id, credential.id).as_slice(),
                        &1,
                    )
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await??;
        Ok(IssuedServiceAccount {
            info,
            secret: SigningSecret::new(secret_text),
        })
    }

    /// Enables or disables one credential independently from its account.
    pub async fn set_credential_enabled(
        &self,
        account_id: ServiceAccountId,
        credential_id: Uuid,
        enabled: bool,
    ) -> Result<(), CredentialStoreError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            let primary_updated = {
                let mut accounts = write.open_table(ACCOUNTS).map_err(store_backend)?;
                let bytes = accounts
                    .get(account_id.as_uuid().as_bytes().as_slice())
                    .map_err(store_backend)?
                    .map(|value| value.value().to_vec())
                    .ok_or(CredentialStoreError::AccountNotFound)?;
                let mut record: EncryptedCredentialRecord = serde_json::from_slice(&bytes)?;
                if record.info.credential.id == credential_id {
                    record.info.credential.disabled = !enabled;
                    if let Some(credential) = record
                        .info
                        .credentials
                        .iter_mut()
                        .find(|value| value.id == credential_id)
                    {
                        credential.disabled = !enabled;
                    }
                    let bytes = serde_json::to_vec(&record)?;
                    accounts
                        .insert(account_id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
                        .map_err(store_backend)?;
                    true
                } else {
                    false
                }
            };
            if !primary_updated {
                let mut records = write
                    .open_table(ROTATED_CREDENTIALS)
                    .map_err(store_backend)?;
                let bytes = records
                    .get(credential_id.as_bytes().as_slice())
                    .map_err(store_backend)?
                    .map(|value| value.value().to_vec())
                    .ok_or(CredentialStoreError::CredentialNotFound)?;
                let mut record: EncryptedCredentialRecord = serde_json::from_slice(&bytes)?;
                if record.info.credential.service_account_id != account_id {
                    return Err(CredentialStoreError::CredentialNotFound);
                }
                record.info.credential.disabled = !enabled;
                let bytes = serde_json::to_vec(&record)?;
                records
                    .insert(credential_id.as_bytes().as_slice(), bytes.as_slice())
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await?
    }

    /// Permanently removes an account and every access-key lookup.
    pub async fn delete_service_account(
        &self,
        account_id: ServiceAccountId,
    ) -> Result<(), CredentialStoreError> {
        let info = self.get_service_account(account_id).await?;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let mut keys = write.open_table(ACCESS_KEYS).map_err(store_backend)?;
                for credential in &info.credentials {
                    keys.remove(credential.key_id.as_str())
                        .map_err(store_backend)?;
                }
            }
            {
                let mut records = write
                    .open_table(ROTATED_CREDENTIALS)
                    .map_err(store_backend)?;
                for credential in info
                    .credentials
                    .iter()
                    .filter(|value| value.id != info.credential.id)
                {
                    records
                        .remove(credential.id.as_bytes().as_slice())
                        .map_err(store_backend)?;
                }
            }
            {
                let mut accounts = write.open_table(ACCOUNTS).map_err(store_backend)?;
                accounts
                    .remove(account_id.as_uuid().as_bytes().as_slice())
                    .map_err(store_backend)?;
            }
            {
                let mut bindings = write.open_table(POLICY_BINDINGS).map_err(store_backend)?;
                for policy in &info.policy_bindings {
                    bindings
                        .remove(policy_binding_key(account_id, *policy).as_slice())
                        .map_err(store_backend)?;
                }
            }
            write.commit().map_err(store_backend)
        })
        .await?
    }

    /// Disables an account and its current credential without deleting audit state.
    pub async fn revoke_service_account(
        &self,
        account_id: ServiceAccountId,
    ) -> Result<(), CredentialStoreError> {
        let database = Arc::clone(&self.database);
        let key = account_id.as_uuid().as_bytes().as_slice().to_vec();
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let mut table = write.open_table(ACCOUNTS).map_err(store_backend)?;
                let encoded = table
                    .get(key.as_slice())
                    .map_err(store_backend)?
                    .map(|value| value.value().to_vec())
                    .ok_or(CredentialStoreError::AccountNotFound)?;
                let mut record: EncryptedCredentialRecord = serde_json::from_slice(&encoded)?;
                record.info.account.disabled = true;
                record.info.credential.disabled = true;
                let encoded = serde_json::to_vec(&record)?;
                table
                    .insert(key.as_slice(), encoded.as_slice())
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use record_store_core::OrganizationId;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn service_account_secrets_are_encrypted_persistent_and_revocable() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("credentials.redb");
        let organization = OrganizationId::new();
        let manager = CredentialManager::open(
            &path,
            "root-access",
            b"root-secret-at-least-sixteen",
            Some(b"dedicated-master-key-at-least-thirty-two-bytes"),
        )
        .await
        .expect("credential manager");
        let issued = manager
            .create_service_account("backup-agent", organization)
            .await
            .expect("create account");
        let access_key = issued.info.credential.key_id.clone();
        let account_id = issued.info.account.id;
        let secret = issued.secret.expose().to_vec();
        assert!(!format!("{issued:?}").contains(String::from_utf8_lossy(&secret).as_ref()));
        drop(manager);

        let database_bytes = std::fs::read(&path).expect("credential database bytes");
        assert!(
            !database_bytes
                .windows(secret.len())
                .any(|window| window == secret)
        );

        let manager = CredentialManager::open(
            &path,
            "root-access",
            b"root-secret-at-least-sixteen",
            Some(b"dedicated-master-key-at-least-thirty-two-bytes"),
        )
        .await
        .expect("reopen credential manager");
        let (_, resolved) = manager
            .signing_secret(&access_key)
            .await
            .expect("resolve signing secret");
        assert_eq!(resolved.expose(), secret);
        manager
            .revoke_service_account(account_id)
            .await
            .expect("revoke account");
        assert!(matches!(
            manager.signing_secret(&access_key).await,
            Err(CredentialLookupError::Inactive)
        ));
    }

    #[tokio::test]
    async fn expired_rotated_credentials_fail_without_cleanup() {
        let directory = tempdir().expect("temporary directory");
        let manager = CredentialManager::open(
            directory.path().join("credentials.redb"),
            "root-access",
            b"root-secret-at-least-sixteen",
            Some(b"dedicated-master-key-at-least-thirty-two-bytes"),
        )
        .await
        .expect("manager");
        let issued = manager
            .create_service_account("temporary-app", OrganizationId::new())
            .await
            .expect("account");
        let temporary = manager
            .rotate_credential(
                issued.info.account.id,
                Some(Utc::now() - chrono::Duration::seconds(1)),
            )
            .await
            .expect("temporary credential");
        assert!(matches!(
            manager
                .signing_secret(&temporary.info.credential.key_id)
                .await,
            Err(CredentialLookupError::Inactive)
        ));
    }

    #[tokio::test]
    async fn service_account_creation_requires_an_explicit_master_key() {
        let directory = tempdir().expect("temporary directory");
        let manager = CredentialManager::open(
            directory.path().join("credentials.redb"),
            "root-access",
            b"root-secret-at-least-sixteen",
            None,
        )
        .await
        .expect("root-only manager");
        assert!(matches!(
            manager
                .create_service_account_with_description(
                    "unsafe-account",
                    "must not be encrypted with an implicit key",
                    OrganizationId::new(),
                )
                .await,
            Err(CredentialStoreError::MasterKeyRequired)
        ));
    }
}
