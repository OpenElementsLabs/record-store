//! Consensus-backed catalog adapters.
//!
//! These adapters make replication invisible to the layers above: object
//! operations keep using [`MetadataRepository`], and cluster operations keep
//! using [`ClusterStore`], while writes travel through the consensus log and
//! reads are served from the locally applied state behind a read barrier.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oes_cluster::{
    ClusterCatalogError, ClusterCommand, ClusterConfig, ClusterIdentity, ClusterOutcome,
    ClusterTopology, ClusterUsage, JoinToken, NodeCredential, NodeRecord, PayloadPlacement,
    PlacementPage, RaftNodeId, ReplicaTask, TaskPage, Tombstone,
};
use oes_core::{
    Bucket, BucketId, BucketName, BucketQuota, ClusterOperationId, JoinTokenId, LifecycleRule,
    LifecycleRuleId, MultipartUpload, NodeCredentialId, NodeId, ObjectId, ObjectKey,
    ObjectMetadata, ObjectVersionRecord, PartNumber, ReplicaTaskId, StorageUsage, UploadId,
    UploadedPart, VersionId, VersioningState,
};
use oes_metadata::{
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
        std::collections::BTreeMap<oes_core::BucketId, oes_metadata::BucketUsageSummary>,
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
    ) -> Result<Option<oes_cluster::ClusterOperation>, ClusterCatalogError>;

    /// Returns recorded operations, newest first.
    async fn operations(
        &self,
        limit: usize,
    ) -> Result<Vec<oes_cluster::ClusterOperation>, ClusterCatalogError>;

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
    local: oes_cluster::ClusterCatalog,
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
    ) -> Result<Option<oes_cluster::ClusterOperation>, ClusterCatalogError> {
        self.local.operation(operation_id).await
    }

    async fn operations(
        &self,
        limit: usize,
    ) -> Result<Vec<oes_cluster::ClusterOperation>, ClusterCatalogError> {
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
    catalog: oes_cluster::ClusterCatalog,
}

impl LocalClusterStore {
    /// Wraps a local catalog.
    #[must_use]
    pub const fn new(catalog: oes_cluster::ClusterCatalog) -> Self {
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
    ) -> Result<Option<oes_cluster::ClusterOperation>, ClusterCatalogError> {
        self.catalog.operation(operation_id).await
    }

    async fn operations(
        &self,
        limit: usize,
    ) -> Result<Vec<oes_cluster::ClusterOperation>, ClusterCatalogError> {
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
