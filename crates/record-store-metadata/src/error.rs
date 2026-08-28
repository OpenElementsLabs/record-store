//! Durable single-node metadata catalog.

use std::fmt::Display;

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
