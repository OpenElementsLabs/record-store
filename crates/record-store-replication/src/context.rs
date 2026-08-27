//! Shared cluster context for the distributed data plane.

use std::sync::Arc;

use oes_cluster::{
    ClusterConfig, ClusterTopology, PayloadPlacement, PlacementPolicy, StorageClass,
};
use oes_consensus::{ClusterStore, ClusterWrite, ConsensusError, MetadataConsensus};
use oes_core::{NodeId, ObjectId};
use oes_metadata::MetadataRepository;
use oes_rpc::{ReplicaTarget, ReplicaTransport};
use oes_storage::{ReplicaStore, StorageError};

/// Everything the distributed data plane needs to serve one node's requests.
pub struct ClusterContext {
    /// This node's stable identity.
    pub node_id: NodeId,
    /// Replicated cluster state.
    pub cluster: Arc<dyn ClusterStore>,
    /// Replicated object catalog.
    pub metadata: Arc<dyn MetadataRepository>,
    /// This node's local replica storage.
    pub local: Arc<dyn ReplicaStore>,
    /// Transport to peer nodes.
    pub transport: Arc<dyn ReplicaTransport>,
    /// Replica placement decisions.
    pub placement: Arc<dyn PlacementPolicy>,
    /// Consensus handle, used to commit metadata and placement together.
    pub consensus: Option<Arc<MetadataConsensus>>,
}

impl ClusterContext {
    /// Returns the cluster-wide configuration.
    pub async fn config(&self) -> Result<ClusterConfig, StorageError> {
        self.cluster
            .config()
            .await
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?
            .ok_or_else(|| {
                StorageError::ClusterUnavailable(
                    "the cluster has not been initialized yet".to_owned(),
                )
            })
    }

    /// Returns the current topology view.
    pub async fn topology(&self) -> Result<ClusterTopology, StorageError> {
        self.cluster
            .topology()
            .await
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))
    }

    /// Returns placement metadata for a payload.
    pub async fn placement_for(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<PayloadPlacement>, StorageError> {
        self.cluster
            .placement(object_id)
            .await
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))
    }

    /// Returns the storage class new replicas should use.
    ///
    /// Bucket-level placement policy is modelled but not yet exposed, so every
    /// payload currently uses the default class.
    #[must_use]
    pub fn default_storage_class(&self) -> StorageClass {
        StorageClass::default()
    }

    /// Resolves the transport target for a node.
    pub async fn target(&self, node_id: NodeId) -> Result<ReplicaTarget, StorageError> {
        let node = self
            .cluster
            .node(node_id)
            .await
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?
            .ok_or_else(|| {
                StorageError::ClusterUnavailable(format!("node {node_id} is not a cluster member"))
            })?;
        Ok(ReplicaTarget {
            node_id,
            address: node.rpc_address,
        })
    }

    /// Commits a replicated write, requiring a consensus group in cluster mode.
    pub async fn commit(&self, write: ClusterWrite) -> Result<(), StorageError> {
        let consensus = self.consensus.as_ref().ok_or_else(|| {
            StorageError::ClusterUnavailable("this node has no metadata consensus".to_owned())
        })?;
        match consensus.write(write).await {
            Ok(oes_consensus::ClusterWriteResponse::Rejected(rejection)) => {
                Err(StorageError::Metadata(rejection.into_metadata_error()))
            }
            Ok(_) => Ok(()),
            Err(ConsensusError::Rejected(rejection)) => {
                Err(StorageError::Metadata(rejection.into_metadata_error()))
            }
            Err(error) => Err(StorageError::ClusterUnavailable(error.to_string())),
        }
    }
}
