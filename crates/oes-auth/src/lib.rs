//! Authentication and authorization contracts.
//!
//! This crate intentionally provides boundaries and safe persisted descriptors,
//! not an IAM implementation. Secret verifiers belong in credential backends and
//! must never be represented by [`Credential`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oes_core::{BucketId, OrganizationId, ServiceAccountId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

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

#[cfg(test)]
mod tests {
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
}
