use record_store_core::{
    ClusterId, ClusterOperationId, JoinTokenId, NodeId, ObjectId, ReplicaTaskId,
};
use thiserror::Error;

use crate::{config::ClusterConfigError, topology::TopologyError};

/// Failures raised by the cluster catalog.
#[derive(Debug, Error)]
pub enum ClusterCatalogError {
    /// The cluster has not been initialized yet.
    #[error("cluster has not been initialized; run 'record-store cluster init' first")]
    NotInitialized,
    /// The cluster was already initialized.
    #[error("cluster {0} is already initialized")]
    AlreadyInitialized(ClusterId),
    /// A command referred to a different cluster.
    #[error("command targets cluster {requested} but this catalog holds cluster {stored}")]
    ClusterMismatch {
        /// Cluster stored in the catalog.
        stored: ClusterId,
        /// Cluster named by the command.
        requested: ClusterId,
    },
    /// The node is unknown.
    #[error("node {0} is not a member of this cluster")]
    NodeNotFound(NodeId),
    /// Placement metadata is unknown.
    #[error("placement metadata for payload {0} was not found")]
    PlacementNotFound(ObjectId),
    /// The referenced replica record does not exist.
    #[error("payload {object_id} has no replica on node {node_id}")]
    ReplicaNotFound {
        /// Payload identifier.
        object_id: ObjectId,
        /// Node identifier.
        node_id: NodeId,
    },
    /// The tombstone is unknown.
    #[error("tombstone for payload {0} was not found")]
    TombstoneNotFound(ObjectId),
    /// The task is unknown.
    #[error("replica task {0} was not found")]
    TaskNotFound(ReplicaTaskId),
    /// The operation is unknown.
    #[error("cluster operation {0} was not found")]
    OperationNotFound(ClusterOperationId),
    /// The join token is unknown.
    #[error("join token {0} was not found")]
    JoinTokenNotFound(JoinTokenId),
    /// The node credential is unknown.
    #[error("node credential for node {0} was not found")]
    CredentialNotFound(NodeId),
    /// A node lifecycle transition was refused.
    #[error(transparent)]
    Topology(#[from] TopologyError),
    /// The proposed configuration was invalid.
    #[error(transparent)]
    Configuration(#[from] ClusterConfigError),
    /// State encoding or decoding failed.
    #[error("cluster catalog encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    /// The durable catalog was written by an incompatible build.
    #[error(
        "cluster catalog format version {found} is newer than this build supports ({expected})"
    )]
    IncompatibleFormat {
        /// Version found on disk.
        found: u32,
        /// Version this build supports.
        expected: u32,
    },
    /// A durable operation failed.
    #[error("cluster catalog operation '{operation}' failed: {reason}")]
    Database {
        /// Stable operation name.
        operation: &'static str,
        /// Backend failure detail.
        reason: String,
    },
    /// A blocking catalog task could not finish.
    #[error("cluster catalog task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub(crate) fn backend<E: std::fmt::Display>(
    operation: &'static str,
    error: E,
) -> ClusterCatalogError {
    ClusterCatalogError::Database {
        operation,
        reason: error.to_string(),
    }
}

pub(crate) type CatalogResult<T> = Result<T, ClusterCatalogError>;
