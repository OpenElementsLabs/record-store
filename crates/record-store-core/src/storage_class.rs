//! Storage-class labels.
//!
//! A storage class is an operator-defined name. It lives here rather than in the
//! cluster crate because both sides of the relationship need it: a device
//! carries the class it belongs to, and a bucket selects the class its objects
//! should be placed on.
//!
//! The label carries no meaning by itself. What a class *means* — which devices,
//! how many copies, what separation — is a cluster storage policy.

use std::{
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// A validated storage-class label such as `standard`, `hot`, or `archive`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StorageClass(String);

impl StorageClass {
    /// Maximum label length.
    pub const MAX_LENGTH: usize = 32;
    /// Class assigned when an operator does not choose one.
    pub const DEFAULT: &'static str = "standard";

    /// Validates and creates a storage class.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        if value.is_empty() || value.len() > Self::MAX_LENGTH {
            return Err(CoreError::InvalidStorageClass(
                "class must contain between 1 and 32 bytes".into(),
            ));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(CoreError::InvalidStorageClass(
                "class may only contain lowercase letters, digits, and hyphens".into(),
            ));
        }
        Ok(Self(value))
    }

    /// Returns the validated label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for StorageClass {
    fn default() -> Self {
        Self(Self::DEFAULT.to_owned())
    }
}

impl Display for StorageClass {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for StorageClass {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for StorageClass {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<StorageClass> for String {
    fn from(value: StorageClass) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_bounded_and_restricted_to_a_safe_alphabet() {
        assert_eq!(StorageClass::default().as_str(), "standard");
        StorageClass::new("hot-2").expect("valid");
        for invalid in ["", "Hot", "with space", "under_score", &"a".repeat(33)] {
            assert!(
                StorageClass::new(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }
}
