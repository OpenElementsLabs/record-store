//! The replicated command and response types.
//!
//! The consensus log carries only metadata commands. Object bytes never enter
//! it: they are streamed directly between storage nodes and referenced from the
//! committed metadata.

use record_store_cluster::{ClusterCatalogError, ClusterCommand, ClusterOutcome};
use record_store_metadata::{MetadataCommand, MetadataError, MetadataOutcome};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One replicated write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ClusterWrite {
    /// A mutation of the object catalog.
    Metadata(Box<MetadataCommand>),
    /// A mutation of cluster state.
    Cluster(Box<ClusterCommand>),
    /// Several mutations that must commit or fail together.
    ///
    /// The distributed commit protocol uses this to publish an object version
    /// and its replica placement atomically, so a half-replicated object can
    /// never become visible.
    Batch(Vec<ClusterWrite>),
    /// A no-op used to establish a fresh leader's commit index.
    Noop,
}

impl ClusterWrite {
    /// Wraps an object-catalog command.
    #[must_use]
    pub fn metadata(command: MetadataCommand) -> Self {
        Self::Metadata(Box::new(command))
    }

    /// Wraps a cluster-state command.
    #[must_use]
    pub fn cluster(command: ClusterCommand) -> Self {
        Self::Cluster(Box::new(command))
    }

    /// Groups commands into one atomic write.
    #[must_use]
    pub fn batch(writes: impl IntoIterator<Item = Self>) -> Self {
        Self::Batch(writes.into_iter().collect())
    }

    /// Returns a stable short name for tracing and metrics.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Metadata(command) => command.name(),
            Self::Cluster(command) => command.name(),
            Self::Batch(_) => "batch",
            Self::Noop => "noop",
        }
    }
}

/// The result of applying one replicated write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ClusterWriteResponse {
    /// The object-catalog command succeeded.
    Metadata(Box<MetadataOutcome>),
    /// The cluster-state command succeeded.
    Cluster(Box<ClusterOutcome>),
    /// Every command in the batch succeeded, in order.
    Batch(Vec<ClusterWriteResponse>),
    /// A no-op completed.
    Noop,
    /// The command was rejected by application rules and changed nothing.
    Rejected(CommandRejection),
}

impl ClusterWriteResponse {
    /// Returns the object-catalog outcome, or the rejection as an error.
    pub fn into_metadata(self) -> Result<MetadataOutcome, MetadataError> {
        match self {
            Self::Metadata(outcome) => Ok(*outcome),
            Self::Rejected(rejection) => Err(rejection.into_metadata_error()),
            Self::Batch(mut responses) if responses.len() == 1 => {
                responses.remove(0).into_metadata()
            }
            other => Err(MetadataError::Database {
                operation: "decode replicated response",
                reason: format!(
                    "expected an object-catalog outcome, received {}",
                    other.name()
                ),
            }),
        }
    }

    /// Returns the cluster outcome, or the rejection as an error.
    pub fn into_cluster(self) -> Result<ClusterOutcome, CommandRejection> {
        match self {
            Self::Cluster(outcome) => Ok(*outcome),
            Self::Rejected(rejection) => Err(rejection),
            Self::Batch(mut responses) if responses.len() == 1 => {
                responses.remove(0).into_cluster()
            }
            other => Err(CommandRejection {
                kind: RejectionKind::Internal,
                message: format!("expected a cluster outcome, received {}", other.name()),
            }),
        }
    }

    /// Returns the responses of a batch write in order.
    pub fn into_batch(self) -> Result<Vec<Self>, CommandRejection> {
        match self {
            Self::Batch(responses) => Ok(responses),
            Self::Rejected(rejection) => Err(rejection),
            other => Ok(vec![other]),
        }
    }

    /// Returns a stable short name for tracing and metrics.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Metadata(_) => "metadata",
            Self::Cluster(_) => "cluster",
            Self::Batch(_) => "batch",
            Self::Noop => "noop",
            Self::Rejected(_) => "rejected",
        }
    }
}

/// Why a replicated command was rejected without changing state.
///
/// Application rejections are replicated as ordinary responses rather than
/// consensus failures: every member must reach the same conclusion, and the
/// consensus layer must not shut down because a client asked for something
/// invalid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{message}")]
pub struct CommandRejection {
    /// Stable machine-readable category.
    pub kind: RejectionKind,
    /// Operator-facing detail.
    pub message: String,
}

impl CommandRejection {
    /// Reconstructs the equivalent object-catalog error.
    #[must_use]
    pub fn into_metadata_error(self) -> MetadataError {
        match self.kind {
            RejectionKind::BucketAlreadyExists => MetadataError::BucketAlreadyExists,
            RejectionKind::BucketNotFound => MetadataError::BucketNotFound,
            RejectionKind::BucketNotEmpty => MetadataError::BucketNotEmpty,
            RejectionKind::MultipartUploadNotFound => MetadataError::MultipartUploadNotFound,
            RejectionKind::MultipartStateConflict => MetadataError::MultipartStateConflict,
            RejectionKind::QuotaExceeded => MetadataError::QuotaExceeded,
            RejectionKind::InvalidVersioningTransition => {
                MetadataError::InvalidVersioningTransition
            }
            RejectionKind::LifecycleRuleNotFound => MetadataError::LifecycleRuleNotFound,
            RejectionKind::InvalidLifecycleRule => {
                MetadataError::InvalidLifecycleRule(self.message)
            }
            _ => MetadataError::Database {
                operation: "replicated metadata command",
                reason: self.message,
            },
        }
    }
}

/// Stable rejection categories shared by every Record Store layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionKind {
    /// A bucket with that name or identifier already exists.
    BucketAlreadyExists,
    /// The bucket does not exist.
    BucketNotFound,
    /// The bucket still holds object versions or multipart uploads.
    BucketNotEmpty,
    /// The multipart upload does not exist.
    MultipartUploadNotFound,
    /// The multipart upload is in a conflicting state.
    MultipartStateConflict,
    /// A quota would be exceeded.
    QuotaExceeded,
    /// The requested versioning transition is not allowed.
    InvalidVersioningTransition,
    /// The lifecycle rule does not exist.
    LifecycleRuleNotFound,
    /// The lifecycle rule is invalid.
    InvalidLifecycleRule,
    /// The cluster has not been initialized.
    ClusterNotInitialized,
    /// The cluster is already initialized with a different identity.
    ClusterAlreadyInitialized,
    /// The node is not a cluster member.
    NodeNotFound,
    /// Replica placement metadata does not exist.
    PlacementNotFound,
    /// The replica record does not exist.
    ReplicaNotFound,
    /// The tombstone does not exist.
    TombstoneNotFound,
    /// The replica movement task does not exist.
    TaskNotFound,
    /// The cluster operation does not exist.
    OperationNotFound,
    /// The join token does not exist.
    JoinTokenNotFound,
    /// The node credential does not exist.
    CredentialNotFound,
    /// A node lifecycle transition is not permitted.
    InvalidTransition,
    /// The proposed configuration is invalid.
    InvalidConfiguration,
    /// An internal invariant was violated.
    Internal,
}

/// Classifies an object-catalog error for replication.
#[must_use]
pub fn classify_metadata_error(error: &MetadataError) -> CommandRejection {
    let kind = match error {
        MetadataError::BucketAlreadyExists => RejectionKind::BucketAlreadyExists,
        MetadataError::BucketNotFound => RejectionKind::BucketNotFound,
        MetadataError::BucketNotEmpty => RejectionKind::BucketNotEmpty,
        MetadataError::MultipartUploadNotFound => RejectionKind::MultipartUploadNotFound,
        MetadataError::MultipartStateConflict => RejectionKind::MultipartStateConflict,
        MetadataError::QuotaExceeded => RejectionKind::QuotaExceeded,
        MetadataError::InvalidVersioningTransition => RejectionKind::InvalidVersioningTransition,
        MetadataError::LifecycleRuleNotFound => RejectionKind::LifecycleRuleNotFound,
        MetadataError::InvalidLifecycleRule(_) => RejectionKind::InvalidLifecycleRule,
        _ => RejectionKind::Internal,
    };
    CommandRejection {
        kind,
        message: error.to_string(),
    }
}

/// Classifies a cluster-catalog error for replication.
#[must_use]
pub fn classify_cluster_error(error: &ClusterCatalogError) -> CommandRejection {
    let kind = match error {
        ClusterCatalogError::NotInitialized => RejectionKind::ClusterNotInitialized,
        ClusterCatalogError::AlreadyInitialized(_)
        | ClusterCatalogError::ClusterMismatch { .. } => RejectionKind::ClusterAlreadyInitialized,
        ClusterCatalogError::NodeNotFound(_) => RejectionKind::NodeNotFound,
        ClusterCatalogError::PlacementNotFound(_) => RejectionKind::PlacementNotFound,
        ClusterCatalogError::ReplicaNotFound { .. } => RejectionKind::ReplicaNotFound,
        ClusterCatalogError::TombstoneNotFound(_) => RejectionKind::TombstoneNotFound,
        ClusterCatalogError::TaskNotFound(_) => RejectionKind::TaskNotFound,
        ClusterCatalogError::OperationNotFound(_) => RejectionKind::OperationNotFound,
        ClusterCatalogError::JoinTokenNotFound(_) => RejectionKind::JoinTokenNotFound,
        ClusterCatalogError::CredentialNotFound(_) => RejectionKind::CredentialNotFound,
        ClusterCatalogError::Topology(_) => RejectionKind::InvalidTransition,
        ClusterCatalogError::Configuration(_) => RejectionKind::InvalidConfiguration,
        _ => RejectionKind::Internal,
    };
    CommandRejection {
        kind,
        message: error.to_string(),
    }
}

/// Returns whether a catalog error means the state machine itself is broken.
///
/// Only genuinely durable failures may abort consensus; application rejections
/// must be replicated so that every member agrees on the outcome.
#[must_use]
pub const fn is_durable_metadata_failure(error: &MetadataError) -> bool {
    matches!(
        error,
        MetadataError::Directory(_) | MetadataError::Database { .. } | MetadataError::Task(_)
    )
}

/// Returns whether a cluster-catalog error means durable state is broken.
#[must_use]
pub const fn is_durable_cluster_failure(error: &ClusterCatalogError) -> bool {
    matches!(
        error,
        ClusterCatalogError::Database { .. }
            | ClusterCatalogError::Task(_)
            | ClusterCatalogError::IncompatibleFormat { .. }
    )
}
