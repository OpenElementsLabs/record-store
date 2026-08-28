use thiserror::Error;

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
    /// A bucket name violated S3/Record Store naming constraints.
    #[error("invalid bucket name: {0}")]
    InvalidBucketName(String),
    /// An object key violated the Record Store key constraints.
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
    /// An erasure-coding profile was outside the supported safety bounds.
    #[error("invalid erasure profile: {0}")]
    InvalidErasureProfile(String),
    /// A replication profile was internally inconsistent.
    #[error("invalid replication profile: {0}")]
    InvalidReplicationProfile(String),
    /// A bucket CORS rule was malformed or unsafe.
    #[error("invalid CORS rule: {0}")]
    InvalidCorsRule(String),
}

impl CoreError {
    /// Returns the stable category for this domain error.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        ErrorCategory::InvalidInput
    }
}
