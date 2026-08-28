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
    use crate::test_support::*;
    use crate::{MetadataRepository, RedbMetadataRepository};

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
}
