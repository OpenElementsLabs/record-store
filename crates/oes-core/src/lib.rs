//! Fundamental domain types shared by OES components.

use std::{
    collections::BTreeMap,
    fmt::{self, Display, Formatter},
    num::{NonZeroU16, NonZeroU64},
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
    /// A bucket name violated S3/OES naming constraints.
    #[error("invalid bucket name: {0}")]
    InvalidBucketName(String),
    /// An object key violated the OES key constraints.
    #[error("invalid object key: {0}")]
    InvalidObjectKey(String),
    /// A checksum was malformed or unsupported.
    #[error("invalid checksum: {0}")]
    InvalidChecksum(String),
    /// An entity tag was malformed.
    #[error("invalid ETag: {0}")]
    InvalidETag(String),
    /// A multipart part number was outside the S3 range.
    #[error("invalid multipart part number: {0}")]
    InvalidPartNumber(String),
    /// A lifecycle expiration rule was contradictory or unsafe.
    #[error("invalid lifecycle rule: {0}")]
    InvalidLifecycleRule(String),
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
uuid_identifier!(UploadId, "multipart upload");
uuid_identifier!(NodeId, "node");
uuid_identifier!(ClusterId, "cluster");
uuid_identifier!(OrganizationId, "organization");
uuid_identifier!(ServiceAccountId, "service account");
uuid_identifier!(CredentialId, "credential");
uuid_identifier!(PolicyId, "policy");
uuid_identifier!(AuditEventId, "audit event");
uuid_identifier!(EventId, "storage event");
uuid_identifier!(WebhookId, "webhook");
uuid_identifier!(LifecycleRuleId, "lifecycle rule");
uuid_identifier!(ReplicaTaskId, "replica task");
uuid_identifier!(ClusterOperationId, "cluster operation");
uuid_identifier!(JoinTokenId, "join token");
uuid_identifier!(NodeCredentialId, "node credential");

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

fn validate_bucket_name(value: &str) -> Result<(), CoreError> {
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
        || matches!(value, "oes-system" | "oes-internal")
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

/// A stable protocol-facing entity tag, stored without HTTP quote characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ETag(String);

impl ETag {
    /// Creates a validated opaque entity tag.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'"')
        {
            return Err(CoreError::InvalidETag(
                "ETag must contain 1 to 128 visible ASCII characters excluding quotes".into(),
            ));
        }
        Ok(Self(value))
    }

    /// Builds the compatibility ETag for a single-part upload.
    #[must_use]
    pub fn from_md5(digest: [u8; 16]) -> Self {
        Self(hex::encode(digest))
    }

    /// Returns the unquoted ETag value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ETag {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for ETag {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ETag> for String {
    fn from(value: ETag) -> Self {
        value.0
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

/// S3 multipart part numbers are one-based and capped at 10,000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct PartNumber(NonZeroU16);

impl PartNumber {
    /// Highest part number accepted by S3 multipart uploads.
    pub const MAX: u16 = 10_000;

    /// Creates a validated part number.
    pub fn new(value: u16) -> Result<Self, CoreError> {
        if value > Self::MAX {
            return Err(CoreError::InvalidPartNumber(format!(
                "part number must be between 1 and {}",
                Self::MAX
            )));
        }
        NonZeroU16::new(value).map(Self).ok_or_else(|| {
            CoreError::InvalidPartNumber(format!("part number must be between 1 and {}", Self::MAX))
        })
    }

    /// Returns the one-based numeric part number.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl TryFrom<u16> for PartNumber {
    type Error = CoreError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PartNumber> for u16 {
    fn from(value: PartNumber) -> Self {
        value.get()
    }
}

impl FromStr for PartNumber {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .map_err(|_| CoreError::InvalidPartNumber("part number must be an integer".into()))
            .and_then(Self::new)
    }
}

impl Display for PartNumber {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.get(), formatter)
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

/// Logical bucket metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// Stable bucket identifier.
    pub id: BucketId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// User-facing bucket name.
    pub name: BucketName,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Current object-versioning behavior.
    #[serde(default)]
    pub versioning: VersioningState,
    /// Limits enforced transactionally at metadata publication.
    #[serde(default)]
    pub quota: BucketQuota,
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
    /// Versioned on-disk representation. Cryptographic key material is never
    /// part of this public metadata structure.
    #[serde(default)]
    pub payload_format: PayloadFormat,
    /// Stable protocol-facing entity tag.
    pub etag: ETag,
    /// Optional media type supplied by the caller.
    pub content_type: Option<String>,
    /// Caller-supplied metadata with deterministic serialization order.
    pub custom_metadata: BTreeMap<String, String>,
    /// Logical creation time.
    pub created_at: DateTime<Utc>,
    /// Last modification time.
    pub modified_at: DateTime<Utc>,
}

/// Versioned physical representation used for an immutable payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadFormat {
    /// Historical and explicitly unencrypted payload bytes.
    #[default]
    Plaintext,
    /// OES envelope encryption format 1 using chunked AES-256-GCM.
    Aes256GcmEnvelopeV1,
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

/// A logical delete marker in a version-enabled bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteMarker {
    /// Stable public version identifier.
    pub version_id: VersionId,
    /// Owning bucket.
    pub bucket_id: BucketId,
    /// Deleted logical key.
    pub key: ObjectKey,
    /// Time the marker became current.
    pub created_at: DateTime<Utc>,
}

/// One immutable entry in a key's version history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectVersionRecord {
    /// A version with an immutable physical payload.
    Object {
        /// Immutable payload metadata.
        metadata: ObjectMetadata,
        /// Whether S3 exposes this as the special `null` version.
        is_null: bool,
    },
    /// A logical deletion without a payload.
    DeleteMarker {
        /// Immutable logical deletion metadata.
        marker: DeleteMarker,
        /// Whether S3 exposes this as the special `null` version.
        is_null: bool,
    },
}

impl ObjectVersionRecord {
    /// Returns the stable version identifier.
    #[must_use]
    pub const fn version_id(&self) -> VersionId {
        match self {
            Self::Object { metadata, .. } => metadata.version_id,
            Self::DeleteMarker { marker, .. } => marker.version_id,
        }
    }

    /// Returns the logical key.
    #[must_use]
    pub fn key(&self) -> &ObjectKey {
        match self {
            Self::Object { metadata, .. } => &metadata.key,
            Self::DeleteMarker { marker, .. } => &marker.key,
        }
    }

    /// Returns the creation timestamp used for deterministic ordering.
    #[must_use]
    pub const fn created_at(&self) -> DateTime<Utc> {
        match self {
            Self::Object { metadata, .. } => metadata.created_at,
            Self::DeleteMarker { marker, .. } => marker.created_at,
        }
    }

    /// Returns true for a logical deletion entry.
    #[must_use]
    pub const fn is_delete_marker(&self) -> bool {
        matches!(self, Self::DeleteMarker { .. })
    }

    /// Returns whether this entry is the special unversioned/null entry.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        match self {
            Self::Object { is_null, .. } | Self::DeleteMarker { is_null, .. } => *is_null,
        }
    }
}

/// Lifecycle state of a durable multipart upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MultipartUploadState {
    /// Parts may be added or replaced.
    Active,
    /// A final immutable payload has been allocated and completion is recoverable.
    Completing { object_id: ObjectId },
}

/// Durable multipart upload descriptor. It contains no caller-controlled paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipartUpload {
    /// Opaque client-facing upload identifier.
    pub id: UploadId,
    /// Destination bucket.
    pub bucket_id: BucketId,
    /// Destination logical key.
    pub key: ObjectKey,
    /// Media type selected at initiation.
    pub content_type: Option<String>,
    /// Custom metadata selected at initiation.
    pub custom_metadata: BTreeMap<String, String>,
    /// Creation timestamp.
    pub initiated_at: DateTime<Utc>,
    /// Crash-recovery state.
    pub state: MultipartUploadState,
}

/// One durably persisted multipart payload part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadedPart {
    /// Owning upload.
    pub upload_id: UploadId,
    /// Validated one-based part number.
    pub number: PartNumber,
    /// OES-controlled immutable payload identifier.
    pub object_id: ObjectId,
    /// Part size in bytes.
    pub size: u64,
    /// Strong internal checksum.
    pub checksum: Checksum,
    /// Versioned physical representation of the durable part payload.
    #[serde(default)]
    pub payload_format: PayloadFormat,
    /// S3-compatible part ETag.
    pub etag: ETag,
    /// Last successful upload time.
    pub modified_at: DateTime<Utc>,
}

/// A caller completion entry, validated against durable parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedPart {
    /// Part number in ascending manifest order.
    pub number: PartNumber,
    /// ETag supplied by the completion request.
    pub etag: ETag,
}

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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

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
    fn bucket_names_follow_safe_s3_constraints() {
        for name in [
            "",
            "ab",
            "UPPERCASE",
            "-leading",
            "trailing-",
            "192.168.1.1",
            "a..b",
            "oes-system",
            "xn--reserved",
        ] {
            assert!(
                BucketName::new(name).is_err(),
                "accepted invalid name: {name}"
            );
        }
        assert_eq!(
            BucketName::new("photos-2026.example")
                .expect("valid bucket")
                .as_str(),
            "photos-2026.example"
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

    proptest! {
        #[test]
        fn accepted_bucket_names_always_satisfy_storage_safety_invariants(value in any::<String>()) {
            if let Ok(name) = BucketName::new(value) {
                let value = name.as_str();
                prop_assert!((BucketName::MIN_LENGTH..=BucketName::MAX_LENGTH).contains(&value.len()));
                prop_assert!(value.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')));
                prop_assert!(!value.contains(".."));
                prop_assert!(!value.starts_with('-') && !value.starts_with('.'));
                prop_assert!(!value.ends_with('-') && !value.ends_with('.'));
                prop_assert!(value.parse::<std::net::Ipv4Addr>().is_err());
            }
        }

        #[test]
        fn accepted_object_keys_never_contain_unsafe_path_segments(value in any::<String>()) {
            if let Ok(key) = ObjectKey::new(value) {
                let value = key.as_str();
                prop_assert!(!value.starts_with('/'));
                prop_assert!(!value.contains('\\'));
                prop_assert!(!value.chars().any(char::is_control));
                prop_assert!(value.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != ".."));
                prop_assert!(value.len() <= ObjectKey::MAX_LENGTH);
            }
        }
    }
}
