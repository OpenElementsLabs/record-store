//! Durable single-node metadata catalog.

use std::fmt::Display;

use record_store_core::LockBlock;
use thiserror::Error;

/// Stable metadata failure categories.
#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("failed to prepare metadata directory: {0}")]
    Directory(#[source] std::io::Error),
    #[error("bucket already exists")]
    BucketAlreadyExists,
    #[error("bucket was not found")]
    BucketNotFound,
    #[error("bucket is not empty")]
    BucketNotEmpty,
    #[error("multipart upload was not found")]
    MultipartUploadNotFound,
    #[error("multipart upload state conflicts with the operation")]
    MultipartStateConflict,
    #[error("storage quota exceeded")]
    QuotaExceeded,
    #[error("invalid bucket versioning transition")]
    InvalidVersioningTransition,
    /// The version is under an Object Lock retention or a legal hold.
    ///
    /// This is raised from inside the write transaction that would have removed
    /// the version, so it cannot be raced by a concurrent retention change.
    #[error("object version is held by {}", .0.label())]
    VersionLocked(LockBlock),
    /// Wall-clock time is behind the recorded high-water mark, so no retention
    /// decision that releases an object can be trusted right now.
    #[error("the system clock is behind the recorded high-water mark")]
    ClockWentBackwards,
    /// The bucket does not have Object Lock enabled.
    #[error("object lock is not enabled on this bucket")]
    ObjectLockNotEnabled,
    /// Object Lock can only be enabled when a bucket is created.
    #[error("object lock cannot be enabled on an existing bucket")]
    ObjectLockNotEnabledAtCreation,
    /// Versioning cannot be suspended while Object Lock is enabled.
    #[error("bucket versioning cannot be suspended while object lock is enabled")]
    ObjectLockRequiresVersioning,
    /// The named version does not exist, or holds no payload to retain.
    #[error("object lock version was not found")]
    ObjectLockVersionNotFound,
    /// The requested lock change would have released a protected version.
    #[error("object lock change refused: {}", .0.label())]
    ObjectLockChangeRefused(record_store_core::LockChangeRefused),
    /// An Object Lock value was incoherent.
    #[error("invalid object lock: {0}")]
    InvalidObjectLock(String),
    #[error("lifecycle rule was not found")]
    LifecycleRuleNotFound,
    #[error("invalid lifecycle rule: {0}")]
    InvalidLifecycleRule(String),
    #[error("metadata encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("metadata database operation '{operation}' failed: {reason}")]
    Database {
        operation: &'static str,
        reason: String,
    },
    #[error("metadata task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub(crate) fn counter_error() -> MetadataError {
    MetadataError::Database {
        operation: "adjust counter",
        reason: "counter overflow or underflow".into(),
    }
}
pub(crate) fn backend(operation: &'static str, error: impl Display) -> MetadataError {
    MetadataError::Database {
        operation,
        reason: error.to_string(),
    }
}
