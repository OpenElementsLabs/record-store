use serde::{Deserialize, Serialize};

use crate::*;

/// Validated positive lifecycle age in whole days.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct ExpirationDays(u32);

impl ExpirationDays {
    pub const MAX: u32 = 36_500;

    pub fn new(days: u32) -> Result<Self, CoreError> {
        if (1..=Self::MAX).contains(&days) {
            Ok(Self(days))
        } else {
            Err(CoreError::InvalidLifecycleRule(
                "expiration days must be between 1 and 36500".into(),
            ))
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for ExpirationDays {
    type Error = CoreError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ExpirationDays> for u32 {
    fn from(value: ExpirationDays) -> Self {
        value.0
    }
}

/// Bucket versioning state. This intentionally is not represented by booleans.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersioningState {
    /// No historical versions or delete markers are retained.
    #[default]
    Disabled,
    /// New writes and deletes create immutable versions.
    Enabled,
    /// Existing history remains, while new writes use replace semantics.
    Suspended,
}

/// A byte quota with an explicit unlimited state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "bytes", rename_all = "snake_case")]
pub enum ByteQuota {
    /// No configured byte limit.
    #[default]
    Unlimited,
    /// Maximum committed logical bytes.
    Limit(u64),
}

impl ByteQuota {
    /// Returns whether a proposed value is allowed.
    #[must_use]
    pub const fn allows(self, proposed: u64) -> bool {
        match self {
            Self::Unlimited => true,
            Self::Limit(limit) => proposed <= limit,
        }
    }
}

/// An object-count quota with an explicit unlimited state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "objects", rename_all = "snake_case")]
pub enum ObjectCountQuota {
    /// No configured object limit.
    #[default]
    Unlimited,
    /// Maximum currently visible object count.
    Limit(u64),
}

impl ObjectCountQuota {
    /// Returns whether a proposed value is allowed.
    #[must_use]
    pub const fn allows(self, proposed: u64) -> bool {
        match self {
            Self::Unlimited => true,
            Self::Limit(limit) => proposed <= limit,
        }
    }
}

/// Per-bucket storage limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketQuota {
    /// Maximum current logical bytes.
    pub bytes: ByteQuota,
    /// Maximum current visible objects.
    pub objects: ObjectCountQuota,
}
