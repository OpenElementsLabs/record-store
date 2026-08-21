//! Authentication and authorization contracts.
//!
//! This crate intentionally provides boundaries and safe persisted descriptors,
//! not an IAM implementation. Secret verifiers belong in credential backends and
//! must never be represented by [`Credential`].

use std::{
    fmt::{self, Debug, Formatter},
    path::Path,
    sync::Arc,
};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use oes_core::{BucketId, OrganizationId, ServiceAccountId};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

/// An authenticated caller identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// A non-human workload identity.
    ServiceAccount {
        /// Stable service-account identifier.
        id: ServiceAccountId,
        /// Owning organization.
        organization_id: OrganizationId,
    },
    /// A trusted internal OES component.
    System {
        /// Stable component name.
        component: String,
    },
    /// No credential has been authenticated.
    Anonymous,
}

/// A workload identity descriptor. It never contains credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAccount {
    /// Stable identifier.
    pub id: ServiceAccountId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Operator-facing name.
    pub name: String,
    /// Whether authentication is currently disabled.
    pub disabled: bool,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// Public credential descriptor. Secret hashes remain inside a credential backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// Stable credential identifier.
    pub id: Uuid,
    /// Owning service account.
    pub service_account_id: ServiceAccountId,
    /// Public lookup identifier, analogous to an access-key ID.
    pub key_id: String,
    /// Whether the credential is inactive.
    pub disabled: bool,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Optional expiry time.
    pub expires_at: Option<DateTime<Utc>>,
}

/// An authorization action understood by the current core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Read object bytes or metadata.
    ReadObject,
    /// Create or replace an object.
    WriteObject,
    /// Delete an object.
    DeleteObject,
    /// Read bucket metadata.
    ReadBucket,
    /// Change bucket state.
    ManageBucket,
}

/// A typed authorization resource scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resource {
    /// Every resource in an organization.
    Organization {
        /// Organization identifier.
        id: OrganizationId,
    },
    /// A specific bucket.
    Bucket {
        /// Bucket identifier.
        id: BucketId,
    },
}

/// A grant combining one action with one resource scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Permission {
    /// Allowed action.
    pub action: Action,
    /// Resource scope.
    pub resource: Resource,
}

/// Explicit inputs to an authorization decision.
#[derive(Debug, Clone)]
pub struct AuthorizationContext<'a> {
    /// Authenticated caller.
    pub principal: &'a Principal,
    /// Requested permission.
    pub permission: &'a Permission,
}

/// Authentication boundary for future credential backends.
#[async_trait]
pub trait Authenticator: Send + Sync {
    /// Verifies opaque credential proof and returns an authenticated principal.
    /// Implementations must use constant-time comparison where applicable.
    async fn authenticate(
        &self,
        public_key_id: &str,
        credential_proof: &[u8],
    ) -> Result<Principal, AuthenticationError>;
}

/// Authorization boundary kept separate from credential verification.
#[async_trait]
pub trait Authorizer: Send + Sync {
    /// Returns success only when the requested permission is granted.
    async fn authorize(&self, context: AuthorizationContext<'_>) -> Result<(), AuthorizationError>;
}

/// Stable authentication failure categories.
#[derive(Debug, Error)]
pub enum AuthenticationError {
    /// No valid credential matched the proof.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// The matching credential cannot currently be used.
    #[error("credential is disabled or expired")]
    CredentialInactive,
    /// The credential backend failed internally.
    #[error("authentication backend unavailable")]
    BackendUnavailable,
}

/// Stable authorization failure categories.
#[derive(Debug, Error)]
pub enum AuthorizationError {
    /// The principal lacks the required permission.
    #[error("permission denied")]
    Denied,
    /// The policy backend failed internally.
    #[error("authorization backend unavailable")]
    BackendUnavailable,
}

const ACCOUNTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("service_accounts.v1");
const ACCESS_KEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("access_keys.v1");

/// Secret signing material with zeroization and a redacted debug representation.
pub struct SigningSecret(Zeroizing<Vec<u8>>);

impl SigningSecret {
    /// Copies secret bytes into zeroizing memory.
    #[must_use]
    pub fn new(value: impl AsRef<[u8]>) -> Self {
        Self(Zeroizing::new(value.as_ref().to_vec()))
    }

    /// Exposes signing bytes only to cryptographic code.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Debug for SigningSecret {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted signing secret>")
    }
}

/// S3 signing-credential lookup boundary.
#[async_trait]
pub trait SigningCredentialProvider: Send + Sync {
    /// Resolves active signing material without exposing persistence details.
    async fn signing_secret(
        &self,
        access_key_id: &str,
    ) -> Result<(Principal, SigningSecret), CredentialLookupError>;
}

/// Credential lookup failures intentionally safe for protocol mapping.
#[derive(Debug, Error)]
pub enum CredentialLookupError {
    /// The public access key is unknown.
    #[error("access key was not found")]
    UnknownAccessKey,
    /// The credential or account is inactive.
    #[error("credential is inactive")]
    Inactive,
    /// Durable credential state could not be read or decrypted.
    #[error("credential backend unavailable")]
    Backend,
}

/// Public service-account and credential descriptors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAccountInfo {
    /// Workload identity descriptor.
    pub account: ServiceAccount,
    /// Public credential descriptor.
    pub credential: Credential,
}

/// A newly issued service-account credential. The secret is returned exactly once.
pub struct IssuedServiceAccount {
    /// Persisted public descriptors.
    pub info: ServiceAccountInfo,
    /// Plaintext secret for the explicit issuance response only.
    pub secret: SigningSecret,
}

impl Debug for IssuedServiceAccount {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedServiceAccount")
            .field("info", &self.info)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct EncryptedCredentialRecord {
    info: ServiceAccountInfo,
    encryption_version: u32,
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

/// Persistent credential manager with authenticated encryption at rest.
#[derive(Clone)]
pub struct CredentialManager {
    database: Arc<Database>,
    root_access_key: Arc<str>,
    root_secret: Arc<SigningSecret>,
    encryption_key: Arc<Zeroizing<[u8; 32]>>,
}

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
            }
            write.commit().map_err(store_backend)?;
            Ok::<_, CredentialStoreError>(database)
        })
        .await??;
        Ok(Self {
            database: Arc::new(database),
            root_access_key: Arc::from(root_access_key),
            root_secret: Arc::new(SigningSecret::new(&root_secret_bytes)),
            encryption_key: Arc::new(Zeroizing::new(encryption_key)),
        })
    }

    /// Creates and durably stores one encrypted service-account credential.
    pub async fn create_service_account(
        &self,
        name: impl Into<String>,
        organization_id: OrganizationId,
    ) -> Result<IssuedServiceAccount, CredentialStoreError> {
        let name = name.into();
        validate_service_account_name(&name)?;
        let account = ServiceAccount {
            id: ServiceAccountId::new(),
            organization_id,
            name,
            disabled: false,
            created_at: Utc::now(),
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
            credential,
        };
        let record = encrypt_record(info.clone(), secret_text.as_bytes(), &self.encryption_key)?;
        let encoded = serde_json::to_vec(&record)?;
        let id_key = info.account.id.as_uuid().as_bytes().to_vec();
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
                result.push(record.info);
            }
            result.sort_by(|left, right| left.account.name.cmp(&right.account.name));
            Ok(result)
        })
        .await?
    }

    /// Disables an account and its current credential without deleting audit state.
    pub async fn revoke_service_account(
        &self,
        account_id: ServiceAccountId,
    ) -> Result<(), CredentialStoreError> {
        let database = Arc::clone(&self.database);
        let key = account_id.as_uuid().as_bytes().to_vec();
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

    /// Constant-time verification for native administrative authentication.
    #[must_use]
    pub fn verify_root(&self, access_key: &str, secret: &[u8]) -> bool {
        bool::from(access_key.as_bytes().ct_eq(self.root_access_key.as_bytes()))
            & bool::from(secret.ct_eq(self.root_secret.expose()))
    }

    fn lookup_service_secret(
        &self,
        access_key_id: String,
    ) -> Result<Option<(Principal, SigningSecret)>, CredentialStoreError> {
        let read = self.database.begin_read().map_err(store_backend)?;
        let keys = read.open_table(ACCESS_KEYS).map_err(store_backend)?;
        let Some(id) = keys
            .get(access_key_id.as_str())
            .map_err(store_backend)?
            .map(|value| value.value().to_vec())
        else {
            return Ok(None);
        };
        let accounts = read.open_table(ACCOUNTS).map_err(store_backend)?;
        let encoded = accounts
            .get(id.as_slice())
            .map_err(store_backend)?
            .map(|value| value.value().to_vec())
            .ok_or(CredentialStoreError::CorruptIndex)?;
        let record: EncryptedCredentialRecord = serde_json::from_slice(&encoded)?;
        if record.info.account.disabled
            || record.info.credential.disabled
            || record
                .info
                .credential
                .expires_at
                .is_some_and(|expiry| expiry <= Utc::now())
        {
            return Err(CredentialStoreError::CredentialInactive);
        }
        let secret = decrypt_record(&record, &self.encryption_key)?;
        Ok(Some((
            Principal::ServiceAccount {
                id: record.info.account.id,
                organization_id: record.info.account.organization_id,
            },
            SigningSecret::new(secret),
        )))
    }
}

#[async_trait]
impl SigningCredentialProvider for CredentialManager {
    async fn signing_secret(
        &self,
        access_key_id: &str,
    ) -> Result<(Principal, SigningSecret), CredentialLookupError> {
        if bool::from(
            access_key_id
                .as_bytes()
                .ct_eq(self.root_access_key.as_bytes()),
        ) {
            return Ok((
                Principal::System {
                    component: "root".into(),
                },
                SigningSecret::new(self.root_secret.expose()),
            ));
        }
        let manager = self.clone();
        let access_key_id = access_key_id.to_owned();
        tokio::task::spawn_blocking(move || manager.lookup_service_secret(access_key_id))
            .await
            .map_err(|_| CredentialLookupError::Backend)?
            .map_err(|error| match error {
                CredentialStoreError::CredentialInactive => CredentialLookupError::Inactive,
                _ => CredentialLookupError::Backend,
            })?
            .ok_or(CredentialLookupError::UnknownAccessKey)
    }
}

/// Durable credential-management failures for administrator-facing operations.
#[derive(Debug, Error)]
pub enum CredentialStoreError {
    /// Credential directory creation failed.
    #[error("failed to prepare credential directory: {0}")]
    Directory(#[source] std::io::Error),
    /// A generated access key collided and issuance can be retried.
    #[error("generated access key collided")]
    AccessKeyCollision,
    /// Requested service account is absent.
    #[error("service account was not found")]
    AccountNotFound,
    /// Credential is disabled or expired.
    #[error("credential is inactive")]
    CredentialInactive,
    /// Credential index points to missing state.
    #[error("credential index is inconsistent")]
    CorruptIndex,
    /// Invalid administrative input.
    #[error("invalid service account: {0}")]
    InvalidInput(String),
    /// Credential encoding failed.
    #[error("credential encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    /// Cryptographic encryption, decryption, or derivation failed.
    #[error("credential cryptography failed")]
    Cryptography,
    /// Embedded credential database failed.
    #[error("credential database failed: {0}")]
    Database(String),
    /// Blocking credential task failed.
    #[error("credential task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

fn derive_encryption_key(material: &[u8]) -> Result<[u8; 32], CredentialStoreError> {
    let derivation = hkdf::Hkdf::<Sha256>::new(Some(b"credential-store-v1"), material);
    let mut key = [0_u8; 32];
    derivation
        .expand(b"service-account-encryption-key", &mut key)
        .map_err(|_| CredentialStoreError::Cryptography)?;
    Ok(key)
}

fn random_secret_bytes() -> [u8; 48] {
    let mut bytes = [0_u8; 48];
    for (chunk, uuid) in
        bytes
            .chunks_exact_mut(16)
            .zip([Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()])
    {
        chunk.copy_from_slice(uuid.as_bytes());
    }
    bytes
}

fn encrypt_record(
    info: ServiceAccountInfo,
    secret: &[u8],
    key: &[u8; 32],
) -> Result<EncryptedCredentialRecord, CredentialStoreError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CredentialStoreError::Cryptography)?;
    let uuid = Uuid::new_v4();
    let mut nonce = [0_u8; 12];
    nonce.copy_from_slice(&uuid.as_bytes()[..12]);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: secret,
                aad: info.credential.key_id.as_bytes(),
            },
        )
        .map_err(|_| CredentialStoreError::Cryptography)?;
    Ok(EncryptedCredentialRecord {
        info,
        encryption_version: 1,
        nonce,
        ciphertext,
    })
}

fn decrypt_record(
    record: &EncryptedCredentialRecord,
    key: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, CredentialStoreError> {
    if record.encryption_version != 1 {
        return Err(CredentialStoreError::Cryptography);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CredentialStoreError::Cryptography)?;
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&record.nonce),
            Payload {
                msg: &record.ciphertext,
                aad: record.info.credential.key_id.as_bytes(),
            },
        )
        .map_err(|_| CredentialStoreError::Cryptography)?;
    Ok(Zeroizing::new(plaintext))
}

fn validate_service_account_name(name: &str) -> Result<(), CredentialStoreError> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(CredentialStoreError::InvalidInput(
            "name must contain 1 to 128 non-control characters".into(),
        ));
    }
    Ok(())
}

fn store_backend(error: impl std::fmt::Display) -> CredentialStoreError {
    CredentialStoreError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn credential_debug_output_contains_no_secret_field() {
        let credential = Credential {
            id: Uuid::new_v4(),
            service_account_id: ServiceAccountId::new(),
            key_id: "oes_test_public".into(),
            disabled: false,
            created_at: Utc::now(),
            expires_at: None,
        };
        let debug = format!("{credential:?}");
        assert!(debug.contains("oes_test_public"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("hash"));
    }

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
}
