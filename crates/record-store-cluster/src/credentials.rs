//! Node credentials and join tokens.
//!
//! Cluster membership uses its own credential namespace. S3 access keys and
//! management tokens are never accepted for internal RPC, and revoking one node
//! never requires rotating another.

use std::fmt::{self, Debug, Formatter};

use chrono::{DateTime, TimeDelta, Utc};
use record_store_core::{JoinTokenId, NodeCredentialId, NodeId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;

/// Prefix identifying a node credential.
const NODE_CREDENTIAL_PREFIX: &str = "recordstorenode";
/// Prefix identifying a cluster join token.
const JOIN_TOKEN_PREFIX: &str = "recordstorejoin";

/// Failures raised while issuing or verifying cluster credentials.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CredentialError {
    /// The presented value was not a well-formed cluster credential.
    #[error("malformed cluster credential")]
    Malformed,
    /// No credential matched.
    #[error("cluster credential was not recognized")]
    Unknown,
    /// The credential exists but may not be used.
    #[error("cluster credential is disabled")]
    Disabled,
    /// The join token is past its expiry.
    #[error("join token has expired")]
    Expired,
    /// The join token has already been used the permitted number of times.
    #[error("join token has already been used")]
    Exhausted,
    /// The join token was explicitly revoked.
    #[error("join token was revoked")]
    Revoked,
}

/// A secret shown to an operator exactly once.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClusterSecret(String);

impl ClusterSecret {
    /// Wraps an already generated secret.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    /// Exposes the secret to code that must transmit or hash it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Debug for ClusterSecret {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Generates operating-system randomness in a URL-safe hexadecimal form.
fn random_secret() -> String {
    let first = Uuid::new_v4().simple().to_string();
    let second = Uuid::new_v4().simple().to_string();
    format!("{first}{second}")
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

/// The replicated record for one node's internal RPC credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCredential {
    /// Stable credential identifier.
    pub id: NodeCredentialId,
    /// Node this credential authenticates.
    pub node_id: NodeId,
    /// SHA-256 digest of the presented secret.
    pub secret_digest: [u8; 32],
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Time the credential was last rotated.
    pub rotated_at: Option<DateTime<Utc>>,
    /// Whether the credential may currently be used.
    pub disabled: bool,
}

/// A newly issued node credential, including the secret shown once.
#[derive(Debug, Clone)]
pub struct IssuedNodeCredential {
    /// The replicated record to persist.
    pub record: NodeCredential,
    /// The full credential string the node must present.
    pub secret: ClusterSecret,
}

impl NodeCredential {
    /// Issues a credential for a node.
    #[must_use]
    pub fn issue(node_id: NodeId, now: DateTime<Utc>) -> IssuedNodeCredential {
        let id = NodeCredentialId::new();
        let raw = format!(
            "{NODE_CREDENTIAL_PREFIX}.{}.{}",
            id.as_uuid().simple(),
            random_secret()
        );
        IssuedNodeCredential {
            record: Self {
                id,
                node_id,
                secret_digest: digest(&raw),
                created_at: now,
                rotated_at: None,
                disabled: false,
            },
            secret: ClusterSecret::new(raw),
        }
    }

    /// Rotates the credential, returning the replacement secret.
    #[must_use]
    pub fn rotate(&self, now: DateTime<Utc>) -> IssuedNodeCredential {
        let raw = format!(
            "{NODE_CREDENTIAL_PREFIX}.{}.{}",
            self.id.as_uuid().simple(),
            random_secret()
        );
        IssuedNodeCredential {
            record: Self {
                id: self.id,
                node_id: self.node_id,
                secret_digest: digest(&raw),
                created_at: self.created_at,
                rotated_at: Some(now),
                disabled: false,
            },
            secret: ClusterSecret::new(raw),
        }
    }

    /// Verifies a presented credential in constant time.
    pub fn verify(&self, presented: &str) -> Result<(), CredentialError> {
        if self.disabled {
            return Err(CredentialError::Disabled);
        }
        let candidate = digest(presented);
        if bool::from(self.secret_digest.ct_eq(&candidate)) {
            Ok(())
        } else {
            Err(CredentialError::Unknown)
        }
    }
}

/// Parses the credential identifier out of a presented node credential.
///
/// The identifier is public: it only selects which stored digest to compare
/// against, and the comparison itself is constant time.
pub fn parse_node_credential(presented: &str) -> Result<NodeCredentialId, CredentialError> {
    parse_prefixed(presented, NODE_CREDENTIAL_PREFIX).map(NodeCredentialId::from_uuid)
}

/// A replicated join token.
///
/// Join tokens are short-lived, revocable, and single-use by default. The
/// cluster root secret is never usable as a join token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinToken {
    /// Stable token identifier.
    pub id: JoinTokenId,
    /// SHA-256 digest of the presented token.
    pub token_digest: [u8; 32],
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Expiry time.
    pub expires_at: DateTime<Utc>,
    /// Maximum number of successful joins.
    pub maximum_uses: u32,
    /// Successful joins so far.
    pub uses: u32,
    /// Whether the token was explicitly revoked.
    pub revoked: bool,
    /// Operator note recorded with the token.
    pub description: String,
}

/// A newly issued join token, including the secret shown once.
#[derive(Debug, Clone)]
pub struct IssuedJoinToken {
    /// The replicated record to persist.
    pub record: JoinToken,
    /// The token an operator passes to `record-store node join`.
    pub token: ClusterSecret,
}

impl JoinToken {
    /// Longest permitted join-token lifetime.
    pub const MAXIMUM_LIFETIME_SECONDS: u64 = 24 * 60 * 60;
    /// Shortest permitted join-token lifetime.
    pub const MINIMUM_LIFETIME_SECONDS: u64 = 60;

    /// Issues a join token.
    #[must_use]
    pub fn issue(
        lifetime_seconds: u64,
        maximum_uses: u32,
        description: String,
        now: DateTime<Utc>,
    ) -> IssuedJoinToken {
        let id = JoinTokenId::new();
        let raw = format!(
            "{JOIN_TOKEN_PREFIX}.{}.{}",
            id.as_uuid().simple(),
            random_secret()
        );
        let lifetime = lifetime_seconds.clamp(
            Self::MINIMUM_LIFETIME_SECONDS,
            Self::MAXIMUM_LIFETIME_SECONDS,
        );
        let expires_at = now
            + TimeDelta::try_seconds(i64::try_from(lifetime).unwrap_or(3_600))
                .unwrap_or_else(TimeDelta::zero);
        IssuedJoinToken {
            record: Self {
                id,
                token_digest: digest(&raw),
                created_at: now,
                expires_at,
                maximum_uses: maximum_uses.max(1),
                uses: 0,
                revoked: false,
                description,
            },
            token: ClusterSecret::new(raw),
        }
    }

    /// Verifies a presented token without consuming it.
    pub fn verify(&self, presented: &str, now: DateTime<Utc>) -> Result<(), CredentialError> {
        if self.revoked {
            return Err(CredentialError::Revoked);
        }
        if now >= self.expires_at {
            return Err(CredentialError::Expired);
        }
        if self.uses >= self.maximum_uses {
            return Err(CredentialError::Exhausted);
        }
        let candidate = digest(presented);
        if bool::from(self.token_digest.ct_eq(&candidate)) {
            Ok(())
        } else {
            Err(CredentialError::Unknown)
        }
    }

    /// Records a successful use.
    pub fn consume(&mut self) {
        self.uses = self.uses.saturating_add(1);
    }

    /// Returns whether the token can never be used again.
    #[must_use]
    pub fn spent(&self, now: DateTime<Utc>) -> bool {
        self.revoked || self.uses >= self.maximum_uses || now >= self.expires_at
    }
}

/// Parses the token identifier out of a presented join token.
pub fn parse_join_token(presented: &str) -> Result<JoinTokenId, CredentialError> {
    parse_prefixed(presented, JOIN_TOKEN_PREFIX).map(JoinTokenId::from_uuid)
}

fn parse_prefixed(presented: &str, expected_prefix: &str) -> Result<Uuid, CredentialError> {
    if presented.len() > 256 {
        return Err(CredentialError::Malformed);
    }
    let mut parts = presented.split('.');
    let prefix = parts.next().ok_or(CredentialError::Malformed)?;
    let encoded_id = parts.next().ok_or(CredentialError::Malformed)?;
    let secret = parts.next().ok_or(CredentialError::Malformed)?;
    if prefix != expected_prefix || parts.next().is_some() || secret.len() < 32 {
        return Err(CredentialError::Malformed);
    }
    Uuid::parse_str(encoded_id).map_err(|_| CredentialError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_credentials_verify_only_their_own_secret() {
        let now = Utc::now();
        let issued = NodeCredential::issue(NodeId::new(), now);
        let other = NodeCredential::issue(NodeId::new(), now);
        assert!(issued.record.verify(issued.secret.expose()).is_ok());
        assert!(matches!(
            issued.record.verify(other.secret.expose()),
            Err(CredentialError::Unknown)
        ));
    }

    #[test]
    fn rotation_invalidates_the_previous_secret_without_touching_others() {
        let now = Utc::now();
        let issued = NodeCredential::issue(NodeId::new(), now);
        let unaffected = NodeCredential::issue(NodeId::new(), now);
        let rotated = issued.record.rotate(now);
        assert!(rotated.record.verify(rotated.secret.expose()).is_ok());
        assert!(rotated.record.verify(issued.secret.expose()).is_err());
        assert!(
            unaffected.record.verify(unaffected.secret.expose()).is_ok(),
            "rotating one node must not affect another"
        );
        assert_eq!(rotated.record.id, issued.record.id);
    }

    #[test]
    fn disabled_credentials_are_refused() {
        let now = Utc::now();
        let mut issued = NodeCredential::issue(NodeId::new(), now);
        issued.record.disabled = true;
        assert!(matches!(
            issued.record.verify(issued.secret.expose()),
            Err(CredentialError::Disabled)
        ));
    }

    #[test]
    fn join_tokens_expire_and_are_single_use_by_default() {
        let now = Utc::now();
        let issued = JoinToken::issue(120, 1, "compose".into(), now);
        let mut record = issued.record.clone();
        record
            .verify(issued.token.expose(), now)
            .expect("token must verify while fresh");
        record.consume();
        assert!(matches!(
            record.verify(issued.token.expose(), now),
            Err(CredentialError::Exhausted)
        ));
        assert!(record.spent(now));

        let mut fresh = JoinToken::issue(120, 1, String::new(), now).record;
        assert!(matches!(
            fresh.verify("wrong", now + TimeDelta::try_seconds(3_600).expect("delta")),
            Err(CredentialError::Expired)
        ));
        fresh.revoked = true;
        assert!(matches!(
            fresh.verify("wrong", now),
            Err(CredentialError::Revoked)
        ));
    }

    #[test]
    fn credential_parsing_rejects_foreign_and_malformed_values() {
        let issued = NodeCredential::issue(NodeId::new(), Utc::now());
        assert_eq!(
            parse_node_credential(issued.secret.expose()).expect("parse"),
            issued.record.id
        );
        for value in [
            "",
            "recordstorenode",
            "recordstorenode.not-a-uuid.0123456789012345678901234567890123456789",
            "recordstorejoin.11111111111111111111111111111111.0123456789012345678901234567890123456789",
            "recordstorenode.11111111111111111111111111111111.short",
        ] {
            assert!(
                parse_node_credential(value).is_err(),
                "accepted malformed credential {value}"
            );
        }
        let token = JoinToken::issue(60, 1, String::new(), Utc::now());
        assert_eq!(
            parse_join_token(token.token.expose()).expect("parse"),
            token.record.id
        );
        assert!(parse_join_token(issued.secret.expose()).is_err());
    }
}
