//! Streaming object storage boundary and local filesystem implementation.

use std::io;

use record_store_core::{Checksum, CoreError, DeviceId, VersionId};
use record_store_metadata::MetadataError;
use thiserror::Error;

/// Storage failures that preserve actionable categories.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The target bucket does not exist.
    #[error("bucket was not found")]
    BucketNotFound,
    /// The target object does not exist.
    #[error("object was not found")]
    ObjectNotFound,
    /// The caller supplied an invalid range or domain value.
    #[error("invalid storage request: {0}")]
    InvalidRequest(#[from] CoreError),
    /// A cluster operation named a device this process does not serve.
    #[error("device {0} is not configured on this node")]
    UnknownDevice(DeviceId),
    /// A supplied checksum did not match the streamed payload.
    #[error("checksum mismatch: expected {expected}, calculated {actual}")]
    ChecksumMismatch {
        /// Caller-supplied checksum.
        expected: Checksum,
        /// Checksum calculated while streaming.
        actual: Checksum,
    },
    /// A requested immutable version is a logical delete marker.
    #[error("requested object version is a delete marker")]
    DeleteMarker {
        /// Stable marker version identifier.
        version_id: VersionId,
    },
    /// An upload stream failed before it was committed.
    #[error("upload stream failed: {0}")]
    UploadStream(#[source] io::Error),
    /// A named filesystem operation failed.
    #[error("storage filesystem operation '{operation}' failed: {source}")]
    Filesystem {
        /// Stable operation name without internal path details.
        operation: &'static str,
        /// Underlying I/O error for internal logs.
        #[source]
        source: io::Error,
    },
    /// Metadata publication failed.
    #[error("storage metadata operation failed: {0}")]
    Metadata(#[from] MetadataError),
    /// A crash-recovery publication record could not be encoded.
    #[error("storage publication record encoding failed: {0}")]
    PublicationEncoding(#[from] serde_json::Error),
    /// Metadata refers to a payload that is absent or unreadable.
    #[error("stored object state is inconsistent")]
    InconsistentState,
    /// Stored bytes no longer match their committed checksum.
    #[error("stored object failed integrity verification")]
    IntegrityMismatch,
    /// Encrypted payload state exists but no configured master key can open it.
    #[error("object encryption master key is required")]
    EncryptionKeyRequired,
    /// The configured master key does not match the durable storage key reference.
    #[error("object encryption master key does not match durable storage state")]
    EncryptionKeyMismatch,
    /// Authenticated payload encryption or decryption failed.
    #[error("object payload cryptography failed")]
    Cryptography,
    /// Fine-grained operation coordination was poisoned by a panic.
    #[error("storage operation coordination failed")]
    Coordination,
    /// A blocking durability operation could not finish.
    #[error("storage task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// Cluster state or placement could not be consulted.
    #[error("cluster storage is currently unavailable: {0}")]
    ClusterUnavailable(String),
    /// Fewer replicas became durable than the write policy requires.
    ///
    /// The write is refused rather than acknowledged: an object must never be
    /// reported as stored before its durability requirement is satisfied.
    #[error(
        "write durability was not met: {achieved} of {required} required replica          acknowledgement(s) succeeded ({detail})"
    )]
    DurabilityNotMet {
        /// Acknowledgements the policy required.
        required: u8,
        /// Acknowledgements that succeeded.
        achieved: u8,
        /// Per-target detail for operators.
        detail: String,
    },
    /// No healthy replica of the payload could be read.
    #[error("no healthy replica of the requested object is currently available")]
    NoHealthyReplica,
}
