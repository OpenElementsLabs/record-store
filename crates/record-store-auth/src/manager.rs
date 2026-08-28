use std::{
    fmt::{self, Debug, Formatter},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use redb::Database;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::crypto::decrypt_record;
use crate::evaluate::evaluate_policies;
use crate::keys::store_backend;
use crate::schema::{ACCESS_KEYS, ACCOUNTS, ROTATED_CREDENTIALS};
use crate::*;

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
pub(crate) struct EncryptedCredentialRecord {
    pub(crate) info: ServiceAccountInfo,
    pub(crate) encryption_version: u32,
    pub(crate) nonce: [u8; 12],
    pub(crate) ciphertext: Vec<u8>,
}

/// Persistent credential manager with authenticated encryption at rest.
#[derive(Clone)]
pub struct CredentialManager {
    pub(crate) database: Arc<Database>,
    pub(crate) root_access_key: Arc<str>,
    pub(crate) root_secret: Arc<SigningSecret>,
    pub(crate) encryption_key: Arc<Zeroizing<[u8; 32]>>,
    pub(crate) explicit_master_key: bool,
}

impl CredentialManager {
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
