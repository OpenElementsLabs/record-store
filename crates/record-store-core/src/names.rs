use std::{
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::*;

/// A validated S3-compatible bucket name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BucketName(String);

impl BucketName {
    /// Minimum bucket-name length.
    pub const MIN_LENGTH: usize = 3;
    /// Maximum bucket-name length.
    pub const MAX_LENGTH: usize = 63;

    /// Validates and creates a bucket name.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        validate_bucket_name(&value)?;
        Ok(Self(value))
    }

    /// Returns the validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for BucketName {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for BucketName {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for BucketName {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<BucketName> for String {
    fn from(value: BucketName) -> Self {
        value.0
    }
}

pub(crate) fn validate_bucket_name(value: &str) -> Result<(), CoreError> {
    let invalid = |reason: &str| CoreError::InvalidBucketName(reason.to_owned());
    if !(BucketName::MIN_LENGTH..=BucketName::MAX_LENGTH).contains(&value.len()) {
        return Err(invalid("name must contain between 3 and 63 bytes"));
    }
    let bytes = value.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(invalid("name must begin and end with a letter or digit"));
    }
    if !bytes.iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
    }) {
        return Err(invalid(
            "only lowercase letters, digits, hyphens, and periods are permitted",
        ));
    }
    if value.contains("..") {
        return Err(invalid("adjacent periods are not permitted"));
    }
    if value.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err(invalid("IP address notation is not permitted"));
    }
    if value.starts_with("xn--")
        || value.starts_with("sthree-")
        || value.ends_with("-s3alias")
        || value.ends_with("--ol-s3")
        || matches!(value, "record-store-system" | "record-store-internal")
    {
        return Err(invalid(
            "name uses a reserved prefix, suffix, or internal name",
        ));
    }
    Ok(())
}

/// A validated logical key. Keys are never interpreted as filesystem paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ObjectKey(String);

impl ObjectKey {
    /// Maximum encoded key size accepted by the initial storage format.
    pub const MAX_LENGTH: usize = 1_024;

    /// Validates and creates an object key.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    /// Returns the key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(value: &str) -> Result<(), CoreError> {
        if value.is_empty() {
            return Err(CoreError::InvalidObjectKey("key must not be empty".into()));
        }
        if value.len() > Self::MAX_LENGTH {
            return Err(CoreError::InvalidObjectKey(format!(
                "key exceeds {} bytes",
                Self::MAX_LENGTH
            )));
        }
        if value.starts_with('/') {
            return Err(CoreError::InvalidObjectKey(
                "key must not begin with a slash".into(),
            ));
        }
        if value.contains('\\') {
            return Err(CoreError::InvalidObjectKey(
                "backslashes are not permitted".into(),
            ));
        }
        if value.chars().any(char::is_control) {
            return Err(CoreError::InvalidObjectKey(
                "control characters are not permitted".into(),
            ));
        }
        if value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        {
            return Err(CoreError::InvalidObjectKey(
                "empty, '.' and '..' path segments are not permitted".into(),
            ));
        }
        Ok(())
    }
}

impl Display for ObjectKey {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ObjectKey {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for ObjectKey {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ObjectKey> for String {
    fn from(value: ObjectKey) -> Self {
        value.0
    }
}
