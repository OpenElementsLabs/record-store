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

#[cfg(test)]
mod tests {
    use record_store_cluster::ClusterCatalogError;
    use record_store_core::{ClusterId, NodeId};
    use record_store_metadata::MetadataError;

    use super::*;

    /// Every catalog error that has a dedicated rejection category must survive
    /// the trip through consensus and back. A rejection is classified on the
    /// leader, replicated as data, and reconstructed on the caller's node: if
    /// the round trip loses the category, two members answer the same command
    /// differently.
    #[test]
    fn a_classified_metadata_rejection_reconstructs_the_same_error() {
        let cases = [
            MetadataError::BucketAlreadyExists,
            MetadataError::BucketNotFound,
            MetadataError::BucketNotEmpty,
            MetadataError::MultipartUploadNotFound,
            MetadataError::MultipartStateConflict,
            MetadataError::QuotaExceeded,
            MetadataError::InvalidVersioningTransition,
            MetadataError::LifecycleRuleNotFound,
        ];
        for original in cases {
            let label = format!("{original:?}");
            let restored = classify_metadata_error(&original).into_metadata_error();
            assert_eq!(
                std::mem::discriminant(&restored),
                std::mem::discriminant(&original),
                "{label} did not survive classification"
            );
        }
    }

    /// The detail of an invalid lifecycle rule is the only part an operator can
    /// act on, so it has to be carried through the rejection rather than being
    /// replaced by a generic message.
    #[test]
    fn an_invalid_lifecycle_rule_carries_its_reason_through_consensus() {
        let original = MetadataError::InvalidLifecycleRule("expiration must be positive".into());
        let restored = classify_metadata_error(&original).into_metadata_error();
        let MetadataError::InvalidLifecycleRule(reason) = restored else {
            panic!("lifecycle rule detail lost its category");
        };
        assert!(reason.contains("expiration must be positive"), "{reason}");
    }

    /// An error with no dedicated category must not be silently mapped onto an
    /// unrelated one; it degrades to an internal rejection that still explains
    /// itself.
    #[test]
    fn an_unclassified_failure_degrades_to_internal_without_losing_its_message() {
        let original = MetadataError::Database {
            operation: "commit",
            reason: "disk full".into(),
        };
        let rejection = classify_metadata_error(&original);
        assert_eq!(rejection.kind, RejectionKind::Internal);
        assert!(
            rejection.message.contains("disk full"),
            "{}",
            rejection.message
        );
    }

    /// This is the invariant that keeps a cluster from tearing itself apart: a
    /// failure that consensus is allowed to abort on must never be one that also
    /// classifies as an ordinary application rejection. If it were both, one
    /// member could abort while another replicated a rejection, and the members
    /// would no longer agree.
    #[test]
    fn no_application_rejection_is_also_treated_as_a_durable_failure() {
        let application_failures = [
            MetadataError::BucketAlreadyExists,
            MetadataError::BucketNotFound,
            MetadataError::BucketNotEmpty,
            MetadataError::MultipartUploadNotFound,
            MetadataError::MultipartStateConflict,
            MetadataError::QuotaExceeded,
            MetadataError::InvalidVersioningTransition,
            MetadataError::LifecycleRuleNotFound,
            MetadataError::InvalidLifecycleRule("bad".into()),
        ];
        for error in application_failures {
            let label = format!("{error:?}");
            assert_ne!(
                classify_metadata_error(&error).kind,
                RejectionKind::Internal,
                "{label} should have a dedicated category"
            );
            assert!(
                !is_durable_metadata_failure(&error),
                "{label} would abort consensus even though it is an application rejection"
            );
        }
    }

    #[test]
    fn genuine_storage_faults_are_treated_as_durable_failures() {
        assert!(is_durable_metadata_failure(&MetadataError::Database {
            operation: "commit",
            reason: "io".into(),
        }));
        assert!(is_durable_cluster_failure(&ClusterCatalogError::Database {
            operation: "commit",
            reason: "io".into(),
        }));
    }

    #[test]
    fn cluster_failures_are_classified_and_never_both_durable_and_rejectable() {
        let cases = [
            (
                ClusterCatalogError::NotInitialized,
                RejectionKind::ClusterNotInitialized,
            ),
            (
                ClusterCatalogError::AlreadyInitialized(ClusterId::new()),
                RejectionKind::ClusterAlreadyInitialized,
            ),
            (
                ClusterCatalogError::NodeNotFound(NodeId::new()),
                RejectionKind::NodeNotFound,
            ),
        ];
        for (error, expected) in cases {
            let label = format!("{error:?}");
            assert_eq!(classify_cluster_error(&error).kind, expected, "{label}");
            assert!(
                !is_durable_cluster_failure(&error),
                "{label} must not abort consensus"
            );
        }
    }

    /// These names travel between members. Renaming a variant would make a
    /// mixed-version cluster misread rejections, so the wire form is pinned
    /// here rather than left to the derive.
    #[test]
    fn rejection_categories_have_a_stable_wire_form() {
        for (kind, expected) in [
            (RejectionKind::BucketNotFound, "\"bucket_not_found\""),
            (RejectionKind::QuotaExceeded, "\"quota_exceeded\""),
            (
                RejectionKind::ClusterAlreadyInitialized,
                "\"cluster_already_initialized\"",
            ),
            (RejectionKind::Internal, "\"internal\""),
        ] {
            let encoded = serde_json::to_string(&kind).expect("serialise");
            assert_eq!(encoded, expected);
            let decoded: RejectionKind = serde_json::from_str(&encoded).expect("deserialise");
            assert_eq!(decoded, kind);
        }
    }

    /// A batch is how an object version and its replica placement are published
    /// atomically. A single-command batch has to be transparent to the caller,
    /// or the distributed commit path would report a decode failure for a write
    /// that actually succeeded.
    #[test]
    fn a_single_command_batch_is_transparent_to_its_caller() {
        let inner = ClusterWriteResponse::Noop;
        let batched = ClusterWriteResponse::Batch(vec![inner.clone()]);
        assert!(batched.clone().into_metadata().is_err());
        assert_eq!(batched.into_batch().expect("batch"), vec![inner]);
    }

    #[test]
    fn a_rejected_batch_surfaces_the_rejection_rather_than_an_empty_result() {
        let rejection = CommandRejection {
            kind: RejectionKind::BucketNotFound,
            message: "no such bucket".into(),
        };
        let response = ClusterWriteResponse::Rejected(rejection.clone());
        assert_eq!(
            response.clone().into_batch().expect_err("must reject").kind,
            rejection.kind
        );
        assert_eq!(
            response.into_cluster().expect_err("must reject").kind,
            rejection.kind
        );
    }

    /// Asking for the wrong kind of outcome must be an explicit error naming
    /// what actually arrived, never a silent default.
    #[test]
    fn a_mismatched_response_reports_what_it_actually_received() {
        let error = ClusterWriteResponse::Noop
            .into_cluster()
            .expect_err("noop is not a cluster outcome");
        assert_eq!(error.kind, RejectionKind::Internal);
        assert!(error.message.contains("noop"), "{}", error.message);
    }

    #[test]
    fn write_and_response_names_are_stable_for_tracing() {
        assert_eq!(ClusterWrite::Noop.name(), "noop");
        assert_eq!(ClusterWrite::batch([ClusterWrite::Noop]).name(), "batch");
        assert_eq!(ClusterWriteResponse::Noop.name(), "noop");
        assert_eq!(ClusterWriteResponse::Batch(Vec::new()).name(), "batch");
    }

    /// The write itself crosses the log, so it has to round-trip through the
    /// same encoding a follower will decode it with.
    #[test]
    fn a_batched_write_round_trips_through_its_encoded_form() {
        let write = ClusterWrite::batch([ClusterWrite::Noop, ClusterWrite::Noop]);
        let encoded = serde_json::to_string(&write).expect("serialise");
        let decoded: ClusterWrite = serde_json::from_str(&encoded).expect("deserialise");
        assert_eq!(decoded.name(), "batch");
        let ClusterWrite::Batch(writes) = decoded else {
            panic!("batch lost its shape");
        };
        assert_eq!(writes.len(), 2);
    }
}
