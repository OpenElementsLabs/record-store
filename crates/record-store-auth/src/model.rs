use std::fmt::Debug;

use chrono::{DateTime, Utc};
use record_store_core::{OrganizationId, ServiceAccountId};
use serde::{Deserialize, Serialize};
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use record_store_core::ServiceAccountId;
    use uuid::Uuid;

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
}
