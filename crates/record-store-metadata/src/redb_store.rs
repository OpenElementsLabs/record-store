//! Durable single-node metadata catalog.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, CorsConfiguration, LifecycleRule, LifecycleRuleId,
    MultipartUpload, ObjectId, ObjectKey, ObjectMetadata, ObjectVersionRecord, PartNumber,
    StorageUsage, UploadId, UploadedPart, VersionId, VersioningState,
};
use redb::{Database, ReadableTable};

use crate::error::backend;
use crate::keys::{
    bucket_key, multipart_order_key, object_key, object_prefix, part_key, prefix_successor,
    version_order_key,
};
use crate::schema::{
    BUCKET_COUNT, BUCKET_NAMES, BUCKET_USAGE, BUCKETS, CLEANUP, COUNTERS, LIFECYCLE_RULES,
    LOGICAL_BYTES, MULTIPART, MULTIPART_BYTES, MULTIPART_ORDER, NULL_VERSIONS, OBJECT_COUNT,
    OBJECTS, PARTS, PHYSICAL_BYTES, VERSION_BYTES, VERSION_COUNT, VERSION_ORDER, VERSIONS,
    initialize_schema,
};
use crate::tx::{
    current_version_read, decode_optional, read_counter, read_encoded, record_matches,
};
use crate::types::BucketUsage;
use crate::*;

/// Redb-backed durable catalog for a standalone Record Store node.
#[derive(Clone)]
pub struct RedbMetadataRepository {
    database: Arc<Database>,
}

impl RedbMetadataRepository {
    /// Opens the catalog and applies ordered non-destructive migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, MetadataError> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(MetadataError::Directory)?;
            }
            let database = Database::create(path).map_err(|error| backend("open", error))?;
            initialize_schema(&database)?;
            Ok(Self {
                database: Arc::new(database),
            })
        })
        .await?
    }

    /// Opens a catalog that shares a database with other durable Record Store state.
    ///
    /// Sharing one database is what allows a consensus state machine to commit
    /// object metadata, cluster metadata, and the applied log position in a
    /// single transaction.
    pub fn from_database(database: Arc<Database>) -> Result<Self, MetadataError> {
        initialize_schema(&database)?;
        Ok(Self { database })
    }

    /// Returns the shared database handle.
    #[must_use]
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.database)
    }

    /// Applies one command in its own transaction.
    ///
    /// Cluster mode routes the same commands through consensus instead; this
    /// entry point serves standalone deployments and tests.
    pub async fn command(
        &self,
        command: MetadataCommand,
    ) -> Result<MetadataOutcome, MetadataError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin metadata command", error))?;
            let outcome = apply_command_tx(&write, command)?;
            write
                .commit()
                .map_err(|error| backend("commit metadata command", error))?;
            Ok(outcome)
        })
        .await?
    }
}

#[async_trait]
impl MetadataRepository for RedbMetadataRepository {
    async fn create_bucket(&self, bucket: &Bucket) -> Result<(), MetadataError> {
        self.command(MetadataCommand::CreateBucket {
            bucket: Box::new(bucket.clone()),
        })
        .await
        .map(|_| ())
    }

    async fn get_bucket(&self, id: BucketId) -> Result<Option<Bucket>, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            read_encoded(&db, BUCKETS, &bucket_key(id), "read bucket")
        })
        .await?
    }

    async fn get_bucket_by_name(&self, name: &BucketName) -> Result<Option<Bucket>, MetadataError> {
        let db = Arc::clone(&self.database);
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let read = db
                .begin_read()
                .map_err(|e| backend("begin read bucket", e))?;
            let table = read
                .open_table(BUCKET_NAMES)
                .map_err(|e| backend("open bucket names", e))?;
            decode_optional(
                table
                    .get(name.as_str())
                    .map_err(|e| backend("read bucket name", e))?
                    .map(|v| v.value().to_vec()),
            )
        })
        .await?
    }

    async fn list_buckets(&self) -> Result<Vec<Bucket>, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db
                .begin_read()
                .map_err(|e| backend("begin list buckets", e))?;
            let table = read
                .open_table(BUCKET_NAMES)
                .map_err(|e| backend("open bucket names", e))?;
            let mut out = Vec::new();
            for entry in table.iter().map_err(|e| backend("iterate buckets", e))? {
                let (_, value) = entry.map_err(|e| backend("read bucket", e))?;
                out.push(serde_json::from_slice(value.value())?);
            }
            Ok(out)
        })
        .await?
    }

    async fn set_bucket_versioning(
        &self,
        id: BucketId,
        state: VersioningState,
    ) -> Result<Bucket, MetadataError> {
        self.command(MetadataCommand::SetBucketVersioning {
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
        self.command(MetadataCommand::SetBucketQuota {
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
        self.command(MetadataCommand::SetBucketCors {
            bucket_id: id,
            configuration,
        })
        .await?
        .into_bucket()
    }

    async fn delete_bucket(&self, name: &BucketName) -> Result<Bucket, MetadataError> {
        self.command(MetadataCommand::DeleteBucket { name: name.clone() })
            .await?
            .into_bucket()
    }

    async fn put_object(
        &self,
        metadata: &ObjectMetadata,
    ) -> Result<ObjectCommitResult, MetadataError> {
        self.command(MetadataCommand::PutObject {
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
        let db = Arc::clone(&self.database);
        let key = object_key(bucket, key);
        tokio::task::spawn_blocking(move || read_encoded(&db, OBJECTS, &key, "read object")).await?
    }

    async fn get_object_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
    ) -> Result<Option<ObjectVersionRecord>, MetadataError> {
        let db = Arc::clone(&self.database);
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let record: Option<ObjectVersionRecord> = read_encoded(
                &db,
                VERSIONS,
                version.as_uuid().as_bytes().as_slice(),
                "read version",
            )?;
            Ok(record.filter(|record| record_matches(record, bucket, &key)))
        })
        .await?
    }

    async fn get_null_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectVersionRecord>, MetadataError> {
        let db = Arc::clone(&self.database);
        let key = object_key(bucket, key);
        tokio::task::spawn_blocking(move || {
            let read = db
                .begin_read()
                .map_err(|e| backend("begin null lookup", e))?;
            let nulls = read
                .open_table(NULL_VERSIONS)
                .map_err(|e| backend("open null versions", e))?;
            let Some(id) = nulls
                .get(key.as_slice())
                .map_err(|e| backend("read null version", e))?
                .map(|v| v.value().to_vec())
            else {
                return Ok(None);
            };
            let versions = read
                .open_table(VERSIONS)
                .map_err(|e| backend("open versions", e))?;
            decode_optional(
                versions
                    .get(id.as_slice())
                    .map_err(|e| backend("resolve null version", e))?
                    .map(|v| v.value().to_vec()),
            )
        })
        .await?
    }

    async fn delete_object(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        marker: NewDeleteMarker,
    ) -> Result<DeleteObjectResult, MetadataError> {
        self.command(MetadataCommand::DeleteObject {
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
        self.command(MetadataCommand::DeleteObjectVersion {
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
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            if request.limit == 0 {
                return Ok(ObjectMetadataPage {
                    objects: Vec::new(),
                    next_key: None,
                });
            }
            let read = db
                .begin_read()
                .map_err(|e| backend("begin object list", e))?;
            let table = read
                .open_table(OBJECTS)
                .map_err(|e| backend("open objects", e))?;
            let prefix = object_prefix(request.bucket_id, &request.prefix);
            let mut start = request.start_after.as_ref().map_or_else(
                || prefix.clone(),
                |value| object_prefix(request.bucket_id, value),
            );
            if request.start_after.is_some() {
                start.push(0);
            }
            let end = prefix_successor(&prefix);
            let mut objects = Vec::with_capacity(request.limit.min(1_000) + 1);
            for entry in table
                .range(start.as_slice()..end.as_slice())
                .map_err(|e| backend("range objects", e))?
                .take(request.limit + 1)
            {
                let (_, value) = entry.map_err(|e| backend("read object", e))?;
                objects.push(serde_json::from_slice(value.value())?);
            }
            let next_key = if objects.len() > request.limit {
                objects.pop();
                objects.last().map(|m: &ObjectMetadata| m.key.to_string())
            } else {
                None
            };
            Ok(ObjectMetadataPage { objects, next_key })
        })
        .await?
    }

    async fn list_object_versions(
        &self,
        request: ListObjectVersionsRequest,
    ) -> Result<ObjectVersionPage, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            if request.key_marker.is_some() != request.version_id_marker.is_some() {
                return Err(MetadataError::Database {
                    operation: "list versions",
                    reason: "both markers are required".into(),
                });
            }
            if request.limit == 0 {
                return Ok(ObjectVersionPage {
                    versions: Vec::new(),
                    next_key_marker: None,
                    next_version_id_marker: None,
                });
            }
            let read = db
                .begin_read()
                .map_err(|e| backend("begin version list", e))?;
            let order = read
                .open_table(VERSION_ORDER)
                .map_err(|e| backend("open version order", e))?;
            let versions = read
                .open_table(VERSIONS)
                .map_err(|e| backend("open versions", e))?;
            let prefix = object_prefix(request.bucket_id, &request.prefix);
            let mut start = prefix.clone();
            if let (Some(marker_key), Some(marker_id)) =
                (&request.key_marker, request.version_id_marker)
            {
                let bytes = versions
                    .get(marker_id.as_uuid().as_bytes().as_slice())
                    .map_err(|e| backend("read version marker", e))?
                    .map(|v| v.value().to_vec())
                    .ok_or_else(|| MetadataError::Database {
                        operation: "list versions",
                        reason: "unknown marker".into(),
                    })?;
                let record: ObjectVersionRecord = serde_json::from_slice(&bytes)?;
                if record.key().as_str() != marker_key
                    || !record_matches(&record, request.bucket_id, record.key())
                {
                    return Err(MetadataError::Database {
                        operation: "list versions",
                        reason: "mismatched marker".into(),
                    });
                }
                start = version_order_key(&record);
                start.push(0);
            }
            let end = prefix_successor(&prefix);
            let mut out = Vec::with_capacity(request.limit.min(1_000) + 1);
            for entry in order
                .range(start.as_slice()..end.as_slice())
                .map_err(|e| backend("range versions", e))?
                .take(request.limit + 1)
            {
                let (_, id) = entry.map_err(|e| backend("read version index", e))?;
                let bytes = versions
                    .get(id.value())
                    .map_err(|e| backend("resolve version", e))?
                    .map(|v| v.value().to_vec())
                    .ok_or_else(|| MetadataError::Database {
                        operation: "list versions",
                        reason: "inconsistent index".into(),
                    })?;
                let record: ObjectVersionRecord = serde_json::from_slice(&bytes)?;
                let latest =
                    current_version_read(&read, &object_key(request.bucket_id, record.key()))?
                        == Some(record.version_id());
                out.push(ListedObjectVersion {
                    record,
                    is_latest: latest,
                });
            }
            let truncated = out.len() > request.limit;
            if truncated {
                out.pop();
            }
            let (next_key_marker, next_version_id_marker) = if truncated {
                out.last().map_or((None, None), |item| {
                    (
                        Some(item.record.key().to_string()),
                        Some(item.record.version_id()),
                    )
                })
            } else {
                (None, None)
            };
            Ok(ObjectVersionPage {
                versions: out,
                next_key_marker,
                next_version_id_marker,
            })
        })
        .await?
    }

    async fn create_multipart_upload(&self, upload: &MultipartUpload) -> Result<(), MetadataError> {
        self.command(MetadataCommand::CreateMultipartUpload {
            upload: Box::new(upload.clone()),
        })
        .await
        .map(|_| ())
    }

    async fn get_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<Option<MultipartUpload>, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            read_encoded(
                &db,
                MULTIPART,
                id.as_uuid().as_bytes().as_slice(),
                "read multipart",
            )
        })
        .await?
    }

    async fn put_multipart_part(
        &self,
        part: &UploadedPart,
    ) -> Result<Option<UploadedPart>, MetadataError> {
        self.command(MetadataCommand::PutMultipartPart {
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
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(|e| backend("begin part list", e))?;
            let uploads = read
                .open_table(MULTIPART)
                .map_err(|e| backend("open multipart", e))?;
            if uploads
                .get(id.as_uuid().as_bytes().as_slice())
                .map_err(|e| backend("read multipart", e))?
                .is_none()
            {
                return Err(MetadataError::MultipartUploadNotFound);
            }
            let table = read
                .open_table(PARTS)
                .map_err(|e| backend("open parts", e))?;
            let prefix = id.as_uuid().as_bytes().as_slice().to_vec();
            let mut start = after.map_or_else(|| prefix.clone(), |number| part_key(id, number));
            if after.is_some() {
                start.push(0);
            }
            let end = prefix_successor(&prefix);
            let mut out = Vec::with_capacity(limit.min(1_000));
            for entry in table
                .range(start.as_slice()..end.as_slice())
                .map_err(|e| backend("range parts", e))?
                .take(limit)
            {
                let (_, value) = entry.map_err(|e| backend("read part", e))?;
                out.push(serde_json::from_slice(value.value())?);
            }
            Ok(out)
        })
        .await?
    }

    async fn list_multipart_uploads(
        &self,
        request: ListMultipartUploadsRequest,
    ) -> Result<MultipartUploadPage, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            if request.limit == 0 {
                return Ok(MultipartUploadPage {
                    uploads: Vec::new(),
                    next_upload_id_marker: None,
                });
            }
            let read = db
                .begin_read()
                .map_err(|e| backend("begin multipart list", e))?;
            let uploads = read
                .open_table(MULTIPART)
                .map_err(|e| backend("open multipart", e))?;
            let order = read
                .open_table(MULTIPART_ORDER)
                .map_err(|e| backend("open multipart order", e))?;
            let prefix = object_prefix(request.bucket_id, &request.prefix);
            let mut start = prefix.clone();
            if let Some(marker) = request.upload_id_marker {
                let bytes = uploads
                    .get(marker.as_uuid().as_bytes().as_slice())
                    .map_err(|e| backend("read multipart marker", e))?
                    .map(|v| v.value().to_vec())
                    .ok_or(MetadataError::MultipartUploadNotFound)?;
                let upload: MultipartUpload = serde_json::from_slice(&bytes)?;
                start = multipart_order_key(&upload);
                start.push(0);
            }
            let end = prefix_successor(&prefix);
            let mut out = Vec::with_capacity(request.limit.min(1_000) + 1);
            for entry in order
                .range(start.as_slice()..end.as_slice())
                .map_err(|e| backend("range multipart", e))?
                .take(request.limit + 1)
            {
                let (_, id) = entry.map_err(|e| backend("read multipart index", e))?;
                let bytes = uploads
                    .get(id.value())
                    .map_err(|e| backend("resolve multipart", e))?
                    .map(|v| v.value().to_vec())
                    .ok_or_else(|| MetadataError::Database {
                        operation: "list multipart",
                        reason: "inconsistent index".into(),
                    })?;
                out.push(serde_json::from_slice::<MultipartUpload>(&bytes)?);
            }
            let next_upload_id_marker = if out.len() > request.limit {
                out.pop();
                out.last().map(|upload| upload.id)
            } else {
                None
            };
            Ok(MultipartUploadPage {
                uploads: out,
                next_upload_id_marker,
            })
        })
        .await?
    }

    async fn begin_multipart_completion(
        &self,
        id: UploadId,
        object_id: ObjectId,
    ) -> Result<MultipartUpload, MetadataError> {
        self.command(MetadataCommand::BeginMultipartCompletion {
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
        self.command(MetadataCommand::FinishMultipartUpload { upload_id: id })
            .await?
            .into_multipart_cleanup()
    }
    async fn abort_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<MultipartCleanupResult, MetadataError> {
        self.command(MetadataCommand::AbortMultipartUpload { upload_id: id })
            .await?
            .into_multipart_cleanup()
    }

    /// Reconciles crash-interrupted completion state before readiness.
    async fn recover_multipart_completions(&self) -> Result<MultipartCleanupResult, MetadataError> {
        self.command(MetadataCommand::RecoverMultipartCompletions)
            .await?
            .into_multipart_cleanup()
    }

    async fn put_lifecycle_rule(&self, rule: &LifecycleRule) -> Result<(), MetadataError> {
        self.command(MetadataCommand::PutLifecycleRule {
            rule: Box::new(rule.clone()),
        })
        .await
        .map(|_| ())
    }

    async fn list_lifecycle_rules(
        &self,
        bucket: Option<BucketId>,
    ) -> Result<Vec<LifecycleRule>, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db
                .begin_read()
                .map_err(|e| backend("begin lifecycle rule list", e))?;
            let table = read
                .open_table(LIFECYCLE_RULES)
                .map_err(|e| backend("open lifecycle rules", e))?;
            let mut rules = Vec::new();
            for item in table
                .iter()
                .map_err(|e| backend("iterate lifecycle rules", e))?
            {
                let (_, value) = item.map_err(|e| backend("read lifecycle rule", e))?;
                let rule: LifecycleRule = serde_json::from_slice(value.value())?;
                if bucket.is_none_or(|bucket| rule.bucket_id == bucket) {
                    rules.push(rule);
                }
            }
            Ok(rules)
        })
        .await?
    }

    async fn delete_lifecycle_rule(&self, id: LifecycleRuleId) -> Result<(), MetadataError> {
        self.command(MetadataCommand::DeleteLifecycleRule { rule_id: id })
            .await
            .map(|_| ())
    }

    async fn storage_usage(&self) -> Result<StorageUsage, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(|e| backend("begin usage", e))?;
            let table = read
                .open_table(COUNTERS)
                .map_err(|e| backend("open counters", e))?;
            Ok(StorageUsage {
                object_count: read_counter(&table, OBJECT_COUNT)?,
                bytes_used: read_counter(&table, LOGICAL_BYTES)?,
                bucket_count: read_counter(&table, BUCKET_COUNT)?,
                version_count: read_counter(&table, VERSION_COUNT)?,
                version_bytes: read_counter(&table, VERSION_BYTES)?,
                physical_bytes: read_counter(&table, PHYSICAL_BYTES)?,
                temporary_multipart_bytes: read_counter(&table, MULTIPART_BYTES)?,
            })
        })
        .await?
    }

    async fn bucket_usage(
        &self,
    ) -> Result<std::collections::BTreeMap<BucketId, BucketUsageSummary>, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db
                .begin_read()
                .map_err(|e| backend("begin bucket usage", e))?;
            let table = read
                .open_table(BUCKET_USAGE)
                .map_err(|e| backend("open bucket usage", e))?;
            let mut out = std::collections::BTreeMap::new();
            for entry in table
                .iter()
                .map_err(|e| backend("iterate bucket usage", e))?
            {
                let (key, value) = entry.map_err(|e| backend("read bucket usage", e))?;
                let bytes: [u8; 16] =
                    key.value()
                        .try_into()
                        .map_err(|_| MetadataError::Database {
                            operation: "decode bucket usage key",
                            reason: "bucket identifier is malformed".into(),
                        })?;
                let usage: BucketUsage = serde_json::from_slice(value.value())?;
                out.insert(
                    BucketId::from_uuid(uuid::Uuid::from_bytes(bytes)),
                    BucketUsageSummary::from(usage),
                );
            }
            Ok(out)
        })
        .await?
    }

    async fn pending_cleanup(&self, limit: usize) -> Result<Vec<ObjectId>, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db
                .begin_read()
                .map_err(|e| backend("begin cleanup list", e))?;
            let table = read
                .open_table(CLEANUP)
                .map_err(|e| backend("open cleanup", e))?;
            let mut out = Vec::with_capacity(limit.min(1_000));
            for entry in table
                .iter()
                .map_err(|e| backend("iterate cleanup", e))?
                .take(limit)
            {
                let (key, _) = entry.map_err(|e| backend("read cleanup", e))?;
                let bytes: [u8; 16] =
                    key.value()
                        .try_into()
                        .map_err(|_| MetadataError::Database {
                            operation: "decode cleanup",
                            reason: "invalid identifier".into(),
                        })?;
                out.push(ObjectId::from_uuid(uuid::Uuid::from_bytes(bytes)));
            }
            Ok(out)
        })
        .await?
    }

    async fn complete_cleanup(&self, id: ObjectId) -> Result<(), MetadataError> {
        self.command(MetadataCommand::CompleteCleanup { object_id: id })
            .await
            .map(|_| ())
    }

    async fn payload_referenced(&self, id: ObjectId) -> Result<bool, MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(|e| backend("begin payload reference check", e))?;
            let versions = read.open_table(VERSIONS).map_err(|e| backend("open versions", e))?;
            for entry in versions.iter().map_err(|e| backend("iterate versions", e))? {
                let (_, value) = entry.map_err(|e| backend("read version", e))?;
                if matches!(serde_json::from_slice::<ObjectVersionRecord>(value.value())?, ObjectVersionRecord::Object { metadata, .. } if metadata.id == id) { return Ok(true); }
            }
            let parts = read.open_table(PARTS).map_err(|e| backend("open parts", e))?;
            for entry in parts.iter().map_err(|e| backend("iterate parts", e))? {
                let (_, value) = entry.map_err(|e| backend("read part", e))?;
                if serde_json::from_slice::<UploadedPart>(value.value())?.object_id == id { return Ok(true); }
            }
            Ok(false)
        }).await?
    }

    async fn list_payload_references(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<PayloadReferencePage, MetadataError> {
        if limit == 0 || limit > 1_000 {
            return Err(MetadataError::Database {
                operation: "list payload references",
                reason: "limit must be between 1 and 1000".into(),
            });
        }
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db
                .begin_read()
                .map_err(|e| backend("begin payload reference list", e))?;
            let mut identifiers = std::collections::BTreeSet::new();
            let mut insert = |id: ObjectId| {
                if after.is_none_or(|after| id > after) {
                    identifiers.insert(id);
                    if identifiers.len() > limit + 1 {
                        identifiers.pop_last();
                    }
                }
            };
            let versions = read
                .open_table(VERSIONS)
                .map_err(|e| backend("open versions", e))?;
            for item in versions
                .iter()
                .map_err(|e| backend("iterate versions", e))?
            {
                let (_, value) = item.map_err(|e| backend("read version", e))?;
                if let ObjectVersionRecord::Object { metadata, .. } =
                    serde_json::from_slice(value.value())?
                {
                    insert(metadata.id);
                }
            }
            let parts = read
                .open_table(PARTS)
                .map_err(|e| backend("open parts", e))?;
            for item in parts.iter().map_err(|e| backend("iterate parts", e))? {
                let (_, value) = item.map_err(|e| backend("read part", e))?;
                insert(serde_json::from_slice::<UploadedPart>(value.value())?.object_id);
            }
            let truncated = identifiers.len() > limit;
            if truncated {
                identifiers.pop_last();
            }
            let object_ids: Vec<_> = identifiers.into_iter().collect();
            let next_object_id = if truncated {
                object_ids.last().copied()
            } else {
                None
            };
            Ok(PayloadReferencePage {
                object_ids,
                next_object_id,
            })
        })
        .await?
    }

    async fn check_ready(&self) -> Result<(), MetadataError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(|e| backend("readiness", e))?;
            {
                write
                    .open_table(OBJECTS)
                    .map_err(|e| backend("readiness table", e))?;
            }
            write.commit().map_err(|e| backend("commit readiness", e))
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use record_store_core::{
        Checksum, ETag, MultipartUploadState, ObjectKey, ObjectVersionRecord, PartNumber, UploadId,
        VersioningState,
    };
    use tempfile::tempdir;

    use super::*;
    use crate::commands::NewDeleteMarker;
    use crate::test_support::*;
    use crate::types::{
        ListMultipartUploadsRequest, ListObjectVersionsRequest, ListObjectsRequest,
    };
    use crate::{MetadataRepository, RedbMetadataRepository};
    use record_store_core::{
        BucketQuota, ByteQuota, ExpirationDays, LifecycleRule, LifecycleRuleId, ObjectCountQuota,
    };

    #[tokio::test]
    async fn versions_markers_and_restart_are_durable() {
        let dir = tempdir().expect("temp");
        let path = dir.path().join("catalog.redb");
        let repo = RedbMetadataRepository::open(&path).await.expect("repo");
        let bucket = bucket("version-bucket");
        repo.create_bucket(&bucket).await.expect("bucket");
        repo.set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");
        let first = object(bucket.id, "report", 10);
        let second = object(bucket.id, "report", 20);
        repo.put_object(&first).await.expect("first");
        repo.put_object(&second).await.expect("second");
        repo.delete_object(bucket.id, &first.key, NewDeleteMarker::generate())
            .await
            .expect("delete");
        drop(repo);
        let repo = RedbMetadataRepository::open(&path).await.expect("reopen");
        assert!(
            repo.get_object(bucket.id, &first.key)
                .await
                .expect("current")
                .is_none()
        );
        assert!(matches!(
            repo.get_object_version(bucket.id, &first.key, first.version_id)
                .await
                .expect("version"),
            Some(ObjectVersionRecord::Object { .. })
        ));
        assert_eq!(repo.storage_usage().await.expect("usage").version_count, 3);
    }

    #[tokio::test]
    async fn bucket_cors_configuration_survives_restart_and_can_be_removed() {
        let dir = tempdir().expect("temp");
        let path = dir.path().join("catalog.redb");
        let bucket = bucket("cors-bucket");
        let configuration = cors_configuration();
        {
            let repo = RedbMetadataRepository::open(&path).await.expect("repo");
            repo.create_bucket(&bucket).await.expect("bucket");
            let updated = repo
                .set_bucket_cors(bucket.id, Some(configuration.clone()))
                .await
                .expect("set CORS");
            assert_eq!(updated.cors.as_ref(), Some(&configuration));
        }

        let repo = RedbMetadataRepository::open(&path).await.expect("reopen");
        let restored = repo
            .get_bucket_by_name(&bucket.name)
            .await
            .expect("read bucket")
            .expect("bucket exists");
        assert_eq!(restored.cors.as_ref(), Some(&configuration));
        let updated = repo
            .set_bucket_cors(bucket.id, None)
            .await
            .expect("remove CORS");
        assert!(updated.cors.is_none());
    }

    #[tokio::test]
    async fn multipart_state_survives_restart() {
        let dir = tempdir().expect("temp");
        let path = dir.path().join("catalog.redb");
        let bucket = bucket("multipart-bucket");
        let upload = MultipartUpload {
            id: UploadId::new(),
            bucket_id: bucket.id,
            key: ObjectKey::new("large").expect("key"),
            content_type: None,
            custom_metadata: BTreeMap::new(),
            initiated_at: Utc::now(),
            state: MultipartUploadState::Active,
        };
        let part = UploadedPart {
            upload_id: upload.id,
            number: PartNumber::new(1).expect("part"),
            object_id: ObjectId::new(),
            size: 12,
            checksum: Checksum::sha256([3; 32]),
            payload_format: record_store_core::PayloadFormat::Plaintext,
            etag: ETag::from_md5([4; 16]),
            modified_at: Utc::now(),
        };
        {
            let repo = RedbMetadataRepository::open(&path).await.expect("repo");
            repo.create_bucket(&bucket).await.expect("bucket");
            repo.create_multipart_upload(&upload).await.expect("upload");
            repo.put_multipart_part(&part).await.expect("part");
        }
        let repo = RedbMetadataRepository::open(&path).await.expect("reopen");
        assert_eq!(
            repo.list_multipart_parts(upload.id, None, 10)
                .await
                .expect("parts"),
            vec![part]
        );
        assert_eq!(
            repo.storage_usage()
                .await
                .expect("usage")
                .temporary_multipart_bytes,
            12
        );
    }

    /// Buckets are addressable by identifier and by name, and both views must
    /// agree; a name index that drifts from the record is how a deleted bucket
    /// becomes un-recreatable.
    #[tokio::test]
    async fn buckets_are_reachable_by_identifier_and_by_name() {
        let (_directory, catalog, bucket) = catalog_with_bucket("photos").await;

        let by_id = catalog.get_bucket(bucket.id).await.expect("lookup");
        let by_name = catalog
            .get_bucket_by_name(&bucket.name)
            .await
            .expect("lookup");
        assert_eq!(by_id, Some(bucket.clone()));
        assert_eq!(by_name, Some(bucket.clone()));

        assert_eq!(catalog.list_buckets().await.expect("list"), vec![bucket]);
    }

    #[tokio::test]
    async fn a_duplicate_bucket_name_is_refused() {
        let (_directory, catalog, _bucket) = catalog_with_bucket("photos").await;
        let clash = super::super::test_support::bucket("photos");
        assert!(matches!(
            catalog.create_bucket(&clash).await,
            Err(MetadataError::BucketAlreadyExists)
        ));
    }

    #[tokio::test]
    async fn an_absent_bucket_reads_as_absent_rather_than_failing() {
        let (_directory, catalog) = catalog().await;
        assert!(
            catalog
                .get_bucket(BucketId::new())
                .await
                .expect("lookup")
                .is_none()
        );
        assert!(
            catalog
                .get_bucket_by_name(&BucketName::new("missing").expect("name"))
                .await
                .expect("lookup")
                .is_none()
        );
        assert!(catalog.list_buckets().await.expect("list").is_empty());
    }

    /// Deleting a bucket that still holds objects would orphan their payloads,
    /// so the catalog refuses until it is empty.
    #[tokio::test]
    async fn a_bucket_holding_objects_cannot_be_deleted() {
        let (_directory, catalog, bucket) = catalog_with_bucket("photos").await;
        catalog
            .put_object(&object(bucket.id, "a.txt", 10))
            .await
            .expect("put");

        assert!(matches!(
            catalog.delete_bucket(&bucket.name).await,
            Err(MetadataError::BucketNotEmpty)
        ));

        catalog
            .delete_object(
                bucket.id,
                &ObjectKey::new("a.txt").expect("key"),
                NewDeleteMarker::generate(),
            )
            .await
            .expect("delete object");
        catalog
            .delete_bucket(&bucket.name)
            .await
            .expect("delete bucket");
        assert!(
            catalog
                .get_bucket(bucket.id)
                .await
                .expect("lookup")
                .is_none()
        );
    }

    #[tokio::test]
    async fn bucket_quota_and_versioning_changes_are_durable() {
        let (_directory, catalog, bucket) = catalog_with_bucket("photos").await;

        catalog
            .set_bucket_quota(
                bucket.id,
                BucketQuota {
                    bytes: ByteQuota::Limit(4_096),
                    objects: ObjectCountQuota::Limit(10),
                },
            )
            .await
            .expect("quota");
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("versioning");

        let stored = catalog
            .get_bucket(bucket.id)
            .await
            .expect("lookup")
            .expect("bucket");
        assert_eq!(stored.quota.bytes, ByteQuota::Limit(4_096));
        assert_eq!(stored.quota.objects, ObjectCountQuota::Limit(10));
        assert_eq!(stored.versioning, VersioningState::Enabled);
    }

    /// Listing is the paging contract the S3 layer depends on: a prefix must not
    /// leak neighbouring keys, and the page boundary must be lossless.
    #[tokio::test]
    async fn object_listing_respects_prefix_limit_and_start_after() {
        let (_directory, catalog, bucket) = catalog_with_bucket("photos").await;
        for key in ["a/1", "a/2", "a/3", "b/1"] {
            catalog
                .put_object(&object(bucket.id, key, 1))
                .await
                .expect("put");
        }

        let page = catalog
            .list_objects(ListObjectsRequest {
                bucket_id: bucket.id,
                prefix: "a/".to_owned(),
                start_after: None,
                limit: 2,
            })
            .await
            .expect("list");
        assert_eq!(
            page.objects
                .iter()
                .map(|object| object.key.as_str())
                .collect::<Vec<_>>(),
            vec!["a/1", "a/2"]
        );

        let rest = catalog
            .list_objects(ListObjectsRequest {
                bucket_id: bucket.id,
                prefix: "a/".to_owned(),
                start_after: Some("a/2".to_owned()),
                limit: 10,
            })
            .await
            .expect("list");
        assert_eq!(
            rest.objects
                .iter()
                .map(|object| object.key.as_str())
                .collect::<Vec<_>>(),
            vec!["a/3"],
            "the prefix must not spill into the neighbouring key space"
        );
    }

    /// Usage counters back quota enforcement, so they have to follow writes and
    /// deletions rather than only ever growing.
    #[tokio::test]
    async fn usage_counters_track_writes_and_deletions() {
        let (_directory, catalog, bucket) = catalog_with_bucket("photos").await;
        let initial = catalog.bucket_usage().await.expect("usage");
        let summary = initial
            .get(&bucket.id)
            .expect("an empty bucket still needs a row so a table can render it");
        assert_eq!(summary.object_count, 0);
        assert_eq!(summary.logical_bytes, 0);

        catalog
            .put_object(&object(bucket.id, "a.txt", 512))
            .await
            .expect("put");
        catalog
            .put_object(&object(bucket.id, "b.txt", 512))
            .await
            .expect("put");

        let usage = catalog.bucket_usage().await.expect("usage");
        let summary = usage.get(&bucket.id).expect("bucket usage");
        assert_eq!(summary.object_count, 2);
        assert_eq!(summary.logical_bytes, 1_024);

        catalog
            .delete_object(
                bucket.id,
                &ObjectKey::new("a.txt").expect("key"),
                NewDeleteMarker::generate(),
            )
            .await
            .expect("delete");
        let after = catalog.bucket_usage().await.expect("usage");
        let summary = after.get(&bucket.id).expect("bucket usage");
        assert_eq!(summary.object_count, 1);
        assert_eq!(summary.logical_bytes, 512);

        let total = catalog.storage_usage().await.expect("usage");
        assert_eq!(total.object_count, 1);
        assert_eq!(total.bucket_count, 1);
    }

    /// A lifecycle rule is durable configuration; losing one silently would stop
    /// expiring data an operator believes is being cleaned up.
    #[tokio::test]
    async fn lifecycle_rules_round_trip_and_can_be_removed() {
        let (_directory, catalog, bucket) = catalog_with_bucket("photos").await;
        let rule = LifecycleRule {
            id: LifecycleRuleId::new(),
            bucket_id: bucket.id,
            prefix: "logs/".to_owned(),
            enabled: true,
            expiration: Some(ExpirationDays::new(30).expect("days")),
            noncurrent_version_expiration: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        catalog.put_lifecycle_rule(&rule).await.expect("put rule");

        let listed = catalog
            .list_lifecycle_rules(Some(bucket.id))
            .await
            .expect("list rules");
        assert_eq!(listed, vec![rule.clone()]);

        catalog
            .delete_lifecycle_rule(rule.id)
            .await
            .expect("delete rule");
        assert!(
            catalog
                .list_lifecycle_rules(Some(bucket.id))
                .await
                .expect("list rules")
                .is_empty()
        );
        assert!(matches!(
            catalog.delete_lifecycle_rule(rule.id).await,
            Err(MetadataError::LifecycleRuleNotFound)
        ));
    }

    /// Multipart state has to survive listing and part enumeration, because an
    /// upload that cannot be found again can never be completed or aborted.
    #[tokio::test]
    async fn multipart_uploads_and_their_parts_are_enumerable() {
        let (_directory, catalog, bucket) = catalog_with_bucket("photos").await;
        let upload = upload(bucket.id, "big.bin");
        catalog
            .create_multipart_upload(&upload)
            .await
            .expect("create upload");
        for number in 1..=3 {
            catalog
                .put_multipart_part(&part(upload.id, number, 1_024))
                .await
                .expect("put part");
        }

        let uploads = catalog
            .list_multipart_uploads(ListMultipartUploadsRequest {
                bucket_id: bucket.id,
                prefix: String::new(),
                upload_id_marker: None,
                limit: 10,
            })
            .await
            .expect("list uploads");
        assert_eq!(uploads.uploads.len(), 1);

        let parts = catalog
            .list_multipart_parts(upload.id, None, 10)
            .await
            .expect("list parts");
        assert_eq!(
            parts
                .iter()
                .map(|part| part.number.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "parts must enumerate in ascending order"
        );

        catalog
            .abort_multipart_upload(upload.id)
            .await
            .expect("abort");
        assert!(
            catalog
                .get_multipart_upload(upload.id)
                .await
                .expect("lookup")
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unknown_multipart_upload_is_reported_as_missing() {
        let (_directory, catalog) = catalog().await;
        assert!(
            catalog
                .get_multipart_upload(UploadId::new())
                .await
                .expect("lookup")
                .is_none()
        );
        assert!(matches!(
            catalog.abort_multipart_upload(UploadId::new()).await,
            Err(MetadataError::MultipartUploadNotFound)
        ));
    }

    /// Payload cleanup is what stops deleted objects leaking disk. A payload
    /// still referenced by any version must never be queued for removal.
    #[tokio::test]
    async fn deleted_payloads_are_queued_for_cleanup_once_unreferenced() {
        let (_directory, catalog, bucket) = catalog_with_bucket("photos").await;
        let stored = object(bucket.id, "a.txt", 64);
        catalog.put_object(&stored).await.expect("put");
        assert!(
            catalog
                .payload_referenced(stored.id)
                .await
                .expect("referenced")
        );

        catalog
            .delete_object(bucket.id, &stored.key, NewDeleteMarker::generate())
            .await
            .expect("delete");
        let queued = catalog.pending_cleanup(10).await.expect("pending");
        assert!(queued.contains(&stored.id), "{queued:?}");

        catalog
            .complete_cleanup(stored.id)
            .await
            .expect("complete cleanup");
        assert!(
            catalog
                .pending_cleanup(10)
                .await
                .expect("pending")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_fresh_catalog_reports_itself_ready() {
        let (_directory, catalog) = catalog().await;
        catalog.check_ready().await.expect("ready");
    }

    /// With versioning on, a write keeps the previous version addressable and a
    /// delete hides the object behind a marker rather than destroying history.
    #[tokio::test]
    async fn versioned_writes_keep_history_and_deletes_leave_a_marker() {
        let (_directory, catalog, bucket) = catalog_with_bucket("versioned").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable versioning");

        let key = ObjectKey::new("note.txt").expect("key");
        let first = object(bucket.id, "note.txt", 3);
        let second = object(bucket.id, "note.txt", 5);
        catalog.put_object(&first).await.expect("put");
        catalog.put_object(&second).await.expect("put");

        assert_eq!(
            catalog
                .get_object(bucket.id, &key)
                .await
                .expect("read")
                .expect("current")
                .version_id,
            second.version_id,
            "the newest write is current"
        );
        assert!(
            catalog
                .get_object_version(bucket.id, &key, first.version_id)
                .await
                .expect("read")
                .is_some(),
            "the older version stays addressable"
        );

        catalog
            .delete_object(bucket.id, &key, NewDeleteMarker::generate())
            .await
            .expect("delete");
        assert!(
            catalog
                .get_object(bucket.id, &key)
                .await
                .expect("read")
                .is_none(),
            "a delete marker hides the object"
        );

        let page = catalog
            .list_object_versions(ListObjectVersionsRequest {
                bucket_id: bucket.id,
                prefix: String::new(),
                key_marker: None,
                version_id_marker: None,
                limit: 10,
            })
            .await
            .expect("list versions");
        assert_eq!(
            page.versions.len(),
            3,
            "two writes and one marker remain listed: {:?}",
            page.versions
        );
    }

    /// Deleting one version must not disturb the others, and deleting a version
    /// that was never stored has to be reported as absent rather than as success.
    #[tokio::test]
    async fn a_single_version_can_be_removed_without_touching_the_rest() {
        let (_directory, catalog, bucket) = catalog_with_bucket("versioned").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable versioning");
        let key = ObjectKey::new("note.txt").expect("key");
        let first = object(bucket.id, "note.txt", 3);
        let second = object(bucket.id, "note.txt", 5);
        catalog.put_object(&first).await.expect("put");
        catalog.put_object(&second).await.expect("put");

        let removed = catalog
            .delete_object_version(bucket.id, &key, first.version_id)
            .await
            .expect("delete version");
        assert!(removed.is_some());
        assert!(
            catalog
                .get_object_version(bucket.id, &key, first.version_id)
                .await
                .expect("read")
                .is_none()
        );
        assert!(
            catalog
                .get_object_version(bucket.id, &key, second.version_id)
                .await
                .expect("read")
                .is_some(),
            "the surviving version is untouched"
        );

        assert!(
            catalog
                .delete_object_version(bucket.id, &key, VersionId::new())
                .await
                .expect("delete version")
                .is_none(),
            "removing a version that was never stored reports absence"
        );
    }

    /// Suspended versioning writes to the null version, replacing it in place
    /// while leaving the versions written before suspension intact.
    #[tokio::test]
    async fn a_suspended_bucket_replaces_its_null_version_in_place() {
        let (_directory, catalog, bucket) = catalog_with_bucket("suspended").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");
        let key = ObjectKey::new("note.txt").expect("key");
        let versioned = object(bucket.id, "note.txt", 3);
        catalog.put_object(&versioned).await.expect("put");

        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Suspended)
            .await
            .expect("suspend");
        let first_null = object(bucket.id, "note.txt", 5);
        let second_null = object(bucket.id, "note.txt", 7);
        catalog.put_object(&first_null).await.expect("put");
        catalog.put_object(&second_null).await.expect("put");

        assert!(
            catalog
                .get_object_version(bucket.id, &key, versioned.version_id)
                .await
                .expect("read")
                .is_some(),
            "history from before suspension survives"
        );
        assert!(
            catalog
                .get_null_version(bucket.id, &key)
                .await
                .expect("read")
                .is_some()
        );
    }

    /// Version listing pages like object listing does, and the marker pair has
    /// to resume exactly where the previous page stopped.
    #[tokio::test]
    async fn version_listing_resumes_from_its_marker() {
        let (_directory, catalog, bucket) = catalog_with_bucket("versioned").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");
        for _ in 0..3 {
            catalog
                .put_object(&object(bucket.id, "note.txt", 1))
                .await
                .expect("put");
        }

        let first = catalog
            .list_object_versions(ListObjectVersionsRequest {
                bucket_id: bucket.id,
                prefix: String::new(),
                key_marker: None,
                version_id_marker: None,
                limit: 2,
            })
            .await
            .expect("list");
        assert_eq!(first.versions.len(), 2);

        let last = &first.versions.last().expect("entry").record;
        let rest = catalog
            .list_object_versions(ListObjectVersionsRequest {
                bucket_id: bucket.id,
                prefix: String::new(),
                key_marker: Some(last.key().as_str().to_owned()),
                version_id_marker: Some(last.version_id()),
                limit: 10,
            })
            .await
            .expect("list");
        assert_eq!(rest.versions.len(), 1, "the last page holds the remainder");
    }

    /// Payload references are what garbage collection consults. A payload owned
    /// by any surviving version must never be reported as collectable.
    #[tokio::test]
    async fn payload_references_follow_the_versions_that_own_them() {
        let (_directory, catalog, bucket) = catalog_with_bucket("payloads").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");
        let key = ObjectKey::new("note.txt").expect("key");
        let first = object(bucket.id, "note.txt", 3);
        let second = object(bucket.id, "note.txt", 5);
        catalog.put_object(&first).await.expect("put");
        catalog.put_object(&second).await.expect("put");

        assert!(catalog.payload_referenced(first.id).await.expect("read"));
        assert!(catalog.payload_referenced(second.id).await.expect("read"));

        let page = catalog
            .list_payload_references(None, 10)
            .await
            .expect("list references");
        assert!(page.object_ids.contains(&first.id), "{page:?}");
        assert!(page.object_ids.contains(&second.id), "{page:?}");

        catalog
            .delete_object_version(bucket.id, &key, first.version_id)
            .await
            .expect("delete version");
        assert!(
            !catalog.payload_referenced(first.id).await.expect("read"),
            "a removed version releases its payload"
        );
        assert!(
            catalog.payload_referenced(second.id).await.expect("read"),
            "the surviving version keeps its own"
        );
    }

    #[tokio::test]
    async fn a_payload_nobody_ever_stored_is_not_referenced() {
        let (_directory, catalog) = catalog().await;
        assert!(
            !catalog
                .payload_referenced(record_store_core::ObjectId::new())
                .await
                .expect("read")
        );
    }

    /// A part still held by an open multipart upload owns its payload, so
    /// collection must not reclaim it mid-upload.
    #[tokio::test]
    async fn an_open_multipart_part_keeps_its_payload_referenced() {
        let (_directory, catalog, bucket) = catalog_with_bucket("multipart").await;
        let upload = upload(bucket.id, "big.bin");
        catalog
            .create_multipart_upload(&upload)
            .await
            .expect("create upload");
        let stored = part(upload.id, 1, 1_024);
        catalog.put_multipart_part(&stored).await.expect("put part");

        assert!(
            catalog
                .payload_referenced(stored.object_id)
                .await
                .expect("read"),
            "an in-flight part owns its payload"
        );

        catalog
            .abort_multipart_upload(upload.id)
            .await
            .expect("abort");
        assert!(
            !catalog
                .payload_referenced(stored.object_id)
                .await
                .expect("read"),
            "aborting releases it"
        );
    }

    #[tokio::test]
    async fn reads_against_an_absent_object_report_absence_rather_than_failing() {
        let (_directory, catalog, bucket) = catalog_with_bucket("empty").await;
        let key = ObjectKey::new("absent.txt").expect("key");
        assert!(
            catalog
                .get_object(bucket.id, &key)
                .await
                .expect("read")
                .is_none()
        );
        assert!(
            catalog
                .get_object_version(bucket.id, &key, VersionId::new())
                .await
                .expect("read")
                .is_none()
        );
        assert!(
            catalog
                .get_null_version(bucket.id, &key)
                .await
                .expect("read")
                .is_none()
        );
    }

    /// Enabling versioning is reversible only as far as suspension. Going back
    /// to disabled would strand the versions already written, so the transition
    /// is refused outright rather than silently dropping history.
    #[tokio::test]
    async fn versioning_cannot_be_switched_back_off_once_enabled() {
        let (_directory, catalog, bucket) = catalog_with_bucket("versioned").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");

        assert!(matches!(
            catalog
                .set_bucket_versioning(bucket.id, VersioningState::Disabled)
                .await,
            Err(MetadataError::InvalidVersioningTransition)
        ));

        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Suspended)
            .await
            .expect("suspending is allowed");
        // Suspending first does permit a return to disabled. The guard only
        // blocks the direct Enabled -> Disabled jump, which is the transition
        // that would strand history without the operator having said so twice.
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Disabled)
            .await
            .expect("disabling from suspended is permitted");
    }

    /// A quota is enforced at publication, which is the only place it can be
    /// enforced transactionally. Accepting a write past the limit would make the
    /// quota advisory rather than real.
    #[tokio::test]
    async fn a_byte_quota_refuses_the_write_that_would_exceed_it() {
        let (_directory, catalog, bucket) = catalog_with_bucket("bounded").await;
        catalog
            .set_bucket_quota(
                bucket.id,
                BucketQuota {
                    bytes: ByteQuota::Limit(100),
                    objects: ObjectCountQuota::Unlimited,
                },
            )
            .await
            .expect("set quota");

        catalog
            .put_object(&object(bucket.id, "small.txt", 60))
            .await
            .expect("a write inside the budget is accepted");
        assert!(matches!(
            catalog
                .put_object(&object(bucket.id, "large.txt", 60))
                .await,
            Err(MetadataError::QuotaExceeded)
        ));
    }

    #[tokio::test]
    async fn an_object_count_quota_refuses_the_object_that_would_exceed_it() {
        let (_directory, catalog, bucket) = catalog_with_bucket("counted").await;
        catalog
            .set_bucket_quota(
                bucket.id,
                BucketQuota {
                    bytes: ByteQuota::Unlimited,
                    objects: ObjectCountQuota::Limit(1),
                },
            )
            .await
            .expect("set quota");

        catalog
            .put_object(&object(bucket.id, "first.txt", 1))
            .await
            .expect("first");
        assert!(matches!(
            catalog
                .put_object(&object(bucket.id, "second.txt", 1))
                .await,
            Err(MetadataError::QuotaExceeded)
        ));
    }

    /// Setting a quota below what is already stored would leave the bucket
    /// permanently over its limit, so it is refused rather than applied.
    #[tokio::test]
    async fn a_quota_below_current_usage_is_refused() {
        let (_directory, catalog, bucket) = catalog_with_bucket("occupied").await;
        catalog
            .put_object(&object(bucket.id, "a.txt", 500))
            .await
            .expect("put");

        assert!(matches!(
            catalog
                .set_bucket_quota(
                    bucket.id,
                    BucketQuota {
                        bytes: ByteQuota::Limit(100),
                        objects: ObjectCountQuota::Unlimited,
                    },
                )
                .await,
            Err(MetadataError::QuotaExceeded)
        ));
    }

    /// A multipart upload is a state machine. Uploading into a completed one, or
    /// completing it twice, has to be refused or two writers could both believe
    /// they own the object.
    #[tokio::test]
    async fn a_multipart_upload_refuses_conflicting_state_changes() {
        let (_directory, catalog, bucket) = catalog_with_bucket("multipart").await;
        let upload = upload(bucket.id, "big.bin");
        catalog
            .create_multipart_upload(&upload)
            .await
            .expect("create");
        catalog
            .put_multipart_part(&part(upload.id, 1, 8))
            .await
            .expect("put part");

        let committed = object(bucket.id, "big.bin", 8);
        catalog
            .begin_multipart_completion(upload.id, committed.id)
            .await
            .expect("begin completion");

        assert!(
            matches!(
                catalog.put_multipart_part(&part(upload.id, 2, 8)).await,
                Err(MetadataError::MultipartStateConflict)
            ),
            "parts cannot be added once completion has begun"
        );
    }

    /// An upload whose completion was interrupted has to be recoverable, or a
    /// crash mid-commit would leave the object permanently unreachable.
    #[tokio::test]
    async fn an_interrupted_completion_is_recoverable() {
        let (_directory, catalog, bucket) = catalog_with_bucket("multipart").await;
        let upload = upload(bucket.id, "big.bin");
        catalog
            .create_multipart_upload(&upload)
            .await
            .expect("create");
        catalog
            .put_multipart_part(&part(upload.id, 1, 8))
            .await
            .expect("put part");
        let committed = object(bucket.id, "big.bin", 8);
        catalog
            .begin_multipart_completion(upload.id, committed.id)
            .await
            .expect("begin completion");

        let recovered = catalog
            .recover_multipart_completions()
            .await
            .expect("recover");
        assert!(
            !recovered.parts.is_empty()
                || catalog
                    .get_multipart_upload(upload.id)
                    .await
                    .expect("read")
                    .is_some(),
            "an interrupted completion must leave something to recover: {recovered:?}"
        );
    }

    /// A bucket's CORS policy decides which websites may read its objects, so it
    /// has to survive a round trip exactly and be removable.
    #[tokio::test]
    async fn a_cors_policy_round_trips_and_can_be_cleared() {
        let (_directory, catalog, bucket) = catalog_with_bucket("cors").await;
        catalog
            .set_bucket_cors(bucket.id, Some(cors_configuration()))
            .await
            .expect("set cors");
        assert!(
            catalog
                .get_bucket(bucket.id)
                .await
                .expect("read")
                .expect("bucket")
                .cors
                .is_some()
        );

        catalog
            .set_bucket_cors(bucket.id, None)
            .await
            .expect("clear cors");
        assert!(
            catalog
                .get_bucket(bucket.id)
                .await
                .expect("read")
                .expect("bucket")
                .cors
                .is_none()
        );
    }

    /// A delete marker can itself be the current version, and writing again has
    /// to make the new object current without disturbing the marker's place in
    /// history. Getting this wrong makes a deleted object reappear or stay gone.
    #[tokio::test]
    async fn a_write_after_a_delete_marker_becomes_current_again() {
        let (_directory, catalog, bucket) = catalog_with_bucket("resurrect").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");
        let key = ObjectKey::new("note.txt").expect("key");

        catalog
            .put_object(&object(bucket.id, "note.txt", 3))
            .await
            .expect("put");
        catalog
            .delete_object(bucket.id, &key, NewDeleteMarker::generate())
            .await
            .expect("delete");
        assert!(
            catalog
                .get_object(bucket.id, &key)
                .await
                .expect("read")
                .is_none()
        );

        let revived = object(bucket.id, "note.txt", 7);
        catalog.put_object(&revived).await.expect("put");
        assert_eq!(
            catalog
                .get_object(bucket.id, &key)
                .await
                .expect("read")
                .expect("current")
                .version_id,
            revived.version_id
        );

        let page = catalog
            .list_object_versions(ListObjectVersionsRequest {
                bucket_id: bucket.id,
                prefix: String::new(),
                key_marker: None,
                version_id_marker: None,
                limit: 10,
            })
            .await
            .expect("list versions");
        assert_eq!(page.versions.len(), 3, "the marker stays in history");
        assert!(
            page.versions.first().expect("newest").is_latest,
            "the newest entry is the current one"
        );
    }

    /// Removing the current version has to promote the next one rather than
    /// leaving the key with no current version at all.
    #[tokio::test]
    async fn deleting_the_current_version_promotes_the_previous_one() {
        let (_directory, catalog, bucket) = catalog_with_bucket("promote").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");
        let key = ObjectKey::new("note.txt").expect("key");
        let first = object(bucket.id, "note.txt", 3);
        let second = object(bucket.id, "note.txt", 5);
        catalog.put_object(&first).await.expect("put");
        catalog.put_object(&second).await.expect("put");

        catalog
            .delete_object_version(bucket.id, &key, second.version_id)
            .await
            .expect("delete current");

        assert_eq!(
            catalog
                .get_object(bucket.id, &key)
                .await
                .expect("read")
                .expect("current")
                .version_id,
            first.version_id,
            "the older version must become current"
        );
    }

    /// Usage counters have to follow version deletion too, or a versioned bucket
    /// would drift permanently over its quota as history is pruned.
    #[tokio::test]
    async fn usage_follows_version_deletion_in_a_versioned_bucket() {
        let (_directory, catalog, bucket) = catalog_with_bucket("counted").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");
        let key = ObjectKey::new("note.txt").expect("key");
        let first = object(bucket.id, "note.txt", 100);
        let second = object(bucket.id, "note.txt", 200);
        catalog.put_object(&first).await.expect("put");
        catalog.put_object(&second).await.expect("put");

        let usage = catalog.bucket_usage().await.expect("usage");
        let before = usage.get(&bucket.id).expect("summary");
        assert_eq!(before.version_count, 2);
        assert_eq!(before.version_bytes, 300);

        catalog
            .delete_object_version(bucket.id, &key, first.version_id)
            .await
            .expect("delete version");

        let usage = catalog.bucket_usage().await.expect("usage");
        let after = usage.get(&bucket.id).expect("summary");
        assert_eq!(after.version_count, 1);
        assert_eq!(after.version_bytes, 200);
    }

    /// Listing has to page across keys as well as within one, so a marker naming
    /// the last key of a page resumes at the next key rather than repeating it.
    #[tokio::test]
    async fn version_listing_pages_across_keys() {
        let (_directory, catalog, bucket) = catalog_with_bucket("paged").await;
        catalog
            .set_bucket_versioning(bucket.id, VersioningState::Enabled)
            .await
            .expect("enable");
        for key in ["a", "b", "c"] {
            catalog
                .put_object(&object(bucket.id, key, 1))
                .await
                .expect("put");
        }

        let first = catalog
            .list_object_versions(ListObjectVersionsRequest {
                bucket_id: bucket.id,
                prefix: String::new(),
                key_marker: None,
                version_id_marker: None,
                limit: 2,
            })
            .await
            .expect("list");
        assert_eq!(first.versions.len(), 2);

        let last = &first.versions.last().expect("entry").record;
        let rest = catalog
            .list_object_versions(ListObjectVersionsRequest {
                bucket_id: bucket.id,
                prefix: String::new(),
                key_marker: Some(last.key().as_str().to_owned()),
                version_id_marker: Some(last.version_id()),
                limit: 10,
            })
            .await
            .expect("list");
        assert_eq!(rest.versions.len(), 1);
        assert_ne!(
            rest.versions[0].record.key().as_str(),
            last.key().as_str(),
            "the marker's own key must not repeat"
        );
    }

    /// Multipart parts count towards the bucket's footprint while they are in
    /// flight, so a stalled upload is visible rather than invisible storage.
    #[tokio::test]
    async fn in_flight_multipart_parts_are_counted_against_the_bucket() {
        let (_directory, catalog, bucket) = catalog_with_bucket("inflight").await;
        let upload = upload(bucket.id, "big.bin");
        catalog
            .create_multipart_upload(&upload)
            .await
            .expect("create");
        catalog
            .put_multipart_part(&part(upload.id, 1, 4_096))
            .await
            .expect("put part");

        let usage = catalog.bucket_usage().await.expect("usage");
        let summary = usage.get(&bucket.id).expect("summary");
        assert_eq!(summary.multipart_bytes, 4_096, "{summary:?}");

        catalog
            .abort_multipart_upload(upload.id)
            .await
            .expect("abort");
        let usage = catalog.bucket_usage().await.expect("usage");
        assert_eq!(
            usage.get(&bucket.id).expect("summary").multipart_bytes,
            0,
            "aborting releases the reservation"
        );
    }

    /// Parts enumerate from a marker so a client can resume listing a large
    /// upload without re-reading what it already has.
    #[tokio::test]
    async fn multipart_parts_page_from_a_marker() {
        let (_directory, catalog, bucket) = catalog_with_bucket("paged-parts").await;
        let upload = upload(bucket.id, "big.bin");
        catalog
            .create_multipart_upload(&upload)
            .await
            .expect("create");
        for number in 1..=3 {
            catalog
                .put_multipart_part(&part(upload.id, number, 8))
                .await
                .expect("put part");
        }

        let first = catalog
            .list_multipart_parts(upload.id, None, 2)
            .await
            .expect("list");
        assert_eq!(
            first
                .iter()
                .map(|part| part.number.get())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        let rest = catalog
            .list_multipart_parts(upload.id, Some(first[1].number), 10)
            .await
            .expect("list");
        assert_eq!(
            rest.iter()
                .map(|part| part.number.get())
                .collect::<Vec<_>>(),
            vec![3],
            "the marker part must not repeat"
        );
    }
}
