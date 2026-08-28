use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::*;

/// Restart-safe metadata-driven object expiration rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRule {
    pub id: LifecycleRuleId,
    pub bucket_id: BucketId,
    pub prefix: String,
    pub enabled: bool,
    pub expiration: Option<ExpirationDays>,
    pub noncurrent_version_expiration: Option<ExpirationDays>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl LifecycleRule {
    /// Rejects no-op rules and unreasonably large prefixes.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.prefix.len() > ObjectKey::MAX_LENGTH {
            return Err(CoreError::InvalidLifecycleRule(
                "prefix exceeds the maximum object-key length".into(),
            ));
        }
        if self.expiration.is_none() && self.noncurrent_version_expiration.is_none() {
            return Err(CoreError::InvalidLifecycleRule(
                "at least one expiration action is required".into(),
            ));
        }
        Ok(())
    }
}

/// Foundation for future Object Lock enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionMode {
    /// Deletion may be bypassed by an appropriately authorized administrator.
    Governance,
    /// Deletion is forbidden until the retention time has elapsed.
    Compliance,
}

/// Retention metadata stored independently from payload layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRetention {
    /// Optional future retention mode.
    pub mode: Option<RetentionMode>,
    /// Optional time before which deletion is not allowed.
    pub retain_until: Option<DateTime<Utc>>,
    /// Independent legal-hold flag.
    pub legal_hold: bool,
}

/// Aggregate storage accounting values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageUsage {
    /// Number of committed objects.
    pub object_count: u64,
    /// Total committed payload bytes.
    pub bytes_used: u64,
    /// Number of buckets.
    pub bucket_count: u64,
    /// Number of immutable data versions, including current versions.
    #[serde(default)]
    pub version_count: u64,
    /// Bytes referenced by all immutable object versions.
    #[serde(default)]
    pub version_bytes: u64,
    /// Bytes occupied by committed immutable payloads.
    #[serde(default)]
    pub physical_bytes: u64,
    /// Bytes occupied by durable multipart parts.
    #[serde(default)]
    pub temporary_multipart_bytes: u64,
}
