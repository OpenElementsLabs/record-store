//! The replicated object store.
//!
//! It implements the same [`ObjectStore`] contract as the single-node store, so
//! the S3 adapter, service layer, and management API are unchanged. What differs
//! is the commit protocol: bytes reach several nodes, each verifies what it
//! stored, and only then are object metadata and replica placement committed
//! together through consensus. An object never becomes visible before its
//! durability requirement is met.

use std::sync::Arc;

use chrono::Utc;
use futures_util::{StreamExt, TryStreamExt, stream};
use md5::{Digest as _, Md5};
use oes_cluster::{
    ClusterCommand, ObjectPlacementRequest, PayloadPlacement, Replica, ReplicaState,
    ReplicaTaskKind, ReplicaTaskPriority,
};
use oes_consensus::ClusterWrite;
use oes_core::{
    CoreError, ETag, MultipartUploadState, ObjectId, ObjectMetadata, ObjectVersionRecord,
    PayloadFormat, UploadedPart, VersionId,
};
use oes_metadata::{
    DeleteObjectResult, MetadataCommand, MetadataError, NewDeleteMarker, ObjectCommitResult,
};
use oes_storage::{
    CompleteMultipartRequest, DeleteObjectRequest, DeleteObjectVersionRequest, GetObjectRequest,
    GetObjectResult, GetObjectVersionRequest, HeadObjectRequest, ObjectStore,
    PutMultipartPartRequest, PutObjectRequest, PutObjectResult, StorageError, StorageInspection,
    StorageRepairRequest, StorageRepairResult, StorageStatus, UploadStream, VerifyObjectRequest,
    upload_stream,
};
use tracing::{info, warn};

use crate::{
    context::ClusterContext,
    read::open_replica,
    write::{ReplicationOutcome, WriteSettings, replicate, rollback},
};

/// Node-local tuning for the replicated store.
#[derive(Debug, Clone, Copy)]
pub struct DistributedSettings {
    /// Bounds on one fan-out write.
    pub write: WriteSettings,
    /// Physical representation this node writes for new payloads.
    pub payload_format: PayloadFormat,
}

impl DistributedSettings {
    /// Creates settings for a node with the given at-rest representation.
    #[must_use]
    pub fn new(payload_format: PayloadFormat) -> Self {
        Self {
            write: WriteSettings::default(),
            payload_format,
        }
    }
}

/// The replicated object store.
pub struct DistributedObjectStore {
    context: Arc<ClusterContext>,
    settings: DistributedSettings,
}

impl DistributedObjectStore {
    /// Creates the store for one node.
    #[must_use]
    pub const fn new(context: Arc<ClusterContext>, settings: DistributedSettings) -> Self {
        Self { context, settings }
    }

    /// Returns the shared cluster context.
    #[must_use]
    pub fn context(&self) -> Arc<ClusterContext> {
        Arc::clone(&self.context)
    }

    /// Streams a payload to its planned replicas and returns what became durable.
    async fn stream_payload(
        &self,
        object_id: ObjectId,
        size_hint: Option<u64>,
        body: UploadStream,
    ) -> Result<(ReplicationOutcome, PayloadPlacement), StorageError> {
        let config = self.context.config().await?;
        let topology = self.context.topology().await?;
        let request = ObjectPlacementRequest::new(
            object_id,
            config.replication_factor,
            config.required_acknowledgements(),
            self.context.default_storage_class(),
        )
        .with_size_hint(size_hint)
        .with_preferred_node(Some(self.context.node_id));
        let plan = self
            .context
            .placement
            .place(&request, &topology)
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?;
        if !plan.fully_replicated() && !config.allow_degraded_writes {
            return Err(StorageError::DurabilityNotMet {
                required: config.replication_factor,
                achieved: u8::try_from(plan.targets.len()).unwrap_or(u8::MAX),
                detail: "the cluster cannot currently place the configured replica count and \
                         degraded writes are disabled"
                    .to_owned(),
            });
        }

        // The operation identity is derived from the payload so a retried
        // transfer is recognized by the destination rather than duplicated.
        let operation_id = format!("put-{}", object_id.as_uuid().simple());
        let outcome = replicate(
            &self.context,
            object_id,
            &operation_id,
            &plan,
            body,
            self.settings.write,
        )
        .await?;

        let required = plan.required_acknowledgements;
        if outcome.acknowledgements() < required {
            rollback(&self.context, object_id, &outcome.durable).await;
            return Err(StorageError::DurabilityNotMet {
                required,
                achieved: outcome.acknowledgements(),
                detail: outcome.detail(),
            });
        }

        let now = Utc::now();
        let replicas = outcome
            .durable
            .iter()
            .map(|node_id| Replica::healthy(*node_id, outcome.size, outcome.checksum.clone(), now))
            .collect();
        let placement = PayloadPlacement::new(
            object_id,
            outcome.size,
            outcome.checksum.clone(),
            config.replication_factor,
            self.context.default_storage_class(),
            replicas,
            now,
        );
        Ok((outcome, placement))
    }

    /// Commits object metadata and replica placement in one atomic write.
    async fn commit_object(
        &self,
        metadata: &ObjectMetadata,
        placement: &PayloadPlacement,
    ) -> Result<ObjectCommitResult, StorageError> {
        let consensus = self.context.consensus.as_ref().ok_or_else(|| {
            StorageError::ClusterUnavailable("this node has no metadata consensus".to_owned())
        })?;
        let write = ClusterWrite::batch([
            ClusterWrite::metadata(MetadataCommand::PutObject {
                metadata: Box::new(metadata.clone()),
            }),
            ClusterWrite::cluster(ClusterCommand::PutPlacement {
                placement: Box::new(placement.clone()),
            }),
        ]);
        let response = consensus.write(write).await.map_err(|error| match error {
            oes_consensus::ConsensusError::Rejected(rejection) => {
                StorageError::Metadata(rejection.into_metadata_error())
            }
            other => StorageError::ClusterUnavailable(other.to_string()),
        })?;
        let responses = response
            .into_batch()
            .map_err(|rejection| StorageError::Metadata(rejection.into_metadata_error()))?;
        let first = responses
            .into_iter()
            .next()
            .ok_or(StorageError::InconsistentState)?;
        first
            .into_metadata()
            .map_err(StorageError::Metadata)?
            .into_object_commit()
            .map_err(StorageError::Metadata)
    }

    /// Retires payloads that an object commit made unreachable.
    async fn retire_payloads(&self, payloads: &[ObjectMetadata]) {
        for previous in payloads {
            self.retire_payload(previous.id).await;
        }
    }

    async fn retire_payload(&self, object_id: ObjectId) {
        // Deleting placement records the tombstone that stops an offline node
        // from resurrecting the payload when it returns.
        if let Err(error) = self
            .context
            .commit(ClusterWrite::cluster(ClusterCommand::DeletePlacement {
                object_id,
                at: Utc::now(),
            }))
            .await
        {
            warn!(%object_id, %error, "could not record a payload tombstone");
        }
    }

    /// Queues repair when a committed object is below its desired replica count.
    async fn queue_replication_gap(&self, placement: &PayloadPlacement) {
        let healthy = u32::try_from(placement.replicas.len()).unwrap_or(0);
        let desired = u32::from(placement.desired_replicas);
        if healthy >= desired {
            return;
        }
        let task = oes_cluster::ReplicaTask::queued(
            placement.object_id,
            ReplicaTaskKind::Repair,
            ReplicaTaskPriority::classify(ReplicaTaskKind::Repair, healthy, desired),
            placement.size,
            Utc::now(),
        );
        if let Err(error) = self
            .context
            .commit(ClusterWrite::cluster(ClusterCommand::EnqueueTask {
                task: Box::new(task),
            }))
            .await
        {
            warn!(
                object = %placement.object_id,
                %error,
                "could not queue repair for an under-replicated object"
            );
        } else {
            info!(
                object = %placement.object_id,
                healthy,
                desired,
                "committed object is under-replicated and queued for repair"
            );
        }
    }

    async fn metadata_for(
        &self,
        bucket_id: oes_core::BucketId,
        key: &oes_core::ObjectKey,
    ) -> Result<ObjectMetadata, StorageError> {
        self.context
            .metadata
            .get_object(bucket_id, key)
            .await?
            .ok_or(StorageError::ObjectNotFound)
    }

    async fn open_object(
        &self,
        metadata: ObjectMetadata,
        range: Option<oes_core::ByteRange>,
    ) -> Result<GetObjectResult, StorageError> {
        let read = open_replica(
            &self.context,
            metadata.id,
            metadata.size,
            &metadata.checksum,
            metadata.payload_format,
            range,
        )
        .await?;
        Ok(GetObjectResult {
            metadata,
            range: read.range,
            body: read.body,
        })
    }
}

#[async_trait::async_trait]
impl ObjectStore for DistributedObjectStore {
    async fn put(&self, request: PutObjectRequest) -> Result<PutObjectResult, StorageError> {
        if self
            .context
            .metadata
            .get_bucket(request.bucket_id)
            .await?
            .is_none()
        {
            return Err(StorageError::BucketNotFound);
        }
        let object_id = request.object_id.unwrap_or_default();
        let (outcome, placement) = self.stream_payload(object_id, None, request.body).await?;

        if let Some(expected) = request.expected_checksum
            && expected != outcome.checksum
        {
            rollback(&self.context, object_id, &outcome.durable).await;
            return Err(StorageError::ChecksumMismatch {
                expected,
                actual: outcome.checksum,
            });
        }

        let now = Utc::now();
        let metadata = ObjectMetadata {
            id: object_id,
            bucket_id: request.bucket_id,
            key: request.key,
            version_id: VersionId::new(),
            size: outcome.size,
            checksum: outcome.checksum.clone(),
            payload_format: self.settings.payload_format,
            etag: request.protocol_etag.unwrap_or(outcome.etag),
            content_type: request.content_type,
            custom_metadata: request.custom_metadata,
            created_at: now,
            modified_at: now,
        };
        let commit = match self.commit_object(&metadata, &placement).await {
            Ok(commit) => commit,
            Err(error) => {
                // Nothing is visible, so the replicas that were written must be
                // released rather than left as silent garbage.
                rollback(&self.context, object_id, &outcome.durable).await;
                return Err(error);
            }
        };
        self.retire_payloads(&commit.cleanup).await;
        self.queue_replication_gap(&placement).await;
        Ok(PutObjectResult { metadata })
    }

    async fn put_multipart_part(
        &self,
        request: PutMultipartPartRequest,
    ) -> Result<UploadedPart, StorageError> {
        let upload = self
            .context
            .metadata
            .get_multipart_upload(request.upload_id)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        if upload.state != MultipartUploadState::Active {
            return Err(StorageError::Metadata(
                MetadataError::MultipartStateConflict,
            ));
        }
        let object_id = ObjectId::new();
        // Parts are replicated exactly like completed objects. That costs
        // temporary space but means an in-progress upload survives the loss of
        // the node that received it, and it keeps one durability story instead
        // of two.
        let (outcome, placement) = self.stream_payload(object_id, None, request.body).await?;
        if let Some(expected) = request.expected_checksum
            && expected != outcome.checksum
        {
            rollback(&self.context, object_id, &outcome.durable).await;
            return Err(StorageError::ChecksumMismatch {
                expected,
                actual: outcome.checksum,
            });
        }
        let part = UploadedPart {
            upload_id: request.upload_id,
            number: request.number,
            object_id,
            size: outcome.size,
            checksum: outcome.checksum.clone(),
            payload_format: self.settings.payload_format,
            etag: outcome.etag,
            modified_at: Utc::now(),
        };
        let consensus = self.context.consensus.as_ref().ok_or_else(|| {
            StorageError::ClusterUnavailable("this node has no metadata consensus".to_owned())
        })?;
        let response = consensus
            .write(ClusterWrite::batch([
                ClusterWrite::metadata(MetadataCommand::PutMultipartPart {
                    part: Box::new(part.clone()),
                }),
                ClusterWrite::cluster(ClusterCommand::PutPlacement {
                    placement: Box::new(placement.clone()),
                }),
            ]))
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                rollback(&self.context, object_id, &outcome.durable).await;
                return Err(match error {
                    oes_consensus::ConsensusError::Rejected(rejection) => {
                        StorageError::Metadata(rejection.into_metadata_error())
                    }
                    other => StorageError::ClusterUnavailable(other.to_string()),
                });
            }
        };
        let responses = match response.into_batch() {
            Ok(responses) => responses,
            Err(rejection) => {
                rollback(&self.context, object_id, &outcome.durable).await;
                return Err(StorageError::Metadata(rejection.into_metadata_error()));
            }
        };
        let replaced = responses
            .into_iter()
            .next()
            .ok_or(StorageError::InconsistentState)?
            .into_metadata()
            .map_err(StorageError::Metadata)?
            .into_replaced_part()
            .map_err(StorageError::Metadata)?;
        if let Some(previous) = replaced {
            self.retire_payload(previous.object_id).await;
        }
        self.queue_replication_gap(&placement).await;
        Ok(part)
    }

    async fn complete_multipart(
        &self,
        request: CompleteMultipartRequest,
    ) -> Result<PutObjectResult, StorageError> {
        if request.parts.is_empty() {
            return Err(StorageError::InvalidRequest(CoreError::InvalidPartNumber(
                "completion manifest must not be empty".into(),
            )));
        }
        let persisted = self
            .context
            .metadata
            .get_multipart_upload(request.upload.id)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        if persisted.bucket_id != request.upload.bucket_id || persisted.key != request.upload.key {
            return Err(StorageError::ObjectNotFound);
        }
        // The preallocated payload identifier is what makes completion
        // crash-recoverable: a retry after a failure reuses it instead of
        // creating a second object.
        let object_id = match persisted.state {
            MultipartUploadState::Active => {
                let object_id = ObjectId::new();
                self.context
                    .metadata
                    .begin_multipart_completion(persisted.id, object_id)
                    .await?;
                object_id
            }
            MultipartUploadState::Completing { object_id } => object_id,
        };
        if let Some(metadata) = self
            .context
            .metadata
            .get_object(persisted.bucket_id, &persisted.key)
            .await?
            .filter(|metadata| metadata.id == object_id)
        {
            let cleanup = self
                .context
                .metadata
                .finish_multipart_upload(persisted.id)
                .await?;
            for part in cleanup.parts {
                self.retire_payload(part.object_id).await;
            }
            return Ok(PutObjectResult { metadata });
        }

        let mut multipart_md5 = Md5::new();
        for part in &request.parts {
            let digest = hex::decode(part.etag.as_str()).map_err(|_| {
                StorageError::InvalidRequest(CoreError::InvalidETag(
                    "multipart part ETag is not an MD5 digest".into(),
                ))
            })?;
            if digest.len() != 16 {
                return Err(StorageError::InvalidRequest(CoreError::InvalidETag(
                    "multipart part ETag is not an MD5 digest".into(),
                )));
            }
            multipart_md5.update(digest);
        }
        let protocol_etag = ETag::new(format!(
            "{}-{}",
            hex::encode(multipart_md5.finalize()),
            request.parts.len()
        ))?;
        let total: u64 = request
            .parts
            .iter()
            .try_fold(0_u64, |sum, part| sum.checked_add(part.size))
            .ok_or_else(|| {
                StorageError::InvalidRequest(CoreError::InvalidPartNumber(
                    "completed object exceeds the addressable size".into(),
                ))
            })?;

        let context = Arc::clone(&self.context);
        let parts = request.parts.clone();
        let body = stream::iter(parts)
            .then(move |part| {
                let context = Arc::clone(&context);
                async move {
                    open_replica(
                        &context,
                        part.object_id,
                        part.size,
                        &part.checksum,
                        part.payload_format,
                        None,
                    )
                    .await
                    .map(|read| read.body.map_err(std::io::Error::other))
                    .map_err(std::io::Error::other)
                }
            })
            .try_flatten();

        let (outcome, placement) = self
            .stream_payload(object_id, Some(total), upload_stream(body))
            .await?;
        let now = Utc::now();
        let metadata = ObjectMetadata {
            id: object_id,
            bucket_id: persisted.bucket_id,
            key: persisted.key,
            version_id: VersionId::new(),
            size: outcome.size,
            checksum: outcome.checksum.clone(),
            payload_format: self.settings.payload_format,
            etag: protocol_etag,
            content_type: persisted.content_type,
            custom_metadata: persisted.custom_metadata,
            created_at: now,
            modified_at: now,
        };
        let commit = match self.commit_object(&metadata, &placement).await {
            Ok(commit) => commit,
            Err(error) => {
                rollback(&self.context, object_id, &outcome.durable).await;
                return Err(error);
            }
        };
        self.retire_payloads(&commit.cleanup).await;
        self.queue_replication_gap(&placement).await;
        let cleanup = self
            .context
            .metadata
            .finish_multipart_upload(persisted.id)
            .await?;
        for part in cleanup.parts {
            self.retire_payload(part.object_id).await;
        }
        Ok(PutObjectResult { metadata })
    }

    async fn get(&self, request: GetObjectRequest) -> Result<GetObjectResult, StorageError> {
        let metadata = self.metadata_for(request.bucket_id, &request.key).await?;
        self.open_object(metadata, request.range).await
    }

    async fn get_version(
        &self,
        request: GetObjectVersionRequest,
    ) -> Result<GetObjectResult, StorageError> {
        let record = self
            .context
            .metadata
            .get_object_version(request.bucket_id, &request.key, request.version_id)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        match record {
            ObjectVersionRecord::Object { metadata, .. } => {
                self.open_object(metadata, request.range).await
            }
            ObjectVersionRecord::DeleteMarker { marker, .. } => Err(StorageError::DeleteMarker {
                version_id: marker.version_id,
            }),
        }
    }

    async fn head(&self, request: HeadObjectRequest) -> Result<ObjectMetadata, StorageError> {
        self.metadata_for(request.bucket_id, &request.key).await
    }

    async fn delete(
        &self,
        request: DeleteObjectRequest,
    ) -> Result<DeleteObjectResult, StorageError> {
        let result = self
            .context
            .metadata
            .delete_object(request.bucket_id, &request.key, NewDeleteMarker::generate())
            .await?;
        if !result.previously_visible && result.delete_marker.is_none() {
            return Err(StorageError::ObjectNotFound);
        }
        self.retire_payloads(&result.cleanup).await;
        Ok(result)
    }

    async fn delete_version(
        &self,
        request: DeleteObjectVersionRequest,
    ) -> Result<(), StorageError> {
        let result = self
            .context
            .metadata
            .delete_object_version(request.bucket_id, &request.key, request.version_id)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        if let Some(metadata) = result.cleanup {
            self.retire_payload(metadata.id).await;
        }
        Ok(())
    }

    async fn verify(&self, request: VerifyObjectRequest) -> Result<ObjectMetadata, StorageError> {
        let metadata = self.metadata_for(request.bucket_id, &request.key).await?;
        let placement = self
            .context
            .placement_for(metadata.id)
            .await?
            .ok_or(StorageError::NoHealthyReplica)?;
        let mut verified = 0_u32;
        for replica in &placement.replicas {
            if replica.state != ReplicaState::Healthy {
                continue;
            }
            let outcome = if replica.node_id == self.context.node_id {
                self.context
                    .local
                    .verify_replica(
                        metadata.id,
                        metadata.size,
                        metadata.payload_format,
                        metadata.checksum.clone(),
                    )
                    .await
                    .map(|verification| (verification.present, verification.matches))
            } else {
                let target = self.context.target(replica.node_id).await?;
                self.context
                    .transport
                    .verify_replica(&target, metadata.id, metadata.size, &metadata.checksum)
                    .await
                    .map(|verification| (verification.present, verification.matches))
                    .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))
            };
            match outcome {
                Ok((true, true)) => verified += 1,
                Ok((present, _)) => {
                    let state = if present {
                        ReplicaState::Corrupt
                    } else {
                        ReplicaState::Missing
                    };
                    crate::read::report_damage(&self.context, &placement, replica.node_id, state)
                        .await;
                }
                Err(error) => {
                    warn!(
                        object = %metadata.id,
                        node = %replica.node_id,
                        %error,
                        "replica verification could not be completed"
                    );
                }
            }
        }
        if verified == 0 {
            return Err(StorageError::IntegrityMismatch);
        }
        Ok(metadata)
    }

    async fn status(&self) -> Result<StorageStatus, StorageError> {
        // Capacity is measured on this node's own filesystem rather than read
        // back from cluster metadata, which is only ever a record of what a node
        // previously reported.
        self.context.local.local_capacity().await
    }

    async fn check_ready(&self) -> Result<(), StorageError> {
        self.context
            .cluster
            .check_ready()
            .await
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?;
        self.context.metadata.check_ready().await?;
        Ok(())
    }

    async fn inspect(&self, maximum_entries: usize) -> Result<StorageInspection, StorageError> {
        self.context.inspect_local(maximum_entries).await
    }

    async fn repair(
        &self,
        request: StorageRepairRequest,
    ) -> Result<StorageRepairResult, StorageError> {
        let inspection = self.context.inspect_local(request.maximum_entries).await?;
        // Cluster mode never deletes bytes based on a local scan: authoritative
        // placement metadata and tombstones decide what may be removed, and the
        // collector acts on those.
        Ok(StorageRepairResult {
            inspection,
            removed_orphan_payloads: 0,
            dry_run: true,
        })
    }

    async fn cleanup_pending(&self, limit: usize) -> Result<usize, StorageError> {
        let pending = self.context.metadata.pending_cleanup(limit).await?;
        let mut completed = 0;
        for object_id in pending {
            self.retire_payload(object_id).await;
            if let Err(error) = self.context.metadata.complete_cleanup(object_id).await {
                warn!(%object_id, %error, "could not clear a payload cleanup record");
                continue;
            }
            completed += 1;
        }
        Ok(completed)
    }
}

impl ClusterContext {
    /// Compares this node's local payloads against authoritative placement.
    ///
    /// The result is diagnostic only. Nothing is deleted from a local scan,
    /// because a payload this node does not know about may simply belong to a
    /// commit that has not reached it yet.
    pub async fn inspect_local(
        &self,
        maximum_entries: usize,
    ) -> Result<StorageInspection, StorageError> {
        let limit = maximum_entries.clamp(1, 100_000);
        let payloads = self.local.list_local_payloads(None, limit).await?;
        let mut inspection = StorageInspection {
            data_payloads_scanned: u64::try_from(payloads.len()).unwrap_or(u64::MAX),
            ..StorageInspection::default()
        };
        let mut samples = Vec::new();
        for object_id in &payloads {
            let placement = self.placement_for(*object_id).await?;
            match placement {
                Some(placement) => {
                    inspection.metadata_payloads_scanned += 1;
                    if placement.replica(self.node_id).is_none() {
                        inspection.data_without_metadata += 1;
                        if samples.len() < 16 {
                            samples.push(*object_id);
                        }
                    }
                }
                None => {
                    inspection.data_without_metadata += 1;
                    if samples.len() < 16 {
                        samples.push(*object_id);
                    }
                }
            }
        }
        let expected = self
            .cluster
            .node_replicas(self.node_id, None, limit)
            .await
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?;
        let held: std::collections::BTreeSet<_> = payloads.into_iter().collect();
        let mut missing = Vec::new();
        for object_id in expected {
            if !held.contains(&object_id) {
                inspection.metadata_without_data += 1;
                if missing.len() < 16 {
                    missing.push(object_id);
                }
            }
        }
        inspection.truncated =
            inspection.data_payloads_scanned >= u64::try_from(limit).unwrap_or(u64::MAX);
        inspection.orphan_payload_samples = samples;
        inspection.missing_payload_samples = missing;
        Ok(inspection)
    }
}
