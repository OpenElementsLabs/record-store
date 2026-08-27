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
use record_store_core::{OrganizationId, ServiceAccountId};
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
        /// Credential used for this authentication.
        #[serde(default)]
        credential_id: Option<Uuid>,
    },
    /// A trusted internal Record Store component.
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
    /// Operator-facing purpose.
    #[serde(default)]
    pub description: String,
    /// Whether authentication is currently disabled.
    pub disabled: bool,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last administrative update time.
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
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

/// Authorization actions with stable S3-aligned names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    #[serde(rename = "s3:ListBucket")]
    ListBucket,
    #[serde(rename = "s3:GetObject")]
    GetObject,
    #[serde(rename = "s3:PutObject")]
    PutObject,
    #[serde(rename = "s3:DeleteObject")]
    DeleteObject,
    #[serde(rename = "s3:GetObjectVersion")]
    GetObjectVersion,
    #[serde(rename = "s3:DeleteObjectVersion")]
    DeleteObjectVersion,
    #[serde(rename = "s3:ManageBucket")]
    ManageBucket,
}

/// Requested action and canonical resource such as `bucket:name/prefix`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Permission {
    pub action: Action,
    pub resource: String,
}

/// Policy effect. Explicit deny takes precedence over allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyEffect {
    Allow,
    Deny,
}

/// One policy statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyStatement {
    pub effect: PolicyEffect,
    pub actions: Vec<Action>,
    pub resources: Vec<String>,
}

/// Durable authorization policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub statements: Vec<PolicyStatement>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
const ROTATED_CREDENTIALS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("rotated_credentials.v1");
const ACCOUNT_CREDENTIALS: TableDefinition<&[u8], u8> =
    TableDefinition::new("account_credentials.v1");
const POLICIES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("policies.v1");
const POLICY_NAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("policy_names.v1");
const POLICY_BINDINGS: TableDefinition<&[u8], u8> = TableDefinition::new("policy_bindings.v1");

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
    /// All credentials, including overlap during rotation.
    #[serde(default)]
    pub credentials: Vec<Credential>,
    /// Attached policy identifiers.
    #[serde(default)]
    pub policy_bindings: Vec<Uuid>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    explicit_master_key: bool,
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

    /// Creates a validated durable authorization policy.
    pub async fn create_policy(
        &self,
        name: impl Into<String>,
        description: impl Into<String>,
        statements: Vec<PolicyStatement>,
    ) -> Result<Policy, CredentialStoreError> {
        let name = name.into();
        let description = description.into();
        validate_service_account_name(&name)?;
        validate_description(&description)?;
        validate_policy_statements(&statements)?;
        let now = Utc::now();
        let policy = Policy {
            id: Uuid::new_v4(),
            name,
            description,
            statements,
            created_at: now,
            updated_at: now,
        };
        let bytes = serde_json::to_vec(&policy)?;
        let database = Arc::clone(&self.database);
        let saved = policy.clone();
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let names = write.open_table(POLICY_NAMES).map_err(store_backend)?;
                if names
                    .get(saved.name.as_str())
                    .map_err(store_backend)?
                    .is_some()
                {
                    return Err(CredentialStoreError::PolicyAlreadyExists);
                }
            }
            {
                let mut policies = write.open_table(POLICIES).map_err(store_backend)?;
                policies
                    .insert(saved.id.as_bytes().as_slice(), bytes.as_slice())
                    .map_err(store_backend)?;
            }
            {
                let mut names = write.open_table(POLICY_NAMES).map_err(store_backend)?;
                names
                    .insert(saved.name.as_str(), saved.id.as_bytes().as_slice())
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await??;
        Ok(policy)
    }

    /// Lists public policies in deterministic name order.
    pub async fn list_policies(&self) -> Result<Vec<Policy>, CredentialStoreError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database.begin_read().map_err(store_backend)?;
            let names = read.open_table(POLICY_NAMES).map_err(store_backend)?;
            let policies = read.open_table(POLICIES).map_err(store_backend)?;
            let mut out = Vec::new();
            for entry in names.iter().map_err(store_backend)? {
                let (_, id) = entry.map_err(store_backend)?;
                let bytes = policies
                    .get(id.value())
                    .map_err(store_backend)?
                    .map(|value| value.value().to_vec())
                    .ok_or(CredentialStoreError::CorruptIndex)?;
                out.push(serde_json::from_slice(&bytes)?);
            }
            Ok(out)
        })
        .await?
    }

    /// Attaches a policy to a service account.
    pub async fn attach_policy(
        &self,
        account_id: ServiceAccountId,
        policy_id: Uuid,
    ) -> Result<(), CredentialStoreError> {
        self.get_service_account(account_id).await?;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let policies = write.open_table(POLICIES).map_err(store_backend)?;
                if policies
                    .get(policy_id.as_bytes().as_slice())
                    .map_err(store_backend)?
                    .is_none()
                {
                    return Err(CredentialStoreError::PolicyNotFound);
                }
            }
            {
                let mut bindings = write.open_table(POLICY_BINDINGS).map_err(store_backend)?;
                bindings
                    .insert(policy_binding_key(account_id, policy_id).as_slice(), &1)
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await?
    }

    /// Detaches a policy from a service account.
    pub async fn detach_policy(
        &self,
        account_id: ServiceAccountId,
        policy_id: Uuid,
    ) -> Result<(), CredentialStoreError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let mut bindings = write.open_table(POLICY_BINDINGS).map_err(store_backend)?;
                bindings
                    .remove(policy_binding_key(account_id, policy_id).as_slice())
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
        if id.len() != 16 && id.len() != 32 {
            return Err(CredentialStoreError::CorruptIndex);
        }
        let accounts = read.open_table(ACCOUNTS).map_err(store_backend)?;
        let account_encoded = accounts
            .get(&id[..16])
            .map_err(store_backend)?
            .map(|value| value.value().to_vec())
            .ok_or(CredentialStoreError::CorruptIndex)?;
        let account_record: EncryptedCredentialRecord = serde_json::from_slice(&account_encoded)?;
        let record = if id.len() == 16 {
            account_record.clone()
        } else {
            let credentials = read
                .open_table(ROTATED_CREDENTIALS)
                .map_err(store_backend)?;
            let encoded = credentials
                .get(&id[16..])
                .map_err(store_backend)?
                .map(|value| value.value().to_vec())
                .ok_or(CredentialStoreError::CorruptIndex)?;
            serde_json::from_slice(&encoded)?
        };
        if account_record.info.account.disabled
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
                credential_id: Some(record.info.credential.id),
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

#[async_trait]
impl Authorizer for CredentialManager {
    async fn authorize(&self, context: AuthorizationContext<'_>) -> Result<(), AuthorizationError> {
        let Principal::ServiceAccount { id, .. } = context.principal else {
            return if matches!(context.principal, Principal::System { .. }) {
                Ok(())
            } else {
                Err(AuthorizationError::Denied)
            };
        };
        let database = Arc::clone(&self.database);
        let account_id = *id;
        let permission = context.permission.clone();
        tokio::task::spawn_blocking(move || evaluate_policies(&database, account_id, &permission))
            .await
            .map_err(|_| AuthorizationError::BackendUnavailable)?
    }
}

/// Durable credential-management failures for administrator-facing operations.
#[derive(Debug, Error)]
pub enum CredentialStoreError {
    /// Credential directory creation failed.
    #[error("failed to prepare credential directory: {0}")]
    Directory(#[source] std::io::Error),
    /// Encrypted durable credentials require an explicitly supplied stable key.
    #[error("an explicit credential master key is required")]
    MasterKeyRequired,
    /// A generated access key collided and issuance can be retried.
    #[error("generated access key collided")]
    AccessKeyCollision,
    /// Requested service account is absent.
    #[error("service account was not found")]
    AccountNotFound,
    /// Requested credential is absent.
    #[error("credential was not found")]
    CredentialNotFound,
    /// Policy name already exists.
    #[error("policy already exists")]
    PolicyAlreadyExists,
    /// Requested policy is absent.
    #[error("policy was not found")]
    PolicyNotFound,
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

fn validate_description(description: &str) -> Result<(), CredentialStoreError> {
    if description.len() > 1_024 || description.chars().any(char::is_control) {
        return Err(CredentialStoreError::InvalidInput(
            "description must not exceed 1024 non-control characters".into(),
        ));
    }
    Ok(())
}

fn validate_policy_statements(statements: &[PolicyStatement]) -> Result<(), CredentialStoreError> {
    if statements.is_empty() || statements.len() > 128 {
        return Err(CredentialStoreError::InvalidInput(
            "policy must contain between 1 and 128 statements".into(),
        ));
    }
    for statement in statements {
        if statement.actions.is_empty() || statement.resources.is_empty() {
            return Err(CredentialStoreError::InvalidInput(
                "policy statements require actions and resources".into(),
            ));
        }
        for resource in &statement.resources {
            let wildcard_count = resource.bytes().filter(|byte| *byte == b'*').count();
            if !resource.starts_with("bucket:")
                || wildcard_count > 1
                || (wildcard_count == 1 && !resource.ends_with('*'))
                || resource.chars().any(char::is_control)
            {
                return Err(CredentialStoreError::InvalidInput(
                    "policy resources must use bucket:... with an optional final wildcard".into(),
                ));
            }
        }
    }
    Ok(())
}

fn enrich_info(
    read: &redb::ReadTransaction,
    mut info: ServiceAccountInfo,
) -> Result<ServiceAccountInfo, CredentialStoreError> {
    info.credentials = vec![info.credential.clone()];
    let rotated = read
        .open_table(ROTATED_CREDENTIALS)
        .map_err(store_backend)?;
    for entry in rotated.iter().map_err(store_backend)? {
        let (_, value) = entry.map_err(store_backend)?;
        let record: EncryptedCredentialRecord = serde_json::from_slice(value.value())?;
        if record.info.credential.service_account_id == info.account.id {
            info.credentials.push(record.info.credential);
        }
    }
    info.credentials
        .sort_by_key(|credential| credential.created_at);
    info.policy_bindings.clear();
    let bindings = read.open_table(POLICY_BINDINGS).map_err(store_backend)?;
    let prefix = info.account.id.as_uuid().as_bytes().as_slice().to_vec();
    let end = prefix_successor(&prefix);
    for entry in bindings
        .range(prefix.as_slice()..end.as_slice())
        .map_err(store_backend)?
    {
        let (key, _) = entry.map_err(store_backend)?;
        let raw = key.value();
        if raw.len() == 32 {
            let id: [u8; 16] = raw[16..]
                .try_into()
                .map_err(|_| CredentialStoreError::CorruptIndex)?;
            info.policy_bindings.push(Uuid::from_bytes(id));
        }
    }
    Ok(info)
}

fn evaluate_policies(
    database: &Database,
    account_id: ServiceAccountId,
    permission: &Permission,
) -> Result<(), AuthorizationError> {
    let read = database
        .begin_read()
        .map_err(|_| AuthorizationError::BackendUnavailable)?;
    let bindings = read
        .open_table(POLICY_BINDINGS)
        .map_err(|_| AuthorizationError::BackendUnavailable)?;
    let policies = read
        .open_table(POLICIES)
        .map_err(|_| AuthorizationError::BackendUnavailable)?;
    let prefix = account_id.as_uuid().as_bytes().as_slice().to_vec();
    let end = prefix_successor(&prefix);
    let mut allowed = false;
    for entry in bindings
        .range(prefix.as_slice()..end.as_slice())
        .map_err(|_| AuthorizationError::BackendUnavailable)?
    {
        let (key, _) = entry.map_err(|_| AuthorizationError::BackendUnavailable)?;
        if key.value().len() != 32 {
            return Err(AuthorizationError::BackendUnavailable);
        }
        let encoded = policies
            .get(&key.value()[16..])
            .map_err(|_| AuthorizationError::BackendUnavailable)?
            .map(|value| value.value().to_vec())
            .ok_or(AuthorizationError::BackendUnavailable)?;
        let policy: Policy =
            serde_json::from_slice(&encoded).map_err(|_| AuthorizationError::BackendUnavailable)?;
        for statement in policy.statements {
            if statement.actions.contains(&permission.action)
                && statement
                    .resources
                    .iter()
                    .any(|resource| resource_matches(resource, &permission.resource))
            {
                if statement.effect == PolicyEffect::Deny {
                    return Err(AuthorizationError::Denied);
                }
                allowed = true;
            }
        }
    }
    if allowed {
        Ok(())
    } else {
        Err(AuthorizationError::Denied)
    }
}

fn resource_matches(pattern: &str, resource: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == resource, |prefix| resource.starts_with(prefix))
}

fn account_credential_key(account: ServiceAccountId, credential: Uuid) -> Vec<u8> {
    let mut key = account.as_uuid().as_bytes().as_slice().to_vec();
    key.extend_from_slice(credential.as_bytes());
    key
}

fn policy_binding_key(account: ServiceAccountId, policy: Uuid) -> Vec<u8> {
    let mut key = account.as_uuid().as_bytes().as_slice().to_vec();
    key.extend_from_slice(policy.as_bytes());
    key
}

fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut successor = prefix.to_vec();
    for index in (0..successor.len()).rev() {
        if successor[index] != u8::MAX {
            successor[index] += 1;
            successor.truncate(index + 1);
            return successor;
        }
    }
    successor.push(u8::MAX);
    successor
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
            key_id: "record_store_test_public".into(),
            disabled: false,
            created_at: Utc::now(),
            expires_at: None,
        };
        let debug = format!("{credential:?}");
        assert!(debug.contains("record_store_test_public"));
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

    #[tokio::test]
    async fn policies_default_deny_scope_prefixes_and_prioritize_explicit_deny() {
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
            .create_service_account("customer-app", OrganizationId::new())
            .await
            .expect("account");
        let principal = Principal::ServiceAccount {
            id: issued.info.account.id,
            organization_id: issued.info.account.organization_id,
            credential_id: Some(issued.info.credential.id),
        };
        let read = Permission {
            action: Action::GetObject,
            resource: "bucket:customers/customer-123/report.pdf".into(),
        };
        assert!(matches!(
            manager
                .authorize(AuthorizationContext {
                    principal: &principal,
                    permission: &read,
                })
                .await,
            Err(AuthorizationError::Denied)
        ));
        let policy = manager
            .create_policy(
                "customer-read",
                "prefix allow with a narrower deny",
                vec![
                    PolicyStatement {
                        effect: PolicyEffect::Allow,
                        actions: vec![Action::GetObject],
                        resources: vec!["bucket:customers/customer-123/*".into()],
                    },
                    PolicyStatement {
                        effect: PolicyEffect::Deny,
                        actions: vec![Action::GetObject],
                        resources: vec!["bucket:customers/customer-123/private/*".into()],
                    },
                ],
            )
            .await
            .expect("policy");
        manager
            .attach_policy(issued.info.account.id, policy.id)
            .await
            .expect("binding");
        assert!(
            manager
                .authorize(AuthorizationContext {
                    principal: &principal,
                    permission: &read,
                })
                .await
                .is_ok()
        );
        let denied = Permission {
            action: Action::GetObject,
            resource: "bucket:customers/customer-123/private/secret.pdf".into(),
        };
        assert!(matches!(
            manager
                .authorize(AuthorizationContext {
                    principal: &principal,
                    permission: &denied,
                })
                .await,
            Err(AuthorizationError::Denied)
        ));
        let escaped = Permission {
            action: Action::GetObject,
            resource: "bucket:customers/customer-124/report.pdf".into(),
        };
        assert!(matches!(
            manager
                .authorize(AuthorizationContext {
                    principal: &principal,
                    permission: &escaped,
                })
                .await,
            Err(AuthorizationError::Denied)
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
