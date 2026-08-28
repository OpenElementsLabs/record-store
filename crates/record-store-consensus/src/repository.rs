//! Consensus-backed catalog adapters.
//!
//! These adapters make replication invisible to the layers above: object
//! operations keep using [`MetadataRepository`], and cluster operations keep
//! using [`ClusterStore`], while writes travel through the consensus log and
//! reads are served from the locally applied state behind a read barrier.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use record_store_cluster::{
    ClusterCatalogError, ClusterCommand, ClusterConfig, ClusterIdentity, ClusterOutcome,
    ClusterTopology, ClusterUsage, JoinToken, NodeCredential, NodeRecord, PayloadPlacement,
    PlacementPage, RaftNodeId, ReplicaTask, TaskPage, Tombstone,
};
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, ClusterOperationId, CorsConfiguration, JoinTokenId,
    LifecycleRule, LifecycleRuleId, MultipartUpload, NodeCredentialId, NodeId, ObjectId, ObjectKey,
    ObjectMetadata, ObjectVersionRecord, PartNumber, ReplicaTaskId, StorageUsage, UploadId,
    UploadedPart, VersionId, VersioningState,
};
use record_store_metadata::{
    DeleteObjectResult, DeleteVersionResult, ListMultipartUploadsRequest,
    ListObjectVersionsRequest, ListObjectsRequest, MetadataCommand, MetadataError, MetadataOutcome,
    MetadataRepository, MultipartCleanupResult, MultipartUploadPage, NewDeleteMarker,
    ObjectCommitResult, ObjectMetadataPage, ObjectVersionPage, PayloadReferencePage,
};

use crate::{
    command::ClusterWrite,
    consensus::{ConsensusError, MetadataConsensus},
};

fn metadata_error(error: ConsensusError) -> MetadataError {
    match error {
        ConsensusError::Rejected(rejection) => rejection.into_metadata_error(),
        other => MetadataError::Database {
            operation: "replicated metadata operation",
            reason: other.to_string(),
        },
    }
}

/// A [`MetadataRepository`] whose writes go through consensus.
///
/// Reads are served from this member's applied state after a read barrier, so a
/// successful write is always visible to the read that follows it.
pub struct ReplicatedMetadataRepository {
    consensus: Arc<MetadataConsensus>,
    local: Arc<dyn MetadataRepository>,
}

impl ReplicatedMetadataRepository {
    /// Wraps the consensus group and its locally applied catalog.
    #[must_use]
    pub fn new(consensus: Arc<MetadataConsensus>) -> Self {
        let local: Arc<dyn MetadataRepository> = Arc::new(consensus.state().metadata().clone());
        Self { consensus, local }
    }

    async fn propose(&self, command: MetadataCommand) -> Result<MetadataOutcome, MetadataError> {
        self.consensus
            .write(ClusterWrite::metadata(command))
            .await
            .map_err(metadata_error)?
            .into_metadata()
    }

    async fn barrier(&self) -> Result<(), MetadataError> {
        self.consensus
            .ensure_read_consistency()
            .await
            .map_err(metadata_error)
    }
}

#[async_trait]
impl MetadataRepository for ReplicatedMetadataRepository {
    async fn create_bucket(&self, bucket: &Bucket) -> Result<(), MetadataError> {
        self.propose(MetadataCommand::CreateBucket {
            bucket: Box::new(bucket.clone()),
        })
        .await
        .map(|_| ())
    }

    async fn get_bucket(&self, id: BucketId) -> Result<Option<Bucket>, MetadataError> {
        self.barrier().await?;
        self.local.get_bucket(id).await
    }

    async fn get_bucket_by_name(&self, name: &BucketName) -> Result<Option<Bucket>, MetadataError> {
        self.barrier().await?;
        self.local.get_bucket_by_name(name).await
    }

    async fn list_buckets(&self) -> Result<Vec<Bucket>, MetadataError> {
        self.barrier().await?;
        self.local.list_buckets().await
    }

    async fn set_bucket_versioning(
        &self,
        id: BucketId,
        state: VersioningState,
    ) -> Result<Bucket, MetadataError> {
        self.propose(MetadataCommand::SetBucketVersioning {
            bucket_id: id,
            state,
        })
        .await?
        .into_bucket()
    }

    async fn set_bucket_quota(
        &self,
        id: BucketId,
        quota: BucketQuota,
    ) -> Result<Bucket, MetadataError> {
        self.propose(MetadataCommand::SetBucketQuota {
            bucket_id: id,
            quota,
        })
        .await?
        .into_bucket()
    }

    async fn set_bucket_cors(
        &self,
        id: BucketId,
        configuration: Option<CorsConfiguration>,
    ) -> Result<Bucket, MetadataError> {
        self.propose(MetadataCommand::SetBucketCors {
            bucket_id: id,
            configuration,
        })
        .await?
        .into_bucket()
    }

    async fn delete_bucket(&self, name: &BucketName) -> Result<Bucket, MetadataError> {
        self.propose(MetadataCommand::DeleteBucket { name: name.clone() })
            .await?
            .into_bucket()
    }

    async fn put_object(
        &self,
        metadata: &ObjectMetadata,
    ) -> Result<ObjectCommitResult, MetadataError> {
        self.propose(MetadataCommand::PutObject {
            metadata: Box::new(metadata.clone()),
        })
        .await?
        .into_object_commit()
    }

    async fn get_object(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectMetadata>, MetadataError> {
        self.barrier().await?;
        self.local.get_object(bucket, key).await
    }

    async fn get_object_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
    ) -> Result<Option<ObjectVersionRecord>, MetadataError> {
        self.barrier().await?;
        self.local.get_object_version(bucket, key, version).await
    }

    async fn get_null_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectVersionRecord>, MetadataError> {
        self.barrier().await?;
        self.local.get_null_version(bucket, key).await
    }

    async fn delete_object(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        marker: NewDeleteMarker,
    ) -> Result<DeleteObjectResult, MetadataError> {
        self.propose(MetadataCommand::DeleteObject {
            bucket_id: bucket,
            key: key.clone(),
            marker,
        })
        .await?
        .into_delete_object()
    }

    async fn delete_object_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
    ) -> Result<Option<DeleteVersionResult>, MetadataError> {
        self.propose(MetadataCommand::DeleteObjectVersion {
            bucket_id: bucket,
            key: key.clone(),
            version_id: version,
        })
        .await?
        .into_delete_version()
    }

    async fn list_objects(
        &self,
        request: ListObjectsRequest,
    ) -> Result<ObjectMetadataPage, MetadataError> {
        self.barrier().await?;
        self.local.list_objects(request).await
    }

    async fn list_object_versions(
        &self,
        request: ListObjectVersionsRequest,
    ) -> Result<ObjectVersionPage, MetadataError> {
        self.barrier().await?;
        self.local.list_object_versions(request).await
    }

    async fn create_multipart_upload(&self, upload: &MultipartUpload) -> Result<(), MetadataError> {
        self.propose(MetadataCommand::CreateMultipartUpload {
            upload: Box::new(upload.clone()),
        })
        .await
        .map(|_| ())
    }

    async fn get_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<Option<MultipartUpload>, MetadataError> {
        self.barrier().await?;
        self.local.get_multipart_upload(id).await
    }

    async fn put_multipart_part(
        &self,
        part: &UploadedPart,
    ) -> Result<Option<UploadedPart>, MetadataError> {
        self.propose(MetadataCommand::PutMultipartPart {
            part: Box::new(part.clone()),
        })
        .await?
        .into_replaced_part()
    }

    async fn list_multipart_parts(
        &self,
        id: UploadId,
        after: Option<PartNumber>,
        limit: usize,
    ) -> Result<Vec<UploadedPart>, MetadataError> {
        self.barrier().await?;
        self.local.list_multipart_parts(id, after, limit).await
    }

    async fn list_multipart_uploads(
        &self,
        request: ListMultipartUploadsRequest,
    ) -> Result<MultipartUploadPage, MetadataError> {
        self.barrier().await?;
        self.local.list_multipart_uploads(request).await
    }

    async fn begin_multipart_completion(
        &self,
        id: UploadId,
        object_id: ObjectId,
    ) -> Result<MultipartUpload, MetadataError> {
        self.propose(MetadataCommand::BeginMultipartCompletion {
            upload_id: id,
            object_id,
        })
        .await?
        .into_multipart_upload()
    }

    async fn finish_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<MultipartCleanupResult, MetadataError> {
        self.propose(MetadataCommand::FinishMultipartUpload { upload_id: id })
            .await?
            .into_multipart_cleanup()
    }

    async fn abort_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<MultipartCleanupResult, MetadataError> {
        self.propose(MetadataCommand::AbortMultipartUpload { upload_id: id })
            .await?
            .into_multipart_cleanup()
    }

    async fn recover_multipart_completions(&self) -> Result<MultipartCleanupResult, MetadataError> {
        // Recovery is a cluster-wide reconciliation, so it is proposed through
        // consensus rather than performed locally on every node.
        self.propose(MetadataCommand::RecoverMultipartCompletions)
            .await?
            .into_multipart_cleanup()
    }

    async fn put_lifecycle_rule(&self, rule: &LifecycleRule) -> Result<(), MetadataError> {
        self.propose(MetadataCommand::PutLifecycleRule {
            rule: Box::new(rule.clone()),
        })
        .await
        .map(|_| ())
    }

    async fn list_lifecycle_rules(
        &self,
        bucket: Option<BucketId>,
    ) -> Result<Vec<LifecycleRule>, MetadataError> {
        self.barrier().await?;
        self.local.list_lifecycle_rules(bucket).await
    }

    async fn delete_lifecycle_rule(&self, id: LifecycleRuleId) -> Result<(), MetadataError> {
        self.propose(MetadataCommand::DeleteLifecycleRule { rule_id: id })
            .await
            .map(|_| ())
    }

    async fn bucket_usage(
        &self,
    ) -> Result<
        std::collections::BTreeMap<
            record_store_core::BucketId,
            record_store_metadata::BucketUsageSummary,
        >,
        MetadataError,
    > {
        // Accounting counters tolerate a locally applied view: they inform
        // operators, they never decide durability or visibility.
        self.local.bucket_usage().await
    }

    async fn storage_usage(&self) -> Result<StorageUsage, MetadataError> {
        // Usage counters tolerate a locally applied view: they are monitoring
        // values, never a durability or visibility decision.
        self.local.storage_usage().await
    }

    async fn pending_cleanup(&self, limit: usize) -> Result<Vec<ObjectId>, MetadataError> {
        self.local.pending_cleanup(limit).await
    }

    async fn complete_cleanup(&self, id: ObjectId) -> Result<(), MetadataError> {
        self.propose(MetadataCommand::CompleteCleanup { object_id: id })
            .await
            .map(|_| ())
    }

    async fn payload_referenced(&self, id: ObjectId) -> Result<bool, MetadataError> {
        self.barrier().await?;
        self.local.payload_referenced(id).await
    }

    async fn list_payload_references(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<PayloadReferencePage, MetadataError> {
        self.barrier().await?;
        self.local.list_payload_references(after, limit).await
    }

    async fn check_ready(&self) -> Result<(), MetadataError> {
        self.local.check_ready().await
    }
}

/// Cluster-state operations used by the control and data planes.
///
/// Standalone deployments implement this directly against the local catalog;
/// cluster deployments route writes through consensus. Callers never need to know
/// which one they hold.
#[async_trait]
pub trait ClusterStore: Send + Sync {
    /// Applies a cluster-state command.
    async fn apply(&self, command: ClusterCommand) -> Result<ClusterOutcome, ClusterCatalogError>;

    /// Ensures a following read observes every committed cluster write.
    async fn ensure_read_consistency(&self) -> Result<(), ClusterCatalogError>;

    /// Returns the cluster identity, if the cluster has been initialized.
    async fn identity(&self) -> Result<Option<ClusterIdentity>, ClusterCatalogError>;

    /// Returns the cluster-wide configuration.
    async fn config(&self) -> Result<Option<ClusterConfig>, ClusterCatalogError>;

    /// Returns one node record.
    async fn node(&self, node_id: NodeId) -> Result<Option<NodeRecord>, ClusterCatalogError>;

    /// Returns the node owning a consensus member identifier.
    async fn node_by_member(
        &self,
        raft_id: RaftNodeId,
    ) -> Result<Option<NodeRecord>, ClusterCatalogError>;

    /// Returns every node record.
    async fn nodes(&self) -> Result<Vec<NodeRecord>, ClusterCatalogError>;

    /// Returns a topology view for placement decisions.
    async fn topology(&self) -> Result<ClusterTopology, ClusterCatalogError>;

    /// Returns placement metadata for one payload.
    async fn placement(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<PayloadPlacement>, ClusterCatalogError>;

    /// Returns a bounded page of placements.
    async fn list_placements(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<PlacementPage, ClusterCatalogError>;

    /// Returns the payloads a node is recorded as holding.
    async fn node_replicas(
        &self,
        node_id: NodeId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, ClusterCatalogError>;

    /// Returns how many replica records a node holds.
    async fn node_replica_count(&self, node_id: NodeId) -> Result<u64, ClusterCatalogError>;

    /// Returns the tombstone for a payload.
    async fn tombstone(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<Tombstone>, ClusterCatalogError>;

    /// Returns tombstones with outstanding acknowledgements.
    async fn pending_tombstones(&self, limit: usize)
    -> Result<Vec<Tombstone>, ClusterCatalogError>;

    /// Returns tombstones eligible for purging.
    async fn purgeable_tombstones(
        &self,
        retention_hours: u32,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, ClusterCatalogError>;

    /// Returns one replica movement task.
    async fn task(
        &self,
        task_id: ReplicaTaskId,
    ) -> Result<Option<ReplicaTask>, ClusterCatalogError>;

    /// Returns active tasks in priority order.
    async fn queued_tasks(&self, limit: usize) -> Result<TaskPage, ClusterCatalogError>;

    /// Returns one long-running operation.
    async fn operation(
        &self,
        operation_id: ClusterOperationId,
    ) -> Result<Option<record_store_cluster::ClusterOperation>, ClusterCatalogError>;

    /// Returns recorded operations, newest first.
    async fn operations(
        &self,
        limit: usize,
    ) -> Result<Vec<record_store_cluster::ClusterOperation>, ClusterCatalogError>;

    /// Returns a join token record.
    async fn join_token(
        &self,
        token_id: JoinTokenId,
    ) -> Result<Option<JoinToken>, ClusterCatalogError>;

    /// Returns a node credential record.
    async fn node_credential(
        &self,
        node_id: NodeId,
    ) -> Result<Option<NodeCredential>, ClusterCatalogError>;

    /// Returns the node credential registered under a credential identifier.
    async fn node_credential_by_id(
        &self,
        credential_id: NodeCredentialId,
    ) -> Result<Option<NodeCredential>, ClusterCatalogError>;

    /// Returns cluster-wide accounting.
    async fn usage(&self) -> Result<ClusterUsage, ClusterCatalogError>;

    /// Recomputes the summary durability counters.
    async fn refresh_durability_counters(&self) -> Result<(), ClusterCatalogError>;

    /// Verifies that cluster state is usable.
    async fn check_ready(&self) -> Result<(), ClusterCatalogError>;
}

fn cluster_error(error: ConsensusError) -> ClusterCatalogError {
    match error {
        ConsensusError::Rejected(rejection) => ClusterCatalogError::Database {
            operation: "replicated cluster command",
            reason: rejection.message,
        },
        other => ClusterCatalogError::Database {
            operation: "replicated cluster operation",
            reason: other.to_string(),
        },
    }
}

/// A [`ClusterStore`] whose writes go through consensus.
pub struct ReplicatedClusterStore {
    consensus: Arc<MetadataConsensus>,
    local: record_store_cluster::ClusterCatalog,
}

impl ReplicatedClusterStore {
    /// Wraps the consensus group and its locally applied cluster catalog.
    #[must_use]
    pub fn new(consensus: Arc<MetadataConsensus>) -> Self {
        let local = consensus.state().cluster().clone();
        Self { consensus, local }
    }
}

#[async_trait]
impl ClusterStore for ReplicatedClusterStore {
    async fn apply(&self, command: ClusterCommand) -> Result<ClusterOutcome, ClusterCatalogError> {
        self.consensus
            .write(ClusterWrite::cluster(command))
            .await
            .map_err(cluster_error)?
            .into_cluster()
            .map_err(|rejection| ClusterCatalogError::Database {
                operation: "replicated cluster command",
                reason: rejection.message,
            })
    }

    async fn ensure_read_consistency(&self) -> Result<(), ClusterCatalogError> {
        self.consensus
            .ensure_read_consistency()
            .await
            .map_err(cluster_error)
    }

    async fn identity(&self) -> Result<Option<ClusterIdentity>, ClusterCatalogError> {
        self.local.identity().await
    }

    async fn config(&self) -> Result<Option<ClusterConfig>, ClusterCatalogError> {
        self.local.config().await
    }

    async fn node(&self, node_id: NodeId) -> Result<Option<NodeRecord>, ClusterCatalogError> {
        self.local.node(node_id).await
    }

    async fn node_by_member(
        &self,
        raft_id: RaftNodeId,
    ) -> Result<Option<NodeRecord>, ClusterCatalogError> {
        self.local.node_by_member(raft_id).await
    }

    async fn nodes(&self) -> Result<Vec<NodeRecord>, ClusterCatalogError> {
        self.local.nodes().await
    }

    async fn topology(&self) -> Result<ClusterTopology, ClusterCatalogError> {
        self.local.topology().await
    }

    async fn placement(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<PayloadPlacement>, ClusterCatalogError> {
        self.local.placement(object_id).await
    }

    async fn list_placements(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<PlacementPage, ClusterCatalogError> {
        self.local.list_placements(after, limit).await
    }

    async fn node_replicas(
        &self,
        node_id: NodeId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, ClusterCatalogError> {
        self.local.node_replicas(node_id, after, limit).await
    }

    async fn node_replica_count(&self, node_id: NodeId) -> Result<u64, ClusterCatalogError> {
        self.local.node_replica_count(node_id).await
    }

    async fn tombstone(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<Tombstone>, ClusterCatalogError> {
        self.local.tombstone(object_id).await
    }

    async fn pending_tombstones(
        &self,
        limit: usize,
    ) -> Result<Vec<Tombstone>, ClusterCatalogError> {
        self.local.pending_tombstones(limit).await
    }

    async fn purgeable_tombstones(
        &self,
        retention_hours: u32,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, ClusterCatalogError> {
        self.local
            .purgeable_tombstones(retention_hours, now, limit)
            .await
    }

    async fn task(
        &self,
        task_id: ReplicaTaskId,
    ) -> Result<Option<ReplicaTask>, ClusterCatalogError> {
        self.local.task(task_id).await
    }

    async fn queued_tasks(&self, limit: usize) -> Result<TaskPage, ClusterCatalogError> {
        self.local.queued_tasks(limit).await
    }

    async fn operation(
        &self,
        operation_id: ClusterOperationId,
    ) -> Result<Option<record_store_cluster::ClusterOperation>, ClusterCatalogError> {
        self.local.operation(operation_id).await
    }

    async fn operations(
        &self,
        limit: usize,
    ) -> Result<Vec<record_store_cluster::ClusterOperation>, ClusterCatalogError> {
        self.local.operations(limit).await
    }

    async fn join_token(
        &self,
        token_id: JoinTokenId,
    ) -> Result<Option<JoinToken>, ClusterCatalogError> {
        self.local.join_token(token_id).await
    }

    async fn node_credential(
        &self,
        node_id: NodeId,
    ) -> Result<Option<NodeCredential>, ClusterCatalogError> {
        self.local.node_credential(node_id).await
    }

    async fn node_credential_by_id(
        &self,
        credential_id: NodeCredentialId,
    ) -> Result<Option<NodeCredential>, ClusterCatalogError> {
        self.local.node_credential_by_id(credential_id).await
    }

    async fn usage(&self) -> Result<ClusterUsage, ClusterCatalogError> {
        self.local.usage().await
    }

    async fn refresh_durability_counters(&self) -> Result<(), ClusterCatalogError> {
        self.local.refresh_durability_counters().await
    }

    async fn check_ready(&self) -> Result<(), ClusterCatalogError> {
        self.local.check_ready().await
    }
}

/// A [`ClusterStore`] backed directly by a local catalog.
///
/// Standalone deployments use this so a single-node installation never pays for
/// consensus it does not need.
pub struct LocalClusterStore {
    catalog: record_store_cluster::ClusterCatalog,
}

impl LocalClusterStore {
    /// Wraps a local catalog.
    #[must_use]
    pub const fn new(catalog: record_store_cluster::ClusterCatalog) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl ClusterStore for LocalClusterStore {
    async fn apply(&self, command: ClusterCommand) -> Result<ClusterOutcome, ClusterCatalogError> {
        self.catalog.apply(command).await
    }

    async fn ensure_read_consistency(&self) -> Result<(), ClusterCatalogError> {
        Ok(())
    }

    async fn identity(&self) -> Result<Option<ClusterIdentity>, ClusterCatalogError> {
        self.catalog.identity().await
    }

    async fn config(&self) -> Result<Option<ClusterConfig>, ClusterCatalogError> {
        self.catalog.config().await
    }

    async fn node(&self, node_id: NodeId) -> Result<Option<NodeRecord>, ClusterCatalogError> {
        self.catalog.node(node_id).await
    }

    async fn node_by_member(
        &self,
        raft_id: RaftNodeId,
    ) -> Result<Option<NodeRecord>, ClusterCatalogError> {
        self.catalog.node_by_member(raft_id).await
    }

    async fn nodes(&self) -> Result<Vec<NodeRecord>, ClusterCatalogError> {
        self.catalog.nodes().await
    }

    async fn topology(&self) -> Result<ClusterTopology, ClusterCatalogError> {
        self.catalog.topology().await
    }

    async fn placement(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<PayloadPlacement>, ClusterCatalogError> {
        self.catalog.placement(object_id).await
    }

    async fn list_placements(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<PlacementPage, ClusterCatalogError> {
        self.catalog.list_placements(after, limit).await
    }

    async fn node_replicas(
        &self,
        node_id: NodeId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, ClusterCatalogError> {
        self.catalog.node_replicas(node_id, after, limit).await
    }

    async fn node_replica_count(&self, node_id: NodeId) -> Result<u64, ClusterCatalogError> {
        self.catalog.node_replica_count(node_id).await
    }

    async fn tombstone(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<Tombstone>, ClusterCatalogError> {
        self.catalog.tombstone(object_id).await
    }

    async fn pending_tombstones(
        &self,
        limit: usize,
    ) -> Result<Vec<Tombstone>, ClusterCatalogError> {
        self.catalog.pending_tombstones(limit).await
    }

    async fn purgeable_tombstones(
        &self,
        retention_hours: u32,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, ClusterCatalogError> {
        self.catalog
            .purgeable_tombstones(retention_hours, now, limit)
            .await
    }

    async fn task(
        &self,
        task_id: ReplicaTaskId,
    ) -> Result<Option<ReplicaTask>, ClusterCatalogError> {
        self.catalog.task(task_id).await
    }

    async fn queued_tasks(&self, limit: usize) -> Result<TaskPage, ClusterCatalogError> {
        self.catalog.queued_tasks(limit).await
    }

    async fn operation(
        &self,
        operation_id: ClusterOperationId,
    ) -> Result<Option<record_store_cluster::ClusterOperation>, ClusterCatalogError> {
        self.catalog.operation(operation_id).await
    }

    async fn operations(
        &self,
        limit: usize,
    ) -> Result<Vec<record_store_cluster::ClusterOperation>, ClusterCatalogError> {
        self.catalog.operations(limit).await
    }

    async fn join_token(
        &self,
        token_id: JoinTokenId,
    ) -> Result<Option<JoinToken>, ClusterCatalogError> {
        self.catalog.join_token(token_id).await
    }

    async fn node_credential(
        &self,
        node_id: NodeId,
    ) -> Result<Option<NodeCredential>, ClusterCatalogError> {
        self.catalog.node_credential(node_id).await
    }

    async fn node_credential_by_id(
        &self,
        credential_id: NodeCredentialId,
    ) -> Result<Option<NodeCredential>, ClusterCatalogError> {
        self.catalog.node_credential_by_id(credential_id).await
    }

    async fn usage(&self) -> Result<ClusterUsage, ClusterCatalogError> {
        self.catalog.usage().await
    }

    async fn refresh_durability_counters(&self) -> Result<(), ClusterCatalogError> {
        self.catalog.refresh_durability_counters().await
    }

    async fn check_ready(&self) -> Result<(), ClusterCatalogError> {
        self.catalog.check_ready().await
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use record_store_cluster::{
        ClusterConfig, ClusterIdentity, FailureDomain, NodeCapacity, NodeRegistration,
        NodeVersions, StorageClass,
    };
    use record_store_core::{
        Bucket, BucketId, BucketName, BucketQuota, ClusterId, NodeId, ObjectKey, OrganizationId,
        VersioningState,
    };

    use super::*;
    use crate::test_support::consensus;

    fn metadata_for(
        bucket_id: BucketId,
        key: &str,
        size: u64,
    ) -> record_store_core::ObjectMetadata {
        record_store_core::ObjectMetadata {
            id: record_store_core::ObjectId::new(),
            bucket_id,
            key: ObjectKey::new(key).expect("key"),
            version_id: record_store_core::VersionId::new(),
            size,
            checksum: record_store_core::Checksum::sha256([9_u8; 32]),
            payload_format: record_store_core::PayloadFormat::Plaintext,
            durability: record_store_core::DurabilityProfile::Single,
            etag: record_store_core::ETag::from_md5([9_u8; 16]),
            content_type: None,
            custom_metadata: Default::default(),
            created_at: Utc::now(),
            modified_at: Utc::now(),
        }
    }

    fn bucket(name: &str) -> Bucket {
        Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new(name).expect("bucket name"),
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
            durability_policy: None,
            cors: None,
        }
    }

    /// A write is only real once consensus has committed and applied it. Reading
    /// it back through the same repository is what proves the round trip, not
    /// just that the proposal was accepted.
    #[tokio::test]
    async fn a_replicated_write_is_readable_once_it_commits() {
        let (_directory, consensus) = consensus().await;
        let repository = ReplicatedMetadataRepository::new(Arc::clone(&consensus));
        let record = bucket("replicated");

        repository.create_bucket(&record).await.expect("create");
        let stored = repository
            .get_bucket(record.id)
            .await
            .expect("read")
            .expect("bucket");
        assert_eq!(stored.name, record.name);
        assert_eq!(repository.list_buckets().await.expect("list").len(), 1);
    }

    /// An application rejection must come back as its own catalog error rather
    /// than as a consensus failure, or a client would be told the cluster is
    /// broken when it merely asked for something invalid.
    #[tokio::test]
    async fn an_application_rejection_surfaces_as_a_catalog_error() {
        let (_directory, consensus) = consensus().await;
        let repository = ReplicatedMetadataRepository::new(Arc::clone(&consensus));
        let record = bucket("duplicated");
        repository.create_bucket(&record).await.expect("create");

        let clash = bucket("duplicated");
        assert!(matches!(
            repository.create_bucket(&clash).await,
            Err(MetadataError::BucketAlreadyExists)
        ));

        assert!(matches!(
            repository
                .delete_bucket(&BucketName::new("never-created").expect("name"))
                .await,
            Err(MetadataError::BucketNotFound)
        ));
    }

    /// Object writes go through the same path and have to leave the catalog in
    /// the state a later read expects, including the usage counters.
    #[tokio::test]
    async fn replicated_object_writes_reach_the_applied_catalog() {
        let (_directory, consensus) = consensus().await;
        let repository = ReplicatedMetadataRepository::new(Arc::clone(&consensus));
        let record = bucket("objects");
        repository.create_bucket(&record).await.expect("create");

        let key = ObjectKey::new("a.txt").expect("key");
        let metadata = record_store_core::ObjectMetadata {
            id: record_store_core::ObjectId::new(),
            bucket_id: record.id,
            key: key.clone(),
            version_id: record_store_core::VersionId::new(),
            size: 12,
            checksum: record_store_core::Checksum::sha256([9_u8; 32]),
            payload_format: record_store_core::PayloadFormat::Plaintext,
            durability: record_store_core::DurabilityProfile::Single,
            etag: record_store_core::ETag::from_md5([9_u8; 16]),
            content_type: None,
            custom_metadata: Default::default(),
            created_at: Utc::now(),
            modified_at: Utc::now(),
        };
        repository.put_object(&metadata).await.expect("put");

        assert_eq!(
            repository
                .get_object(record.id, &key)
                .await
                .expect("read")
                .expect("object")
                .size,
            12
        );
        let usage = repository.storage_usage().await.expect("usage");
        assert_eq!(usage.object_count, 1);
    }

    /// Cluster state replicates through the same group, so both stores have to
    /// agree about what was committed.
    #[tokio::test]
    async fn the_replicated_cluster_store_commits_and_reads_back() {
        let (_directory, consensus) = consensus().await;
        let store = ReplicatedClusterStore::new(Arc::clone(&consensus));

        store
            .apply(ClusterCommand::InitializeCluster {
                identity: ClusterIdentity {
                    cluster_id: ClusterId::new(),
                    cluster_format_version: record_store_cluster::CLUSTER_FORMAT_VERSION,
                    created_at: Utc::now(),
                },
                config: Box::new(ClusterConfig::default()),
            })
            .await
            .expect("initialize");

        let node_id = NodeId::new();
        store
            .apply(ClusterCommand::RegisterNode {
                registration: Box::new(NodeRegistration {
                    node_id,
                    versions: NodeVersions::current("test"),
                    rpc_address: "127.0.0.1:17604".to_owned(),
                    s3_endpoint: None,
                    storage_class: StorageClass::new("standard").expect("class"),
                    failure_domain: FailureDomain::default(),
                    capacity: NodeCapacity::default(),
                    started_at: Utc::now(),
                }),
                at: Utc::now(),
            })
            .await
            .expect("register");

        assert!(store.node(node_id).await.expect("read").is_some());
        assert_eq!(store.nodes().await.expect("list").len(), 1);
    }

    /// Forgetting a node the cluster no longer knows is a no-op rather than a
    /// failure, so a retried decommission does not error on its second attempt.
    /// The outcome still reports that nothing changed.
    #[tokio::test]
    async fn forgetting_an_unknown_node_is_an_idempotent_no_op() {
        let (_directory, consensus) = consensus().await;
        let store = ReplicatedClusterStore::new(Arc::clone(&consensus));

        let outcome = store
            .apply(ClusterCommand::ForgetNode {
                node_id: NodeId::new(),
            })
            .await
            .expect("forgetting an unknown node is not an error");
        assert!(
            !outcome.changed(),
            "nothing was there to forget: {outcome:?}"
        );
    }

    /// A rejected cluster command keeps its explanation but not its variant.
    ///
    /// Unlike the metadata path, which reconstructs `BucketNotFound` and friends
    /// exactly, cluster errors carry identifiers (`NodeNotFound(NodeId)`) that a
    /// `CommandRejection` does not preserve — it holds only a kind and a message.
    /// So the replicated path reports `Database` with the original reason. That
    /// is pinned here because callers can only rely on the message today; making
    /// the variant survive would mean carrying the identifier in the rejection.
    #[tokio::test]
    async fn a_rejected_cluster_command_keeps_its_explanation() {
        let (_directory, consensus) = consensus().await;
        let store = ReplicatedClusterStore::new(Arc::clone(&consensus));
        let node_id = NodeId::new();

        let result = store
            .apply(ClusterCommand::SetNodeState {
                node_id,
                state: record_store_cluster::NodeState::Draining,
                reason: None,
                at: Utc::now(),
            })
            .await;
        let Err(error) = result else {
            panic!("a state change for an unknown node must not succeed");
        };
        let rendered = error.to_string();
        assert!(
            rendered.contains(&node_id.to_string()),
            "the node the caller asked about must appear: {rendered}"
        );
    }

    /// The standalone store is the same surface without consensus, and a
    /// deployment that never forms a cluster depends on it behaving identically.
    #[tokio::test]
    async fn the_local_cluster_store_behaves_like_the_replicated_one() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let catalog =
            record_store_cluster::ClusterCatalog::open(directory.path().join("cluster.redb"))
                .await
                .expect("catalog");
        let store = LocalClusterStore::new(catalog);

        store
            .apply(ClusterCommand::InitializeCluster {
                identity: ClusterIdentity {
                    cluster_id: ClusterId::new(),
                    cluster_format_version: record_store_cluster::CLUSTER_FORMAT_VERSION,
                    created_at: Utc::now(),
                },
                config: Box::new(ClusterConfig::default()),
            })
            .await
            .expect("initialize");

        assert!(store.identity().await.expect("read").is_some());
        assert!(store.nodes().await.expect("list").is_empty());
        assert!(matches!(
            store
                .apply(ClusterCommand::SetNodeState {
                    node_id: NodeId::new(),
                    state: record_store_cluster::NodeState::Draining,
                    reason: None,
                    at: Utc::now(),
                })
                .await,
            Err(ClusterCatalogError::NodeNotFound(_))
        ));
    }

    /// Every catalog mutation a deployment performs goes through consensus in
    /// cluster mode. Exercising each one here is what proves the replicated
    /// repository is a drop-in for the local catalog rather than a subset.
    #[tokio::test]
    async fn every_bucket_mutation_replicates_and_reads_back() {
        let (_directory, consensus) = consensus().await;
        let repository = ReplicatedMetadataRepository::new(Arc::clone(&consensus));
        let record = bucket("mutated");
        repository.create_bucket(&record).await.expect("create");

        repository
            .set_bucket_versioning(record.id, VersioningState::Enabled)
            .await
            .expect("versioning");
        repository
            .set_bucket_quota(
                record.id,
                record_store_core::BucketQuota {
                    bytes: record_store_core::ByteQuota::Limit(8_192),
                    objects: record_store_core::ObjectCountQuota::Limit(16),
                },
            )
            .await
            .expect("quota");
        repository
            .set_bucket_cors(record.id, None)
            .await
            .expect("cors");

        let stored = repository
            .get_bucket(record.id)
            .await
            .expect("read")
            .expect("bucket");
        assert_eq!(stored.versioning, VersioningState::Enabled);
        assert_eq!(
            stored.quota.bytes,
            record_store_core::ByteQuota::Limit(8_192)
        );
        assert!(
            repository
                .get_bucket_by_name(&record.name)
                .await
                .expect("read")
                .is_some()
        );
    }

    /// The object surface replicates too, including the delete that leaves a
    /// marker and the version reads that follow it.
    #[tokio::test]
    async fn the_object_surface_replicates_including_versions() {
        let (_directory, consensus) = consensus().await;
        let repository = ReplicatedMetadataRepository::new(Arc::clone(&consensus));
        let record = bucket("objects");
        repository.create_bucket(&record).await.expect("create");
        repository
            .set_bucket_versioning(record.id, VersioningState::Enabled)
            .await
            .expect("versioning");

        let key = ObjectKey::new("note.txt").expect("key");
        let first = metadata_for(record.id, "note.txt", 3);
        let second = metadata_for(record.id, "note.txt", 5);
        repository.put_object(&first).await.expect("put");
        repository.put_object(&second).await.expect("put");

        assert!(
            repository
                .get_object_version(record.id, &key, first.version_id)
                .await
                .expect("read")
                .is_some()
        );
        assert_eq!(
            repository
                .list_objects(record_store_metadata::ListObjectsRequest {
                    bucket_id: record.id,
                    prefix: String::new(),
                    start_after: None,
                    limit: 10,
                })
                .await
                .expect("list")
                .objects
                .len(),
            1
        );

        repository
            .delete_object(
                record.id,
                &key,
                record_store_metadata::NewDeleteMarker::generate(),
            )
            .await
            .expect("delete");
        assert!(
            repository
                .get_object(record.id, &key)
                .await
                .expect("read")
                .is_none()
        );

        let versions = repository
            .list_object_versions(record_store_metadata::ListObjectVersionsRequest {
                bucket_id: record.id,
                prefix: String::new(),
                key_marker: None,
                version_id_marker: None,
                limit: 10,
            })
            .await
            .expect("list versions");
        assert_eq!(versions.versions.len(), 3, "history survives replication");

        repository
            .delete_object_version(record.id, &key, first.version_id)
            .await
            .expect("delete version");
    }

    /// Multipart state is replicated so an upload survives a leader change; a
    /// node that lost the upload could never complete or abort it.
    #[tokio::test]
    async fn multipart_state_replicates_end_to_end() {
        let (_directory, consensus) = consensus().await;
        let repository = ReplicatedMetadataRepository::new(Arc::clone(&consensus));
        let record = bucket("multipart");
        repository.create_bucket(&record).await.expect("create");

        let upload = record_store_core::MultipartUpload {
            id: record_store_core::UploadId::new(),
            bucket_id: record.id,
            key: ObjectKey::new("big.bin").expect("key"),
            content_type: None,
            custom_metadata: Default::default(),
            initiated_at: Utc::now(),
            state: record_store_core::MultipartUploadState::Active,
        };
        repository
            .create_multipart_upload(&upload)
            .await
            .expect("create upload");
        assert!(
            repository
                .get_multipart_upload(upload.id)
                .await
                .expect("read")
                .is_some()
        );

        let part = record_store_core::UploadedPart {
            upload_id: upload.id,
            number: record_store_core::PartNumber::new(1).expect("part number"),
            object_id: record_store_core::ObjectId::new(),
            size: 64,
            checksum: record_store_core::Checksum::sha256([1_u8; 32]),
            payload_format: record_store_core::PayloadFormat::Plaintext,
            etag: record_store_core::ETag::from_md5([1_u8; 16]),
            modified_at: Utc::now(),
        };
        repository
            .put_multipart_part(&part)
            .await
            .expect("put part");
        assert_eq!(
            repository
                .list_multipart_parts(upload.id, None, 10)
                .await
                .expect("list parts")
                .len(),
            1
        );
        assert_eq!(
            repository
                .list_multipart_uploads(record_store_metadata::ListMultipartUploadsRequest {
                    bucket_id: record.id,
                    prefix: String::new(),
                    upload_id_marker: None,
                    limit: 10,
                })
                .await
                .expect("list uploads")
                .uploads
                .len(),
            1
        );

        repository
            .abort_multipart_upload(upload.id)
            .await
            .expect("abort");
        assert!(
            repository
                .get_multipart_upload(upload.id)
                .await
                .expect("read")
                .is_none()
        );
    }

    /// Lifecycle rules and payload accounting replicate as well, because a
    /// cluster that lost them would stop expiring data and stop collecting it.
    #[tokio::test]
    async fn lifecycle_rules_and_accounting_replicate() {
        let (_directory, consensus) = consensus().await;
        let repository = ReplicatedMetadataRepository::new(Arc::clone(&consensus));
        let record = bucket("lifecycle");
        repository.create_bucket(&record).await.expect("create");

        let rule = record_store_core::LifecycleRule {
            id: record_store_core::LifecycleRuleId::new(),
            bucket_id: record.id,
            prefix: "logs/".to_owned(),
            enabled: true,
            expiration: Some(record_store_core::ExpirationDays::new(7).expect("days")),
            noncurrent_version_expiration: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        repository
            .put_lifecycle_rule(&rule)
            .await
            .expect("put rule");
        assert_eq!(
            repository
                .list_lifecycle_rules(Some(record.id))
                .await
                .expect("list rules")
                .len(),
            1
        );
        repository
            .delete_lifecycle_rule(rule.id)
            .await
            .expect("delete rule");

        let stored = metadata_for(record.id, "logs/a", 16);
        repository.put_object(&stored).await.expect("put");
        assert!(
            repository
                .payload_referenced(stored.id)
                .await
                .expect("read")
        );
        assert!(
            !repository
                .list_payload_references(None, 10)
                .await
                .expect("references")
                .object_ids
                .is_empty()
        );
        assert!(
            repository
                .bucket_usage()
                .await
                .expect("usage")
                .contains_key(&record.id)
        );

        repository
            .delete_object(
                record.id,
                &stored.key,
                record_store_metadata::NewDeleteMarker::generate(),
            )
            .await
            .expect("delete");
        let pending = repository.pending_cleanup(10).await.expect("pending");
        assert!(pending.contains(&stored.id), "{pending:?}");
        repository
            .complete_cleanup(stored.id)
            .await
            .expect("complete cleanup");

        repository.check_ready().await.expect("ready");
    }

    /// Deleting a bucket through consensus has to be refused while it still
    /// holds objects, exactly as the local catalog would.
    #[tokio::test]
    async fn a_replicated_bucket_delete_respects_its_contents() {
        let (_directory, consensus) = consensus().await;
        let repository = ReplicatedMetadataRepository::new(Arc::clone(&consensus));
        let record = bucket("occupied");
        repository.create_bucket(&record).await.expect("create");
        let stored = metadata_for(record.id, "a.txt", 1);
        repository.put_object(&stored).await.expect("put");

        assert!(matches!(
            repository.delete_bucket(&record.name).await,
            Err(MetadataError::BucketNotEmpty)
        ));

        repository
            .delete_object(
                record.id,
                &stored.key,
                record_store_metadata::NewDeleteMarker::generate(),
            )
            .await
            .expect("delete object");
        repository
            .delete_bucket(&record.name)
            .await
            .expect("delete bucket");
    }
}
