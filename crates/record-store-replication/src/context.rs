//! Shared cluster context for the distributed data plane.

use std::sync::Arc;

use record_store_cluster::{
    ClusterConfig, ClusterTopology, PayloadPlacement, PlacementPolicy, StorageClass,
};
use record_store_consensus::{ClusterStore, ClusterWrite, ConsensusError, MetadataConsensus};
use record_store_core::{DeviceId, NodeId, ObjectId};
use record_store_metadata::MetadataRepository;
use record_store_rpc::{ReplicaTarget, ReplicaTransport};
use record_store_storage::{DeviceStore, StorageError};

/// Everything the distributed data plane needs to serve one node's requests.
pub struct ClusterContext {
    /// This node's stable identity.
    pub node_id: NodeId,
    /// Replicated cluster state.
    pub cluster: Arc<dyn ClusterStore>,
    /// Replicated object catalog.
    pub metadata: Arc<dyn MetadataRepository>,
    /// This node's local replica storage.
    pub local: Arc<DeviceStore>,
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
    /// The class a payload falls back to when a bucket selected none.
    #[must_use]
    pub fn default_storage_class(&self) -> StorageClass {
        StorageClass::default()
    }

    /// Returns the storage class a bucket's new objects belong to.
    ///
    /// One linearizable read. Callers on the write path use this and pass the
    /// class onward rather than resolving the bucket twice.
    pub async fn storage_class_for(
        &self,
        bucket_id: record_store_core::BucketId,
    ) -> Result<Option<record_store_core::StorageClass>, StorageError> {
        self.metadata
            .get_bucket(bucket_id)
            .await?
            .map(|bucket| bucket.storage_class)
            .ok_or(StorageError::BucketNotFound)
    }

    /// Resolves the policy a bucket's objects should be placed by.
    ///
    /// Convenience for callers that hold only a bucket identifier. The write
    /// path should use [`ClusterContext::storage_policy_for_class`] instead,
    /// which avoids a second linearizable read.
    pub async fn storage_policy_for(
        &self,
        bucket_id: record_store_core::BucketId,
    ) -> Result<Option<record_store_cluster::StoragePolicy>, StorageError> {
        let Some(bucket) = self.metadata.get_bucket(bucket_id).await? else {
            return Err(StorageError::BucketNotFound);
        };
        self.storage_policy_for_class(bucket.storage_class).await
    }

    /// Resolves the policy a storage class means.
    ///
    /// Takes the class rather than a bucket on purpose: reading a bucket is a
    /// linearizable metadata read, and the write path has already read one. A
    /// second read per write would put an avoidable consensus round trip in
    /// front of every upload.
    ///
    /// A class nobody defined is reported rather than quietly replaced by the
    /// default — silently ignoring it would put data on hardware the operator
    /// deliberately excluded.
    pub async fn storage_policy_for_class(
        &self,
        class: Option<record_store_core::StorageClass>,
    ) -> Result<Option<record_store_cluster::StoragePolicy>, StorageError> {
        let class = class.unwrap_or_default();
        self.cluster
            .storage_policies()
            .await
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?
            .into_iter()
            .find(|policy| policy.class == class)
            .map(Some)
            .ok_or_else(|| {
                StorageError::ClusterUnavailable(format!(
                    "bucket storage class '{class}' is not a defined storage policy"
                ))
            })
    }

    /// Resolves the transport target for a node.
    pub async fn target(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
    ) -> Result<ReplicaTarget, StorageError> {
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
            device_id,
            address: node.rpc_address,
        })
    }

    /// Commits a replicated write, requiring a consensus group in cluster mode.
    pub async fn commit(&self, write: ClusterWrite) -> Result<(), StorageError> {
        let consensus = self.consensus.as_ref().ok_or_else(|| {
            StorageError::ClusterUnavailable("this node has no metadata consensus".to_owned())
        })?;
        match consensus.write(write).await {
            Ok(record_store_consensus::ClusterWriteResponse::Rejected(rejection)) => {
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
