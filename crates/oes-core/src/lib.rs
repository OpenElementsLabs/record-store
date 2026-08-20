//! Fundamental domain types shared by OES components.

use std::{
    collections::BTreeMap,
    fmt::{self, Display, Formatter},
    num::NonZeroU64,
    str::FromStr,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Broad error categories suitable for transport-layer mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// The caller supplied an invalid value.
    InvalidInput,
    /// The requested entity does not exist.
    NotFound,
    /// Existing state conflicts with the operation.
    Conflict,
    /// An internal or durable dependency failed.
    Internal,
}

/// Errors raised while constructing or operating on core domain values.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// An identifier was not a valid UUID.
    #[error("invalid {kind} identifier: {reason}")]
    InvalidIdentifier {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// UUID parsing failure.
        reason: String,
    },
    /// An object key violated the OES key constraints.
    #[error("invalid object key: {0}")]
    InvalidObjectKey(String),
    /// A checksum was malformed or unsupported.
    #[error("invalid checksum: {0}")]
    InvalidChecksum(String),
    /// A byte range was invalid for the requested object.
    #[error("invalid byte range: {0}")]
    InvalidByteRange(String),
}

impl CoreError {
    /// Returns the stable category for this domain error.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        ErrorCategory::InvalidInput
    }
}

macro_rules! uuid_identifier {
    ($name:ident, $kind:literal) => {
        #[doc = concat!("Strongly typed ", $kind, " identifier.")]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            #[doc = concat!("Creates a random ", $kind, " identifier.")]
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Creates the typed identifier from an existing UUID.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                Display::fmt(&self.0, formatter)
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value)
                    .map(Self)
                    .map_err(|error| CoreError::InvalidIdentifier {
                        kind: $kind,
                        reason: error.to_string(),
                    })
            }
        }
    };
}

uuid_identifier!(BucketId, "bucket");
uuid_identifier!(ObjectId, "object");
uuid_identifier!(VersionId, "version");
uuid_identifier!(NodeId, "node");
uuid_identifier!(OrganizationId, "organization");
uuid_identifier!(ServiceAccountId, "service account");

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

/// Supported content-integrity algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    /// SHA-256, stored as a 32-byte digest.
    Sha256,
}

impl Display for ChecksumAlgorithm {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha256 => formatter.write_str("sha256"),
        }
    }
}

/// A content checksum with an explicit algorithm discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Checksum {
    algorithm: ChecksumAlgorithm,
    digest: Vec<u8>,
}

impl Checksum {
    /// Creates a SHA-256 checksum from a complete digest.
    #[must_use]
    pub fn sha256(digest: [u8; 32]) -> Self {
        Self {
            algorithm: ChecksumAlgorithm::Sha256,
            digest: digest.to_vec(),
        }
    }

    /// Returns the checksum algorithm.
    #[must_use]
    pub const fn algorithm(&self) -> ChecksumAlgorithm {
        self.algorithm
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub fn digest(&self) -> &[u8] {
        &self.digest
    }
}

impl Display for Checksum {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}",
            self.algorithm,
            hex::encode(&self.digest)
        )
    }
}

impl FromStr for Checksum {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (algorithm, encoded) = value.split_once(':').ok_or_else(|| {
            CoreError::InvalidChecksum("expected '<algorithm>:<hex digest>'".into())
        })?;
        let digest = hex::decode(encoded)
            .map_err(|error| CoreError::InvalidChecksum(format!("invalid hex digest: {error}")))?;
        match algorithm {
            "sha256" if digest.len() == 32 => Ok(Self {
                algorithm: ChecksumAlgorithm::Sha256,
                digest,
            }),
            "sha256" => Err(CoreError::InvalidChecksum(
                "SHA-256 digest must contain exactly 32 bytes".into(),
            )),
            _ => Err(CoreError::InvalidChecksum(format!(
                "unsupported algorithm '{algorithm}'"
            ))),
        }
    }
}

impl TryFrom<String> for Checksum {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Checksum> for String {
    fn from(value: Checksum) -> Self {
        value.to_string()
    }
}

/// A requested byte range expressed as an offset and non-zero length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    offset: u64,
    length: NonZeroU64,
}

impl ByteRange {
    /// Creates a range and rejects zero lengths or integer overflow.
    pub fn new(offset: u64, length: u64) -> Result<Self, CoreError> {
        let length = NonZeroU64::new(length)
            .ok_or_else(|| CoreError::InvalidByteRange("length must be non-zero".into()))?;
        offset
            .checked_add(length.get())
            .ok_or_else(|| CoreError::InvalidByteRange("offset plus length exceeds u64".into()))?;
        Ok(Self { offset, length })
    }

    /// Returns the first requested byte offset.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Returns the requested length.
    #[must_use]
    pub const fn length(self) -> u64 {
        self.length.get()
    }

    /// Resolves the range against an object, truncating its tail at EOF.
    pub fn resolve(self, object_size: u64) -> Result<ResolvedByteRange, CoreError> {
        if self.offset >= object_size {
            return Err(CoreError::InvalidByteRange(
                "range starts at or beyond the end of the object".into(),
            ));
        }
        let available = object_size - self.offset;
        Ok(ResolvedByteRange {
            offset: self.offset,
            length: self.length().min(available),
        })
    }
}

/// A byte range resolved against a concrete object size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedByteRange {
    /// First returned byte offset.
    pub offset: u64,
    /// Number of bytes returned.
    pub length: u64,
}

/// Logical bucket metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// Stable bucket identifier.
    pub id: BucketId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// User-facing bucket name.
    pub name: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// Persisted object metadata, independent of the physical object layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectMetadata {
    /// Immutable physical object identifier.
    pub id: ObjectId,
    /// Logical owning bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
    /// Current version identifier.
    pub version_id: VersionId,
    /// Payload length in bytes.
    pub size: u64,
    /// Payload integrity checksum.
    pub checksum: Checksum,
    /// Optional media type supplied by the caller.
    pub content_type: Option<String>,
    /// Caller-supplied metadata with deterministic serialization order.
    pub custom_metadata: BTreeMap<String, String>,
    /// Logical creation time.
    pub created_at: DateTime<Utc>,
    /// Last modification time.
    pub modified_at: DateTime<Utc>,
}

/// A concise reference to an immutable object version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectVersion {
    /// Version identifier.
    pub id: VersionId,
    /// Physical payload identifier.
    pub object_id: ObjectId,
    /// Time at which the version was committed.
    pub created_at: DateTime<Utc>,
}

/// Aggregate storage accounting values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageUsage {
    /// Number of committed objects.
    pub object_count: u64,
    /// Total committed payload bytes.
    pub bytes_used: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_identifiers_are_not_interchangeable() {
        let raw = Uuid::new_v4();
        let bucket = BucketId::from_uuid(raw);
        let object = ObjectId::from_uuid(raw);
        assert_eq!(bucket.to_string(), object.to_string());
        assert_eq!(bucket.as_uuid(), raw);
    }

    #[test]
    fn object_keys_reject_path_traversal_and_ambiguous_paths() {
        for key in ["", "/root", "../secret", "a/../secret", "a//b", "a\\b"] {
            assert!(ObjectKey::new(key).is_err(), "accepted invalid key: {key}");
        }
        assert_eq!(
            ObjectKey::new("images/2026/photo.jpg")
                .expect("valid object key")
                .as_str(),
            "images/2026/photo.jpg"
        );
    }

    #[test]
    fn checksum_round_trips_through_its_stable_text_form() {
        let checksum = Checksum::sha256([0xab; 32]);
        let encoded = checksum.to_string();
        assert_eq!(
            encoded.parse::<Checksum>().expect("valid checksum"),
            checksum
        );
        assert!("sha256:abcd".parse::<Checksum>().is_err());
        assert!(
            format!("md5:{}", "00".repeat(16))
                .parse::<Checksum>()
                .is_err()
        );
    }

    #[test]
    fn byte_ranges_are_checked_and_clamped() {
        assert!(ByteRange::new(0, 0).is_err());
        assert!(ByteRange::new(u64::MAX, 2).is_err());
        assert!(ByteRange::new(10, 1).expect("range").resolve(10).is_err());
        assert_eq!(
            ByteRange::new(5, 20)
                .expect("range")
                .resolve(10)
                .expect("resolved"),
            ResolvedByteRange {
                offset: 5,
                length: 5
            }
        );
    }
}
