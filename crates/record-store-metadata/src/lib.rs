//! Durable single-node metadata catalog.

use std::{fmt::Display, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oes_core::{
    Bucket, BucketId, BucketName, BucketQuota, CorsConfiguration, DeleteMarker, LifecycleRule,
    LifecycleRuleId, MultipartUpload, MultipartUploadState, ObjectId, ObjectKey, ObjectMetadata,
    ObjectVersionRecord, PartNumber, StorageUsage, UploadId, UploadedPart, VersionId,
    VersioningState,
};
use redb::{Database, ReadableTable, TableDefinition, TableHandle};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

const BUCKETS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("buckets.v1");
const BUCKET_NAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("bucket_names.v1");
const OBJECTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("objects.v1");
const MARKERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("delete_markers.v1");
const VERSIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("versions.v1");
const VERSION_ORDER: TableDefinition<&[u8], &[u8]> = TableDefinition::new("version_order.v1");
const NULL_VERSIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("null_versions.v1");
const MULTIPART: TableDefinition<&[u8], &[u8]> = TableDefinition::new("multipart.v1");
const MULTIPART_ORDER: TableDefinition<&[u8], &[u8]> = TableDefinition::new("multipart_order.v1");
const PARTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("multipart_parts.v1");
const CLEANUP: TableDefinition<&[u8], u8> = TableDefinition::new("payload_cleanup.v1");
const BUCKET_USAGE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("bucket_usage.v1");
const LIFECYCLE_RULES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("lifecycle_rules.v1");
const COUNTERS: TableDefinition<&str, u64> = TableDefinition::new("counters.v1");
const SCHEMA: TableDefinition<&str, u64> = TableDefinition::new("schema.v1");

/// Current durable catalog format used by offline backup compatibility checks.
pub const METADATA_SCHEMA_VERSION: u64 = 4;
const CURRENT_SCHEMA_VERSION: u64 = METADATA_SCHEMA_VERSION;
const OBJECT_COUNT: &str = "objects";
const BUCKET_COUNT: &str = "buckets";
const LOGICAL_BYTES: &str = "logical_bytes";
const LEGACY_BYTES: &str = "bytes";
const VERSION_COUNT: &str = "versions";
const VERSION_BYTES: &str = "version_bytes";
const PHYSICAL_BYTES: &str = "physical_bytes";
const MULTIPART_BYTES: &str = "multipart_bytes";

/// Bounded ordered object-listing input.
#[derive(Debug, Clone)]
pub struct ListObjectsRequest {
    pub bucket_id: BucketId,
    pub prefix: String,
    pub start_after: Option<String>,
    pub limit: usize,
}

/// Bounded ordered current-object page.
#[derive(Debug, Clone)]
pub struct ObjectMetadataPage {
    pub objects: Vec<ObjectMetadata>,
    pub next_key: Option<String>,
}

/// Bounded ordered version-listing input.
#[derive(Debug, Clone)]
pub struct ListObjectVersionsRequest {
    pub bucket_id: BucketId,
    pub prefix: String,
    pub key_marker: Option<String>,
    pub version_id_marker: Option<VersionId>,
    pub limit: usize,
}

/// Version entry with current-state annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedObjectVersion {
    pub record: ObjectVersionRecord,
    pub is_latest: bool,
}

/// Bounded ordered object-version page.
#[derive(Debug, Clone)]
pub struct ObjectVersionPage {
    pub versions: Vec<ListedObjectVersion>,
    pub next_key_marker: Option<String>,
    pub next_version_id_marker: Option<VersionId>,
}

/// Bounded multipart-upload listing input.
#[derive(Debug, Clone)]
pub struct ListMultipartUploadsRequest {
    pub bucket_id: BucketId,
    pub prefix: String,
    pub upload_id_marker: Option<UploadId>,
    pub limit: usize,
}

/// Bounded multipart-upload page.
#[derive(Debug, Clone)]
pub struct MultipartUploadPage {
    pub uploads: Vec<MultipartUpload>,
    pub next_upload_id_marker: Option<UploadId>,
}

/// Publication result naming only payloads that became unreachable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectCommitResult {
    pub cleanup: Vec<ObjectMetadata>,
}

/// Result of applying an ordinary delete.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteObjectResult {
    pub delete_marker: Option<DeleteMarker>,
    pub cleanup: Vec<ObjectMetadata>,
    pub previously_visible: bool,
}

/// Result of permanently deleting an explicit version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteVersionResult {
    pub removed: ObjectVersionRecord,
    pub cleanup: Option<ObjectMetadata>,
}

/// Multipart state removed after completion or abort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipartCleanupResult {
    pub parts: Vec<UploadedPart>,
}

/// Bounded page of immutable payload identifiers referenced by metadata.
#[derive(Debug, Clone, Default)]
pub struct PayloadReferencePage {
    pub object_ids: Vec<ObjectId>,
    pub next_object_id: Option<ObjectId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct BucketUsage {
    current_objects: u64,
    logical_bytes: u64,
    versions: u64,
    version_bytes: u64,
    multipart_bytes: u64,
}

/// Per-bucket accounting maintained transactionally with every commit.
///
/// These counters exist so that listing buckets can report size and object
/// counts in one request instead of one request per bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketUsageSummary {
    /// Currently visible objects.
    pub object_count: u64,
    /// Bytes referenced by currently visible objects.
    pub logical_bytes: u64,
    /// Immutable versions, including current versions.
    pub version_count: u64,
    /// Bytes referenced by every immutable version.
    pub version_bytes: u64,
    /// Bytes held by durable multipart parts.
    pub multipart_bytes: u64,
}

impl From<BucketUsage> for BucketUsageSummary {
    fn from(value: BucketUsage) -> Self {
        Self {
            object_count: value.current_objects,
            logical_bytes: value.logical_bytes,
            version_count: value.versions,
            version_bytes: value.version_bytes,
            multipart_bytes: value.multipart_bytes,
        }
    }
}

/// Durable metadata boundary used by storage and application services.
#[async_trait]
pub trait MetadataRepository: Send + Sync {
    async fn create_bucket(&self, bucket: &Bucket) -> Result<(), MetadataError>;
    async fn get_bucket(&self, id: BucketId) -> Result<Option<Bucket>, MetadataError>;
    async fn get_bucket_by_name(&self, name: &BucketName) -> Result<Option<Bucket>, MetadataError>;
    async fn list_buckets(&self) -> Result<Vec<Bucket>, MetadataError>;
    async fn set_bucket_versioning(
        &self,
        id: BucketId,
        state: VersioningState,
    ) -> Result<Bucket, MetadataError>;
    async fn set_bucket_quota(
        &self,
        id: BucketId,
        quota: BucketQuota,
    ) -> Result<Bucket, MetadataError>;
    async fn set_bucket_cors(
        &self,
        id: BucketId,
        configuration: Option<CorsConfiguration>,
    ) -> Result<Bucket, MetadataError>;
    async fn delete_bucket(&self, name: &BucketName) -> Result<Bucket, MetadataError>;
    async fn put_object(
        &self,
        metadata: &ObjectMetadata,
    ) -> Result<ObjectCommitResult, MetadataError>;
    async fn get_object(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectMetadata>, MetadataError>;
    async fn get_object_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
    ) -> Result<Option<ObjectVersionRecord>, MetadataError>;
    async fn get_null_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectVersionRecord>, MetadataError>;
    /// Applies ordinary delete semantics.
    ///
    /// The caller supplies the delete-marker identity so that the operation is
    /// deterministic and can be replicated through consensus.
    async fn delete_object(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        marker: NewDeleteMarker,
    ) -> Result<DeleteObjectResult, MetadataError>;
    async fn delete_object_version(
        &self,
        bucket: BucketId,
        key: &ObjectKey,
        version: VersionId,
    ) -> Result<Option<DeleteVersionResult>, MetadataError>;
    async fn list_objects(
        &self,
        request: ListObjectsRequest,
    ) -> Result<ObjectMetadataPage, MetadataError>;
    async fn list_object_versions(
        &self,
        request: ListObjectVersionsRequest,
    ) -> Result<ObjectVersionPage, MetadataError>;
    async fn create_multipart_upload(&self, upload: &MultipartUpload) -> Result<(), MetadataError>;
    async fn get_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<Option<MultipartUpload>, MetadataError>;
    async fn put_multipart_part(
        &self,
        part: &UploadedPart,
    ) -> Result<Option<UploadedPart>, MetadataError>;
    async fn list_multipart_parts(
        &self,
        id: UploadId,
        after: Option<PartNumber>,
        limit: usize,
    ) -> Result<Vec<UploadedPart>, MetadataError>;
    async fn list_multipart_uploads(
        &self,
        request: ListMultipartUploadsRequest,
    ) -> Result<MultipartUploadPage, MetadataError>;
    async fn begin_multipart_completion(
        &self,
        id: UploadId,
        object_id: ObjectId,
    ) -> Result<MultipartUpload, MetadataError>;
    async fn finish_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<MultipartCleanupResult, MetadataError>;
    async fn abort_multipart_upload(
        &self,
        id: UploadId,
    ) -> Result<MultipartCleanupResult, MetadataError>;
    /// Reconciles crash-interrupted completion state before readiness.
    async fn recover_multipart_completions(&self) -> Result<MultipartCleanupResult, MetadataError>;
    async fn put_lifecycle_rule(&self, rule: &LifecycleRule) -> Result<(), MetadataError>;
    async fn list_lifecycle_rules(
        &self,
        bucket: Option<BucketId>,
    ) -> Result<Vec<LifecycleRule>, MetadataError>;
    async fn delete_lifecycle_rule(&self, id: LifecycleRuleId) -> Result<(), MetadataError>;
    async fn storage_usage(&self) -> Result<StorageUsage, MetadataError>;
    /// Returns per-bucket accounting for every bucket in one pass.
    ///
    /// Callers that render a bucket table need this to avoid issuing one request
    /// per bucket.
    async fn bucket_usage(
        &self,
    ) -> Result<std::collections::BTreeMap<BucketId, BucketUsageSummary>, MetadataError>;
    async fn pending_cleanup(&self, limit: usize) -> Result<Vec<ObjectId>, MetadataError>;
    async fn complete_cleanup(&self, id: ObjectId) -> Result<(), MetadataError>;
    /// Returns whether any durable object version or multipart part owns a payload.
    async fn payload_referenced(&self, id: ObjectId) -> Result<bool, MetadataError>;
    async fn list_payload_references(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<PayloadReferencePage, MetadataError>;
    async fn check_ready(&self) -> Result<(), MetadataError>;
}

/// Stable metadata failure categories.
#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("failed to prepare metadata directory: {0}")]
    Directory(#[source] std::io::Error),
    #[error("bucket already exists")]
    BucketAlreadyExists,
    #[error("bucket was not found")]
    BucketNotFound,
    #[error("bucket is not empty")]
    BucketNotEmpty,
    #[error("multipart upload was not found")]
    MultipartUploadNotFound,
    #[error("multipart upload state conflicts with the operation")]
    MultipartStateConflict,
    #[error("storage quota exceeded")]
    QuotaExceeded,
    #[error("invalid bucket versioning transition")]
    InvalidVersioningTransition,
    #[error("lifecycle rule was not found")]
    LifecycleRuleNotFound,
    #[error("invalid lifecycle rule: {0}")]
    InvalidLifecycleRule(String),
    #[error("metadata encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("metadata database operation '{operation}' failed: {reason}")]
    Database {
        operation: &'static str,
        reason: String,
    },
    #[error("metadata task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Redb-backed durable catalog for a standalone OES node.
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

    /// Opens a catalog that shares a database with other durable OES state.
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

fn initialize_schema(database: &Database) -> Result<(), MetadataError> {
    let write = database
        .begin_write()
        .map_err(|e| backend("initialize", e))?;
    for table in [
        BUCKETS,
        OBJECTS,
        MARKERS,
        VERSIONS,
        VERSION_ORDER,
        NULL_VERSIONS,
        MULTIPART,
        MULTIPART_ORDER,
        PARTS,
        BUCKET_USAGE,
        LIFECYCLE_RULES,
    ] {
        write
            .open_table(table)
            .map_err(|e| backend("initialize byte table", e))?;
    }
    write
        .open_table(BUCKET_NAMES)
        .map_err(|e| backend("initialize bucket names", e))?;
    write
        .open_table(CLEANUP)
        .map_err(|e| backend("initialize cleanup", e))?;
    write
        .open_table(COUNTERS)
        .map_err(|e| backend("initialize counters", e))?;
    write
        .open_table(SCHEMA)
        .map_err(|e| backend("initialize schema", e))?;
    let version = {
        let table = write
            .open_table(SCHEMA)
            .map_err(|e| backend("open schema", e))?;
        table
            .get("metadata")
            .map_err(|e| backend("read schema", e))?
            .map_or(0, |v| v.value())
    };
    if version > CURRENT_SCHEMA_VERSION {
        return Err(MetadataError::Database {
            operation: "schema compatibility",
            reason: format!(
                "schema {version} is newer than supported schema {CURRENT_SCHEMA_VERSION}"
            ),
        });
    }
    if version < 3 {
        migrate_v3(&write)?;
    }
    if version < 4 {
        migrate_v4(&write)?;
    }
    if version < CURRENT_SCHEMA_VERSION {
        let mut table = write
            .open_table(SCHEMA)
            .map_err(|e| backend("open schema", e))?;
        table
            .insert("metadata", &CURRENT_SCHEMA_VERSION)
            .map_err(|e| backend("write schema", e))?;
    }
    write
        .commit()
        .map_err(|e| backend("commit initialization", e))
}

fn migrate_v4(write: &redb::WriteTransaction) -> Result<(), MetadataError> {
    write
        .open_table(LIFECYCLE_RULES)
        .map_err(|e| backend("migrate lifecycle rules", e))?;
    Ok(())
}

fn migrate_v3(write: &redb::WriteTransaction) -> Result<(), MetadataError> {
    let buckets = {
        let table = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open migration buckets", e))?;
        let mut out = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| backend("iterate migration buckets", e))?
        {
            let (_, value) = entry.map_err(|e| backend("read migration bucket", e))?;
            out.push(serde_json::from_slice::<Bucket>(value.value())?);
        }
        out
    };
    let objects = {
        let table = write
            .open_table(OBJECTS)
            .map_err(|e| backend("open migration objects", e))?;
        let mut out = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| backend("iterate migration objects", e))?
        {
            let (_, value) = entry.map_err(|e| backend("read migration object", e))?;
            out.push(decode_migrating_object(value.value())?);
        }
        out
    };
    {
        let mut ids = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open migration buckets", e))?;
        let mut names = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open migration names", e))?;
        for bucket in &buckets {
            let bytes = serde_json::to_vec(bucket)?;
            ids.insert(bucket_key(bucket.id).as_slice(), bytes.as_slice())
                .map_err(|e| backend("migrate bucket", e))?;
            names
                .insert(bucket.name.as_str(), bytes.as_slice())
                .map_err(|e| backend("migrate bucket index", e))?;
        }
    }
    let mut total = 0_u64;
    for metadata in &objects {
        let record = ObjectVersionRecord::Object {
            metadata: metadata.clone(),
            is_null: true,
        };
        insert_version(write, &record)?;
        set_null(
            write,
            &object_key(metadata.bucket_id, &metadata.key),
            metadata.version_id,
        )?;
        let mut usage = read_bucket_usage(write, metadata.bucket_id)?;
        usage.current_objects += 1;
        usage.logical_bytes = usage
            .logical_bytes
            .checked_add(metadata.size)
            .ok_or_else(counter_error)?;
        usage.versions += 1;
        usage.version_bytes = usage
            .version_bytes
            .checked_add(metadata.size)
            .ok_or_else(counter_error)?;
        write_bucket_usage(write, metadata.bucket_id, usage)?;
        total = total.checked_add(metadata.size).ok_or_else(counter_error)?;
    }
    for bucket in &buckets {
        if read_bucket_usage(write, bucket.id)? == BucketUsage::default() {
            write_bucket_usage(write, bucket.id, BucketUsage::default())?;
        }
    }
    let legacy = {
        let table = write
            .open_table(COUNTERS)
            .map_err(|e| backend("open migration counters", e))?;
        read_counter(&table, LEGACY_BYTES)?.max(total)
    };
    let mut table = write
        .open_table(COUNTERS)
        .map_err(|e| backend("open migration counters", e))?;
    for (name, value) in [
        (OBJECT_COUNT, objects.len() as u64),
        (BUCKET_COUNT, buckets.len() as u64),
        (LOGICAL_BYTES, legacy),
        (VERSION_COUNT, objects.len() as u64),
        (VERSION_BYTES, legacy),
        (PHYSICAL_BYTES, legacy),
        (MULTIPART_BYTES, 0),
    ] {
        table
            .insert(name, &value)
            .map_err(|e| backend("write migration counter", e))?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct LegacyObjectMetadata {
    id: ObjectId,
    bucket_id: BucketId,
    key: ObjectKey,
    version_id: VersionId,
    size: u64,
    checksum: oes_core::Checksum,
    content_type: Option<String>,
    custom_metadata: std::collections::BTreeMap<String, String>,
    created_at: chrono::DateTime<Utc>,
    modified_at: chrono::DateTime<Utc>,
}

fn decode_migrating_object(bytes: &[u8]) -> Result<ObjectMetadata, MetadataError> {
    if let Ok(metadata) = serde_json::from_slice::<ObjectMetadata>(bytes) {
        return Ok(metadata);
    }
    let old: LegacyObjectMetadata = serde_json::from_slice(bytes)?;
    let etag = old
        .checksum
        .to_string()
        .split_once(':')
        .and_then(|(_, digest)| oes_core::ETag::new(digest).ok())
        .ok_or_else(|| MetadataError::Database {
            operation: "migrate ETag",
            reason: "invalid legacy checksum".into(),
        })?;
    Ok(ObjectMetadata {
        id: old.id,
        bucket_id: old.bucket_id,
        key: old.key,
        version_id: old.version_id,
        size: old.size,
        checksum: old.checksum,
        payload_format: oes_core::PayloadFormat::Plaintext,
        durability: oes_core::DurabilityProfile::Single,
        etag,
        content_type: old.content_type,
        custom_metadata: old.custom_metadata,
        created_at: old.created_at,
        modified_at: old.modified_at,
    })
}

fn update_bucket_tx<F>(
    write: &redb::WriteTransaction,
    id: BucketId,
    update: F,
) -> Result<Bucket, MetadataError>
where
    F: FnOnce(&mut Bucket) -> Result<(), MetadataError>,
{
    let mut bucket = read_bucket(write, id)?.ok_or(MetadataError::BucketNotFound)?;
    update(&mut bucket)?;
    let bytes = serde_json::to_vec(&bucket)?;
    {
        let mut table = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open buckets", e))?;
        table
            .insert(bucket_key(id).as_slice(), bytes.as_slice())
            .map_err(|e| backend("update bucket", e))?;
    }
    {
        let mut table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        table
            .insert(bucket.name.as_str(), bytes.as_slice())
            .map_err(|e| backend("update bucket index", e))?;
    }
    Ok(bucket)
}

fn read_encoded<T: DeserializeOwned>(
    database: &Database,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    operation: &'static str,
) -> Result<Option<T>, MetadataError> {
    let read = database
        .begin_read()
        .map_err(|e| backend("begin read", e))?;
    let table = read
        .open_table(definition)
        .map_err(|e| backend("open table", e))?;
    decode_optional(
        table
            .get(key)
            .map_err(|e| backend(operation, e))?
            .map(|v| v.value().to_vec()),
    )
}
fn read_tx<T: DeserializeOwned>(
    write: &redb::WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    operation: &'static str,
) -> Result<Option<T>, MetadataError> {
    let table = write
        .open_table(definition)
        .map_err(|e| backend("open table", e))?;
    decode_optional(
        table
            .get(key)
            .map_err(|e| backend(operation, e))?
            .map(|v| v.value().to_vec()),
    )
}
fn decode_optional<T: DeserializeOwned>(
    bytes: Option<Vec<u8>>,
) -> Result<Option<T>, MetadataError> {
    bytes
        .map(|value| serde_json::from_slice(&value))
        .transpose()
        .map_err(MetadataError::from)
}
fn read_bucket(
    write: &redb::WriteTransaction,
    id: BucketId,
) -> Result<Option<Bucket>, MetadataError> {
    read_tx(write, BUCKETS, &bucket_key(id), "read bucket")
}
fn read_bucket_usage(
    write: &redb::WriteTransaction,
    id: BucketId,
) -> Result<BucketUsage, MetadataError> {
    Ok(read_tx(write, BUCKET_USAGE, &bucket_key(id), "read bucket usage")?.unwrap_or_default())
}
fn write_bucket_usage(
    write: &redb::WriteTransaction,
    id: BucketId,
    usage: BucketUsage,
) -> Result<(), MetadataError> {
    let bytes = serde_json::to_vec(&usage)?;
    let mut table = write
        .open_table(BUCKET_USAGE)
        .map_err(|e| backend("open bucket usage", e))?;
    table
        .insert(bucket_key(id).as_slice(), bytes.as_slice())
        .map_err(|e| backend("write bucket usage", e))?;
    Ok(())
}

fn insert_version(
    write: &redb::WriteTransaction,
    record: &ObjectVersionRecord,
) -> Result<(), MetadataError> {
    let bytes = serde_json::to_vec(record)?;
    let id = record.version_id().as_uuid().as_bytes().to_vec();
    {
        let mut table = write
            .open_table(VERSIONS)
            .map_err(|e| backend("open versions", e))?;
        table
            .insert(id.as_slice(), bytes.as_slice())
            .map_err(|e| backend("insert version", e))?;
    }
    {
        let mut table = write
            .open_table(VERSION_ORDER)
            .map_err(|e| backend("open version order", e))?;
        table
            .insert(version_order_key(record).as_slice(), id.as_slice())
            .map_err(|e| backend("index version", e))?;
    }
    Ok(())
}
fn remove_version(
    write: &redb::WriteTransaction,
    record: &ObjectVersionRecord,
) -> Result<(), MetadataError> {
    {
        let mut table = write
            .open_table(VERSIONS)
            .map_err(|e| backend("open versions", e))?;
        table
            .remove(record.version_id().as_uuid().as_bytes().as_slice())
            .map_err(|e| backend("remove version", e))?;
    }
    {
        let mut table = write
            .open_table(VERSION_ORDER)
            .map_err(|e| backend("open version order", e))?;
        table
            .remove(version_order_key(record).as_slice())
            .map_err(|e| backend("remove version index", e))?;
    }
    Ok(())
}
fn take_null(
    write: &redb::WriteTransaction,
    bucket: BucketId,
    key: &ObjectKey,
) -> Result<Option<ObjectVersionRecord>, MetadataError> {
    let key = object_key(bucket, key);
    let id = {
        let mut table = write
            .open_table(NULL_VERSIONS)
            .map_err(|e| backend("open null versions", e))?;
        table
            .remove(key.as_slice())
            .map_err(|e| backend("remove null index", e))?
            .map(|v| v.value().to_vec())
    };
    id.map(|id| {
        read_tx(write, VERSIONS, &id, "read null version")?.ok_or_else(|| MetadataError::Database {
            operation: "read null version",
            reason: "inconsistent index".into(),
        })
    })
    .transpose()
}
fn set_null(
    write: &redb::WriteTransaction,
    key: &[u8],
    id: VersionId,
) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(NULL_VERSIONS)
        .map_err(|e| backend("open null versions", e))?;
    table
        .insert(key, id.as_uuid().as_bytes().as_slice())
        .map_err(|e| backend("index null version", e))?;
    Ok(())
}
fn clear_null(
    write: &redb::WriteTransaction,
    key: &[u8],
    id: VersionId,
) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(NULL_VERSIONS)
        .map_err(|e| backend("open null versions", e))?;
    let matches = table
        .get(key)
        .map_err(|e| backend("read null version", e))?
        .is_some_and(|v| v.value() == id.as_uuid().as_bytes().as_slice());
    if matches {
        table
            .remove(key)
            .map_err(|e| backend("remove null version", e))?;
    }
    Ok(())
}

fn latest_version(
    write: &redb::WriteTransaction,
    bucket: BucketId,
    key: &ObjectKey,
) -> Result<Option<ObjectVersionRecord>, MetadataError> {
    let prefix = exact_version_prefix(bucket, key);
    let end = prefix_successor(&prefix);
    let table = write
        .open_table(VERSION_ORDER)
        .map_err(|e| backend("open version order", e))?;
    let Some(entry) = table
        .range(prefix.as_slice()..end.as_slice())
        .map_err(|e| backend("range versions", e))?
        .next()
    else {
        return Ok(None);
    };
    let (_, id) = entry.map_err(|e| backend("read latest version", e))?;
    read_tx(write, VERSIONS, id.value(), "resolve latest version")
}
fn publish_current(
    write: &redb::WriteTransaction,
    key: &[u8],
    record: &ObjectVersionRecord,
) -> Result<(), MetadataError> {
    match record {
        ObjectVersionRecord::Object { metadata, .. } => {
            let bytes = serde_json::to_vec(metadata)?;
            let mut table = write
                .open_table(OBJECTS)
                .map_err(|e| backend("open objects", e))?;
            table
                .insert(key, bytes.as_slice())
                .map_err(|e| backend("publish current", e))?;
        }
        ObjectVersionRecord::DeleteMarker { marker, .. } => {
            let bytes = serde_json::to_vec(marker)?;
            let mut table = write
                .open_table(MARKERS)
                .map_err(|e| backend("open markers", e))?;
            table
                .insert(key, bytes.as_slice())
                .map_err(|e| backend("publish marker", e))?;
        }
    }
    Ok(())
}
fn remove_current(write: &redb::WriteTransaction, key: &[u8]) -> Result<(), MetadataError> {
    {
        let mut table = write
            .open_table(OBJECTS)
            .map_err(|e| backend("open objects", e))?;
        table
            .remove(key)
            .map_err(|e| backend("remove current", e))?;
    }
    {
        let mut table = write
            .open_table(MARKERS)
            .map_err(|e| backend("open markers", e))?;
        table.remove(key).map_err(|e| backend("remove marker", e))?;
    }
    Ok(())
}
fn current_version(
    write: &redb::WriteTransaction,
    key: &[u8],
) -> Result<Option<VersionId>, MetadataError> {
    if let Some(metadata) = read_tx::<ObjectMetadata>(write, OBJECTS, key, "read current")? {
        return Ok(Some(metadata.version_id));
    }
    Ok(read_tx::<DeleteMarker>(write, MARKERS, key, "read marker")?.map(|m| m.version_id))
}
fn current_version_read(
    read: &redb::ReadTransaction,
    key: &[u8],
) -> Result<Option<VersionId>, MetadataError> {
    let objects = read
        .open_table(OBJECTS)
        .map_err(|e| backend("open objects", e))?;
    if let Some(bytes) = objects
        .get(key)
        .map_err(|e| backend("read current", e))?
        .map(|v| v.value().to_vec())
    {
        return Ok(Some(
            serde_json::from_slice::<ObjectMetadata>(&bytes)?.version_id,
        ));
    }
    let markers = read
        .open_table(MARKERS)
        .map_err(|e| backend("open markers", e))?;
    Ok(markers
        .get(key)
        .map_err(|e| backend("read marker", e))?
        .map(|v| serde_json::from_slice::<DeleteMarker>(v.value()))
        .transpose()?
        .map(|m| m.version_id))
}

fn list_parts_tx(
    write: &redb::WriteTransaction,
    id: UploadId,
) -> Result<Vec<UploadedPart>, MetadataError> {
    let table = write
        .open_table(PARTS)
        .map_err(|e| backend("open parts", e))?;
    let prefix = id.as_uuid().as_bytes().as_slice().to_vec();
    let end = prefix_successor(&prefix);
    let mut out = Vec::new();
    for entry in table
        .range(prefix.as_slice()..end.as_slice())
        .map_err(|e| backend("range parts", e))?
    {
        let (_, value) = entry.map_err(|e| backend("read part", e))?;
        out.push(serde_json::from_slice(value.value())?);
    }
    Ok(out)
}
fn has_multipart(write: &redb::WriteTransaction, bucket: BucketId) -> Result<bool, MetadataError> {
    let table = write
        .open_table(MULTIPART_ORDER)
        .map_err(|e| backend("open multipart order", e))?;
    let prefix = bucket_key(bucket);
    let end = prefix_successor(&prefix);
    Ok(table
        .range(prefix.as_slice()..end.as_slice())
        .map_err(|e| backend("range multipart", e))?
        .next()
        .is_some())
}
fn as_object(record: &ObjectVersionRecord) -> Option<&ObjectMetadata> {
    match record {
        ObjectVersionRecord::Object { metadata, .. } => Some(metadata),
        ObjectVersionRecord::DeleteMarker { .. } => None,
    }
}
fn record_matches(record: &ObjectVersionRecord, bucket: BucketId, key: &ObjectKey) -> bool {
    match record {
        ObjectVersionRecord::Object { metadata, .. } => {
            metadata.bucket_id == bucket && metadata.key == *key
        }
        ObjectVersionRecord::DeleteMarker { marker, .. } => {
            marker.bucket_id == bucket && marker.key == *key
        }
    }
}

fn bucket_key(id: BucketId) -> Vec<u8> {
    id.as_uuid().as_bytes().as_slice().to_vec()
}
fn object_key(bucket: BucketId, key: &ObjectKey) -> Vec<u8> {
    object_prefix(bucket, key.as_str())
}
fn object_prefix(bucket: BucketId, prefix: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + prefix.len());
    out.extend_from_slice(bucket.as_uuid().as_bytes().as_slice());
    out.extend_from_slice(prefix.as_bytes());
    out
}
fn exact_version_prefix(bucket: BucketId, key: &ObjectKey) -> Vec<u8> {
    let mut out = object_key(bucket, key);
    out.push(0);
    out
}
fn version_order_key(record: &ObjectVersionRecord) -> Vec<u8> {
    let bucket = match record {
        ObjectVersionRecord::Object { metadata, .. } => metadata.bucket_id,
        ObjectVersionRecord::DeleteMarker { marker, .. } => marker.bucket_id,
    };
    let mut out = exact_version_prefix(bucket, record.key());
    let inverted = u64::MAX - record.created_at().timestamp_micros().max(0) as u64;
    out.extend_from_slice(&inverted.to_be_bytes());
    out.extend_from_slice(record.version_id().as_uuid().as_bytes().as_slice());
    out
}
fn multipart_order_key(upload: &MultipartUpload) -> Vec<u8> {
    let mut out = object_key(upload.bucket_id, &upload.key);
    out.push(0);
    out.extend_from_slice(&(upload.initiated_at.timestamp_micros().max(0) as u64).to_be_bytes());
    out.extend_from_slice(upload.id.as_uuid().as_bytes().as_slice());
    out
}
fn part_key(id: UploadId, number: PartNumber) -> Vec<u8> {
    let mut out = id.as_uuid().as_bytes().as_slice().to_vec();
    out.extend_from_slice(&number.get().to_be_bytes());
    out
}
fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    for index in (0..out.len()).rev() {
        if out[index] != u8::MAX {
            out[index] += 1;
            out.truncate(index + 1);
            return out;
        }
    }
    out.push(u8::MAX);
    out
}

fn queue_cleanup(write: &redb::WriteTransaction, id: ObjectId) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(CLEANUP)
        .map_err(|e| backend("open cleanup", e))?;
    table
        .insert(id.as_uuid().as_bytes().as_slice(), &1)
        .map_err(|e| backend("queue cleanup", e))?;
    Ok(())
}
fn read_counter(
    table: &impl ReadableTable<&'static str, u64>,
    name: &'static str,
) -> Result<u64, MetadataError> {
    Ok(table
        .get(name)
        .map_err(|e| backend("read counter", e))?
        .map_or(0, |v| v.value()))
}
fn adjust_counter(
    write: &redb::WriteTransaction,
    name: &'static str,
    delta: impl Into<i128>,
) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(COUNTERS)
        .map_err(|e| backend("open counters", e))?;
    let value = i128::from(read_counter(&table, name)?)
        .checked_add(delta.into())
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(counter_error)?;
    table
        .insert(name, &value)
        .map_err(|e| backend("write counter", e))?;
    Ok(())
}
fn counter_error() -> MetadataError {
    MetadataError::Database {
        operation: "adjust counter",
        reason: "counter overflow or underflow".into(),
    }
}
fn backend(operation: &'static str, error: impl Display) -> MetadataError {
    MetadataError::Database {
        operation,
        reason: error.to_string(),
    }
}

/// A newly generated delete marker supplied by the caller.
///
/// Delete markers carry a fresh version identifier and timestamp. Generating
/// them outside the catalog keeps command application deterministic, which is
/// what allows the catalog to be replicated through consensus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewDeleteMarker {
    /// Version identifier the marker will be published under.
    pub version_id: VersionId,
    /// Time the marker becomes current.
    pub created_at: DateTime<Utc>,
}

impl NewDeleteMarker {
    /// Generates a marker for a delete that is about to be proposed.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            version_id: VersionId::new(),
            created_at: Utc::now(),
        }
    }
}

/// One deterministic mutation of the durable object catalog.
///
/// Commands carry every non-deterministic input so that applying the same
/// ordered sequence on any node yields identical state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum MetadataCommand {
    /// Create a bucket.
    CreateBucket {
        /// Bucket to create.
        bucket: Box<Bucket>,
    },
    /// Change a bucket's versioning state.
    SetBucketVersioning {
        /// Bucket to change.
        bucket_id: BucketId,
        /// Requested versioning state.
        state: VersioningState,
    },
    /// Change a bucket's quota.
    SetBucketQuota {
        /// Bucket to change.
        bucket_id: BucketId,
        /// Requested quota.
        quota: BucketQuota,
    },
    /// Replace or remove a bucket's browser CORS configuration.
    SetBucketCors {
        /// Bucket to change.
        bucket_id: BucketId,
        /// Complete replacement configuration, or `None` to remove it.
        configuration: Option<CorsConfiguration>,
    },
    /// Delete an empty bucket.
    DeleteBucket {
        /// Bucket name.
        name: BucketName,
    },
    /// Publish an immutable object version and make it current.
    PutObject {
        /// Object metadata to publish.
        metadata: Box<ObjectMetadata>,
    },
    /// Apply ordinary delete semantics for the current version of a key.
    DeleteObject {
        /// Owning bucket.
        bucket_id: BucketId,
        /// Logical key.
        key: ObjectKey,
        /// Marker values to use when the bucket keeps history.
        marker: NewDeleteMarker,
    },
    /// Permanently delete one explicit version.
    DeleteObjectVersion {
        /// Owning bucket.
        bucket_id: BucketId,
        /// Logical key.
        key: ObjectKey,
        /// Version to remove.
        version_id: VersionId,
    },
    /// Create a multipart upload.
    CreateMultipartUpload {
        /// Upload descriptor.
        upload: Box<MultipartUpload>,
    },
    /// Publish one durable multipart part.
    PutMultipartPart {
        /// Part to publish.
        part: Box<UploadedPart>,
    },
    /// Mark a multipart upload as completing under a preallocated payload.
    BeginMultipartCompletion {
        /// Upload identifier.
        upload_id: UploadId,
        /// Preallocated payload identifier.
        object_id: ObjectId,
    },
    /// Remove multipart state after a successful completion.
    FinishMultipartUpload {
        /// Upload identifier.
        upload_id: UploadId,
    },
    /// Remove multipart state after an abort.
    AbortMultipartUpload {
        /// Upload identifier.
        upload_id: UploadId,
    },
    /// Reconcile multipart uploads interrupted mid-completion.
    RecoverMultipartCompletions,
    /// Create or replace a lifecycle rule.
    PutLifecycleRule {
        /// Rule to store.
        rule: Box<LifecycleRule>,
    },
    /// Remove a lifecycle rule.
    DeleteLifecycleRule {
        /// Rule identifier.
        rule_id: LifecycleRuleId,
    },
    /// Clear a durable payload cleanup record.
    CompleteCleanup {
        /// Payload identifier.
        object_id: ObjectId,
    },
}

impl MetadataCommand {
    /// Returns a stable short name for tracing, metrics, and audit records.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::CreateBucket { .. } => "create_bucket",
            Self::SetBucketVersioning { .. } => "set_bucket_versioning",
            Self::SetBucketQuota { .. } => "set_bucket_quota",
            Self::SetBucketCors { .. } => "set_bucket_cors",
            Self::DeleteBucket { .. } => "delete_bucket",
            Self::PutObject { .. } => "put_object",
            Self::DeleteObject { .. } => "delete_object",
            Self::DeleteObjectVersion { .. } => "delete_object_version",
            Self::CreateMultipartUpload { .. } => "create_multipart_upload",
            Self::PutMultipartPart { .. } => "put_multipart_part",
            Self::BeginMultipartCompletion { .. } => "begin_multipart_completion",
            Self::FinishMultipartUpload { .. } => "finish_multipart_upload",
            Self::AbortMultipartUpload { .. } => "abort_multipart_upload",
            Self::RecoverMultipartCompletions => "recover_multipart_completions",
            Self::PutLifecycleRule { .. } => "put_lifecycle_rule",
            Self::DeleteLifecycleRule { .. } => "delete_lifecycle_rule",
            Self::CompleteCleanup { .. } => "complete_cleanup",
        }
    }
}

/// Result of applying one metadata command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum MetadataOutcome {
    /// The command produced no value.
    None,
    /// A bucket record after the command was applied.
    Bucket(Box<Bucket>),
    /// The payloads an object commit made unreachable.
    ObjectCommit(ObjectCommitResult),
    /// The result of an ordinary delete.
    DeleteObject(DeleteObjectResult),
    /// The result of deleting an explicit version.
    ///
    /// Boxed because it is by far the largest outcome and would otherwise set
    /// the size of every command response.
    DeleteVersion(Option<Box<DeleteVersionResult>>),
    /// A multipart upload descriptor.
    MultipartUpload(Box<MultipartUpload>),
    /// The part a publish replaced, when it replaced one.
    ReplacedPart(Option<Box<UploadedPart>>),
    /// Multipart state removed by completion, abort, or recovery.
    MultipartCleanup(MultipartCleanupResult),
}

fn unexpected(expected: &'static str) -> MetadataError {
    MetadataError::Database {
        operation: "decode command outcome",
        reason: format!("expected a {expected} outcome"),
    }
}

impl MetadataOutcome {
    /// Returns the bucket produced by the command.
    pub fn into_bucket(self) -> Result<Bucket, MetadataError> {
        match self {
            Self::Bucket(bucket) => Ok(*bucket),
            _ => Err(unexpected("bucket")),
        }
    }

    /// Returns the object commit result produced by the command.
    pub fn into_object_commit(self) -> Result<ObjectCommitResult, MetadataError> {
        match self {
            Self::ObjectCommit(result) => Ok(result),
            _ => Err(unexpected("object commit")),
        }
    }

    /// Returns the delete result produced by the command.
    pub fn into_delete_object(self) -> Result<DeleteObjectResult, MetadataError> {
        match self {
            Self::DeleteObject(result) => Ok(result),
            _ => Err(unexpected("object delete")),
        }
    }

    /// Returns the version delete result produced by the command.
    pub fn into_delete_version(self) -> Result<Option<DeleteVersionResult>, MetadataError> {
        match self {
            Self::DeleteVersion(result) => Ok(result.map(|result| *result)),
            _ => Err(unexpected("version delete")),
        }
    }

    /// Returns the multipart upload produced by the command.
    pub fn into_multipart_upload(self) -> Result<MultipartUpload, MetadataError> {
        match self {
            Self::MultipartUpload(upload) => Ok(*upload),
            _ => Err(unexpected("multipart upload")),
        }
    }

    /// Returns the part a publish replaced.
    pub fn into_replaced_part(self) -> Result<Option<UploadedPart>, MetadataError> {
        match self {
            Self::ReplacedPart(part) => Ok(part.map(|part| *part)),
            _ => Err(unexpected("multipart part")),
        }
    }

    /// Returns the multipart state removed by the command.
    pub fn into_multipart_cleanup(self) -> Result<MultipartCleanupResult, MetadataError> {
        match self {
            Self::MultipartCleanup(result) => Ok(result),
            _ => Err(unexpected("multipart cleanup")),
        }
    }
}

/// Applies one metadata command inside a caller-provided transaction.
///
/// Keeping the transaction external lets a consensus state machine commit the
/// state change and its applied log position atomically, so a crash can neither
/// apply an entry twice nor lose one.
pub fn apply_command_tx(
    write: &redb::WriteTransaction,
    command: MetadataCommand,
) -> Result<MetadataOutcome, MetadataError> {
    match command {
        MetadataCommand::CreateBucket { bucket } => {
            create_bucket_tx(write, &bucket)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::SetBucketVersioning { bucket_id, state } => {
            let bucket = update_bucket_tx(write, bucket_id, move |bucket| {
                if bucket.versioning == VersioningState::Enabled
                    && state == VersioningState::Disabled
                {
                    return Err(MetadataError::InvalidVersioningTransition);
                }
                bucket.versioning = state;
                Ok(())
            })?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::SetBucketQuota { bucket_id, quota } => {
            let usage = read_bucket_usage(write, bucket_id)?;
            if !quota
                .bytes
                .allows(usage.version_bytes.saturating_add(usage.multipart_bytes))
                || !quota.objects.allows(usage.current_objects)
            {
                return Err(MetadataError::QuotaExceeded);
            }
            let bucket = update_bucket_tx(write, bucket_id, move |bucket| {
                bucket.quota = quota;
                Ok(())
            })?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::SetBucketCors {
            bucket_id,
            configuration,
        } => {
            let bucket = update_bucket_tx(write, bucket_id, move |bucket| {
                bucket.cors = configuration;
                Ok(())
            })?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::DeleteBucket { name } => {
            let bucket = delete_bucket_tx(write, &name)?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::PutObject { metadata } => Ok(MetadataOutcome::ObjectCommit(
            put_object_tx(write, &metadata)?,
        )),
        MetadataCommand::DeleteObject {
            bucket_id,
            key,
            marker,
        } => Ok(MetadataOutcome::DeleteObject(delete_object_tx(
            write, bucket_id, &key, marker,
        )?)),
        MetadataCommand::DeleteObjectVersion {
            bucket_id,
            key,
            version_id,
        } => Ok(MetadataOutcome::DeleteVersion(
            delete_object_version_tx(write, bucket_id, &key, version_id)?.map(Box::new),
        )),
        MetadataCommand::CreateMultipartUpload { upload } => {
            create_multipart_upload_tx(write, &upload)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::PutMultipartPart { part } => Ok(MetadataOutcome::ReplacedPart(
            put_multipart_part_tx(write, &part)?.map(Box::new),
        )),
        MetadataCommand::BeginMultipartCompletion {
            upload_id,
            object_id,
        } => Ok(MetadataOutcome::MultipartUpload(Box::new(
            begin_multipart_completion_tx(write, upload_id, object_id)?,
        ))),
        MetadataCommand::FinishMultipartUpload { upload_id } => Ok(
            MetadataOutcome::MultipartCleanup(remove_multipart_tx(write, upload_id, true)?),
        ),
        MetadataCommand::AbortMultipartUpload { upload_id } => Ok(
            MetadataOutcome::MultipartCleanup(remove_multipart_tx(write, upload_id, false)?),
        ),
        MetadataCommand::RecoverMultipartCompletions => Ok(MetadataOutcome::MultipartCleanup(
            recover_multipart_completions_tx(write)?,
        )),
        MetadataCommand::PutLifecycleRule { rule } => {
            put_lifecycle_rule_tx(write, &rule)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::DeleteLifecycleRule { rule_id } => {
            delete_lifecycle_rule_tx(write, rule_id)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::CompleteCleanup { object_id } => {
            complete_cleanup_tx(write, object_id)?;
            Ok(MetadataOutcome::None)
        }
    }
}

fn create_bucket_tx(write: &redb::WriteTransaction, bucket: &Bucket) -> Result<(), MetadataError> {
    let key = bucket_key(bucket.id);
    let encoded = serde_json::to_vec(bucket)?;
    {
        let ids = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open buckets", e))?;
        let names = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        if ids
            .get(key.as_slice())
            .map_err(|e| backend("read bucket", e))?
            .is_some()
            || names
                .get(bucket.name.as_str())
                .map_err(|e| backend("read bucket name", e))?
                .is_some()
        {
            return Err(MetadataError::BucketAlreadyExists);
        }
    }
    {
        let mut table = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open buckets", e))?;
        table
            .insert(key.as_slice(), encoded.as_slice())
            .map_err(|e| backend("insert bucket", e))?;
    }
    {
        let mut table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        table
            .insert(bucket.name.as_str(), encoded.as_slice())
            .map_err(|e| backend("index bucket", e))?;
    }
    write_bucket_usage(write, bucket.id, BucketUsage::default())?;
    adjust_counter(write, BUCKET_COUNT, 1)
}

fn delete_bucket_tx(
    write: &redb::WriteTransaction,
    name: &BucketName,
) -> Result<Bucket, MetadataError> {
    let bucket: Bucket = {
        let table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        let bytes = table
            .get(name.as_str())
            .map_err(|e| backend("read bucket", e))?
            .map(|v| v.value().to_vec())
            .ok_or(MetadataError::BucketNotFound)?;
        serde_json::from_slice(&bytes)?
    };
    if read_bucket_usage(write, bucket.id)?.versions != 0 || has_multipart(write, bucket.id)? {
        return Err(MetadataError::BucketNotEmpty);
    }
    {
        let mut table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        table
            .remove(name.as_str())
            .map_err(|e| backend("remove bucket name", e))?;
    }
    {
        let mut table = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open buckets", e))?;
        table
            .remove(bucket_key(bucket.id).as_slice())
            .map_err(|e| backend("remove bucket", e))?;
    }
    {
        let mut table = write
            .open_table(BUCKET_USAGE)
            .map_err(|e| backend("open bucket usage", e))?;
        table
            .remove(bucket_key(bucket.id).as_slice())
            .map_err(|e| backend("remove bucket usage", e))?;
    }
    {
        let mut table = write
            .open_table(LIFECYCLE_RULES)
            .map_err(|e| backend("open lifecycle rules", e))?;
        let mut ids = Vec::new();
        for item in table
            .iter()
            .map_err(|e| backend("iterate lifecycle rules", e))?
        {
            let (id, value) = item.map_err(|e| backend("read lifecycle rule", e))?;
            let rule: LifecycleRule = serde_json::from_slice(value.value())?;
            if rule.bucket_id == bucket.id {
                ids.push(id.value().to_vec());
            }
        }
        for id in ids {
            table
                .remove(id.as_slice())
                .map_err(|e| backend("remove lifecycle rule", e))?;
        }
    }
    adjust_counter(write, BUCKET_COUNT, -1)?;
    Ok(bucket)
}

fn put_object_tx(
    write: &redb::WriteTransaction,
    metadata: &ObjectMetadata,
) -> Result<ObjectCommitResult, MetadataError> {
    let bucket = read_bucket(write, metadata.bucket_id)?.ok_or(MetadataError::BucketNotFound)?;
    let key = object_key(metadata.bucket_id, &metadata.key);
    let previous: Option<ObjectMetadata> = read_tx(write, OBJECTS, &key, "read current object")?;
    let is_null = bucket.versioning != VersioningState::Enabled;
    let replaced = if is_null {
        take_null(write, metadata.bucket_id, &metadata.key)?
    } else {
        None
    };
    let removed_bytes = replaced.as_ref().and_then(as_object).map_or(0, |m| m.size);
    let mut usage = read_bucket_usage(write, metadata.bucket_id)?;
    let proposed_objects = usage.current_objects + u64::from(previous.is_none());
    let proposed_logical = usage
        .logical_bytes
        .checked_sub(previous.as_ref().map_or(0, |m| m.size))
        .and_then(|v| v.checked_add(metadata.size))
        .ok_or_else(counter_error)?;
    let proposed_versions = usage.versions - u64::from(replaced.is_some()) + 1;
    let proposed_version_bytes = usage
        .version_bytes
        .checked_sub(removed_bytes)
        .and_then(|v| v.checked_add(metadata.size))
        .ok_or_else(counter_error)?;
    if !bucket
        .quota
        .bytes
        .allows(proposed_version_bytes.saturating_add(usage.multipart_bytes))
        || !bucket.quota.objects.allows(proposed_objects)
    {
        return Err(MetadataError::QuotaExceeded);
    }
    if let Some(record) = &replaced {
        remove_version(write, record)?;
    }
    let record = ObjectVersionRecord::Object {
        metadata: metadata.clone(),
        is_null,
    };
    insert_version(write, &record)?;
    if is_null {
        set_null(write, &key, metadata.version_id)?;
    }
    {
        let mut table = write
            .open_table(OBJECTS)
            .map_err(|e| backend("open objects", e))?;
        let bytes = serde_json::to_vec(&metadata)?;
        table
            .insert(key.as_slice(), bytes.as_slice())
            .map_err(|e| backend("publish object", e))?;
    }
    {
        let mut table = write
            .open_table(MARKERS)
            .map_err(|e| backend("open markers", e))?;
        table
            .remove(key.as_slice())
            .map_err(|e| backend("remove marker", e))?;
    }
    usage.current_objects = proposed_objects;
    usage.logical_bytes = proposed_logical;
    usage.versions = proposed_versions;
    usage.version_bytes = proposed_version_bytes;
    write_bucket_usage(write, metadata.bucket_id, usage)?;
    adjust_counter(write, OBJECT_COUNT, i128::from(previous.is_none()))?;
    adjust_counter(
        write,
        LOGICAL_BYTES,
        i128::from(metadata.size) - i128::from(previous.as_ref().map_or(0, |m| m.size)),
    )?;
    adjust_counter(write, VERSION_COUNT, 1 - i128::from(replaced.is_some()))?;
    adjust_counter(
        write,
        VERSION_BYTES,
        i128::from(metadata.size) - i128::from(removed_bytes),
    )?;
    adjust_counter(
        write,
        PHYSICAL_BYTES,
        i128::from(metadata.size) - i128::from(removed_bytes),
    )?;
    let cleanup: Vec<_> = replaced
        .as_ref()
        .and_then(as_object)
        .cloned()
        .into_iter()
        .collect();
    for old in &cleanup {
        queue_cleanup(write, old.id)?;
    }
    Ok(ObjectCommitResult { cleanup })
}

fn delete_object_tx(
    write: &redb::WriteTransaction,
    bucket: BucketId,
    object_key_value: &ObjectKey,
    marker_values: NewDeleteMarker,
) -> Result<DeleteObjectResult, MetadataError> {
    let bucket_record = read_bucket(write, bucket)?.ok_or(MetadataError::BucketNotFound)?;
    let key = object_key(bucket, object_key_value);
    let current: Option<ObjectMetadata> = read_tx(write, OBJECTS, &key, "read current object")?;
    let mut usage = read_bucket_usage(write, bucket)?;
    let mut result = DeleteObjectResult {
        previously_visible: current.is_some(),
        ..DeleteObjectResult::default()
    };
    match bucket_record.versioning {
        VersioningState::Disabled => {
            if let Some(record) = take_null(write, bucket, object_key_value)? {
                remove_version(write, &record)?;
                usage.versions -= 1;
                adjust_counter(write, VERSION_COUNT, -1)?;
                if let Some(metadata) = as_object(&record) {
                    usage.version_bytes -= metadata.size;
                    adjust_counter(write, VERSION_BYTES, -i128::from(metadata.size))?;
                    adjust_counter(write, PHYSICAL_BYTES, -i128::from(metadata.size))?;
                    queue_cleanup(write, metadata.id)?;
                    result.cleanup.push(metadata.clone());
                }
            }
            remove_current(write, &key)?;
        }
        VersioningState::Enabled | VersioningState::Suspended => {
            let is_null = bucket_record.versioning == VersioningState::Suspended;
            if is_null && let Some(record) = take_null(write, bucket, object_key_value)? {
                remove_version(write, &record)?;
                usage.versions -= 1;
                adjust_counter(write, VERSION_COUNT, -1)?;
                if let Some(metadata) = as_object(&record) {
                    usage.version_bytes -= metadata.size;
                    adjust_counter(write, VERSION_BYTES, -i128::from(metadata.size))?;
                    adjust_counter(write, PHYSICAL_BYTES, -i128::from(metadata.size))?;
                    queue_cleanup(write, metadata.id)?;
                    result.cleanup.push(metadata.clone());
                }
            }
            let marker = DeleteMarker {
                version_id: marker_values.version_id,
                bucket_id: bucket,
                key: object_key_value.clone(),
                created_at: marker_values.created_at,
            };
            let record = ObjectVersionRecord::DeleteMarker {
                marker: marker.clone(),
                is_null,
            };
            insert_version(write, &record)?;
            if is_null {
                set_null(write, &key, marker.version_id)?;
            }
            {
                let mut objects = write
                    .open_table(OBJECTS)
                    .map_err(|e| backend("open objects", e))?;
                objects
                    .remove(key.as_slice())
                    .map_err(|e| backend("hide object", e))?;
            }
            {
                let mut markers = write
                    .open_table(MARKERS)
                    .map_err(|e| backend("open markers", e))?;
                let bytes = serde_json::to_vec(&marker)?;
                markers
                    .insert(key.as_slice(), bytes.as_slice())
                    .map_err(|e| backend("publish marker", e))?;
            }
            usage.versions += 1;
            adjust_counter(write, VERSION_COUNT, 1)?;
            result.delete_marker = Some(marker);
        }
    }
    if let Some(metadata) = current {
        usage.current_objects -= 1;
        usage.logical_bytes -= metadata.size;
        adjust_counter(write, OBJECT_COUNT, -1)?;
        adjust_counter(write, LOGICAL_BYTES, -i128::from(metadata.size))?;
    }
    write_bucket_usage(write, bucket, usage)?;
    Ok(result)
}

fn delete_object_version_tx(
    write: &redb::WriteTransaction,
    bucket: BucketId,
    object_key_value: &ObjectKey,
    version: VersionId,
) -> Result<Option<DeleteVersionResult>, MetadataError> {
    let Some(record): Option<ObjectVersionRecord> = read_tx(
        write,
        VERSIONS,
        version.as_uuid().as_bytes().as_slice(),
        "read version",
    )?
    else {
        return Ok(None);
    };
    if !record_matches(&record, bucket, object_key_value) {
        return Ok(None);
    }
    let key = object_key(bucket, object_key_value);
    let was_current = current_version(write, &key)? == Some(version);
    remove_version(write, &record)?;
    if record.is_null() {
        clear_null(write, &key, version)?;
    }
    let mut usage = read_bucket_usage(write, bucket)?;
    usage.versions -= 1;
    adjust_counter(write, VERSION_COUNT, -1)?;
    let cleanup = as_object(&record).cloned();
    if let Some(metadata) = &cleanup {
        usage.version_bytes -= metadata.size;
        adjust_counter(write, VERSION_BYTES, -i128::from(metadata.size))?;
        adjust_counter(write, PHYSICAL_BYTES, -i128::from(metadata.size))?;
        queue_cleanup(write, metadata.id)?;
    }
    if was_current {
        if let Some(metadata) =
            read_tx::<ObjectMetadata>(write, OBJECTS, &key, "read current object")?
        {
            usage.current_objects -= 1;
            usage.logical_bytes -= metadata.size;
            adjust_counter(write, OBJECT_COUNT, -1)?;
            adjust_counter(write, LOGICAL_BYTES, -i128::from(metadata.size))?;
        }
        remove_current(write, &key)?;
        if let Some(next) = latest_version(write, bucket, object_key_value)? {
            publish_current(write, &key, &next)?;
            if let Some(metadata) = as_object(&next) {
                usage.current_objects += 1;
                usage.logical_bytes += metadata.size;
                adjust_counter(write, OBJECT_COUNT, 1)?;
                adjust_counter(write, LOGICAL_BYTES, i128::from(metadata.size))?;
            }
        }
    }
    write_bucket_usage(write, bucket, usage)?;
    Ok(Some(DeleteVersionResult {
        removed: record,
        cleanup,
    }))
}

fn create_multipart_upload_tx(
    write: &redb::WriteTransaction,
    upload: &MultipartUpload,
) -> Result<(), MetadataError> {
    if read_bucket(write, upload.bucket_id)?.is_none() {
        return Err(MetadataError::BucketNotFound);
    }
    let id = upload.id.as_uuid().as_bytes().to_vec();
    {
        let mut table = write
            .open_table(MULTIPART)
            .map_err(|e| backend("open multipart", e))?;
        if table
            .get(id.as_slice())
            .map_err(|e| backend("read multipart", e))?
            .is_some()
        {
            return Err(MetadataError::MultipartStateConflict);
        }
        let bytes = serde_json::to_vec(upload)?;
        table
            .insert(id.as_slice(), bytes.as_slice())
            .map_err(|e| backend("insert multipart", e))?;
    }
    {
        let mut table = write
            .open_table(MULTIPART_ORDER)
            .map_err(|e| backend("open multipart order", e))?;
        table
            .insert(multipart_order_key(upload).as_slice(), id.as_slice())
            .map_err(|e| backend("index multipart", e))?;
    }
    Ok(())
}

fn put_multipart_part_tx(
    write: &redb::WriteTransaction,
    part: &UploadedPart,
) -> Result<Option<UploadedPart>, MetadataError> {
    let upload: MultipartUpload = read_tx(
        write,
        MULTIPART,
        part.upload_id.as_uuid().as_bytes().as_slice(),
        "read multipart",
    )?
    .ok_or(MetadataError::MultipartUploadNotFound)?;
    if upload.state != MultipartUploadState::Active {
        return Err(MetadataError::MultipartStateConflict);
    }
    let bucket = read_bucket(write, upload.bucket_id)?.ok_or(MetadataError::BucketNotFound)?;
    let key = part_key(part.upload_id, part.number);
    let previous: Option<UploadedPart> = read_tx(write, PARTS, &key, "read part")?;
    let mut usage = read_bucket_usage(write, upload.bucket_id)?;
    let proposed = usage
        .multipart_bytes
        .checked_sub(previous.as_ref().map_or(0, |p| p.size))
        .and_then(|v| v.checked_add(part.size))
        .ok_or_else(counter_error)?;
    if !bucket
        .quota
        .bytes
        .allows(usage.version_bytes.saturating_add(proposed))
    {
        return Err(MetadataError::QuotaExceeded);
    }
    {
        let mut table = write
            .open_table(PARTS)
            .map_err(|e| backend("open parts", e))?;
        let bytes = serde_json::to_vec(part)?;
        table
            .insert(key.as_slice(), bytes.as_slice())
            .map_err(|e| backend("insert part", e))?;
    }
    usage.multipart_bytes = proposed;
    write_bucket_usage(write, upload.bucket_id, usage)?;
    adjust_counter(
        write,
        MULTIPART_BYTES,
        i128::from(part.size) - i128::from(previous.as_ref().map_or(0, |p| p.size)),
    )?;
    if let Some(old) = &previous {
        queue_cleanup(write, old.object_id)?;
    }
    Ok(previous)
}

fn begin_multipart_completion_tx(
    write: &redb::WriteTransaction,
    id: UploadId,
    object_id: ObjectId,
) -> Result<MultipartUpload, MetadataError> {
    let mut upload: MultipartUpload = read_tx(
        write,
        MULTIPART,
        id.as_uuid().as_bytes().as_slice(),
        "read multipart",
    )?
    .ok_or(MetadataError::MultipartUploadNotFound)?;
    match upload.state {
        MultipartUploadState::Active => {
            upload.state = MultipartUploadState::Completing { object_id }
        }
        MultipartUploadState::Completing {
            object_id: existing,
        } if existing == object_id => {}
        MultipartUploadState::Completing { .. } => {
            return Err(MetadataError::MultipartStateConflict);
        }
    }
    {
        let mut table = write
            .open_table(MULTIPART)
            .map_err(|e| backend("open multipart", e))?;
        let bytes = serde_json::to_vec(&upload)?;
        table
            .insert(id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
            .map_err(|e| backend("mark completion", e))?;
    }
    Ok(upload)
}

fn remove_multipart_tx(
    write: &redb::WriteTransaction,
    id: UploadId,
    require_completing: bool,
) -> Result<MultipartCleanupResult, MetadataError> {
    let upload: MultipartUpload = read_tx(
        write,
        MULTIPART,
        id.as_uuid().as_bytes().as_slice(),
        "read multipart",
    )?
    .ok_or(MetadataError::MultipartUploadNotFound)?;
    if require_completing && !matches!(upload.state, MultipartUploadState::Completing { .. }) {
        return Err(MetadataError::MultipartStateConflict);
    }
    let parts = list_parts_tx(write, id)?;
    {
        let mut table = write
            .open_table(MULTIPART)
            .map_err(|e| backend("open multipart", e))?;
        table
            .remove(id.as_uuid().as_bytes().as_slice())
            .map_err(|e| backend("remove multipart", e))?;
    }
    {
        let mut table = write
            .open_table(MULTIPART_ORDER)
            .map_err(|e| backend("open multipart order", e))?;
        table
            .remove(multipart_order_key(&upload).as_slice())
            .map_err(|e| backend("remove multipart index", e))?;
    }
    {
        let mut table = write
            .open_table(PARTS)
            .map_err(|e| backend("open parts", e))?;
        for part in &parts {
            table
                .remove(part_key(id, part.number).as_slice())
                .map_err(|e| backend("remove part", e))?;
        }
    }
    let bytes = parts.iter().try_fold(0_u64, |sum, part| {
        sum.checked_add(part.size).ok_or_else(counter_error)
    })?;
    let mut usage = read_bucket_usage(write, upload.bucket_id)?;
    usage.multipart_bytes = usage
        .multipart_bytes
        .checked_sub(bytes)
        .ok_or_else(counter_error)?;
    write_bucket_usage(write, upload.bucket_id, usage)?;
    adjust_counter(write, MULTIPART_BYTES, -i128::from(bytes))?;
    for part in &parts {
        queue_cleanup(write, part.object_id)?;
    }
    Ok(MultipartCleanupResult { parts })
}

fn recover_multipart_completions_tx(
    write: &redb::WriteTransaction,
) -> Result<MultipartCleanupResult, MetadataError> {
    let uploads = {
        let table = write
            .open_table(MULTIPART)
            .map_err(|e| backend("open multipart", e))?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(|e| backend("iterate multipart", e))? {
            let (_, value) = entry.map_err(|e| backend("read multipart", e))?;
            let upload: MultipartUpload = serde_json::from_slice(value.value())?;
            if matches!(upload.state, MultipartUploadState::Completing { .. }) {
                out.push(upload);
            }
        }
        out
    };
    let mut cleaned = Vec::new();
    for upload in uploads {
        let MultipartUploadState::Completing { object_id } = upload.state else {
            continue;
        };
        let key = object_key(upload.bucket_id, &upload.key);
        let committed = read_tx::<ObjectMetadata>(write, OBJECTS, &key, "read current object")?
            .is_some_and(|metadata| metadata.id == object_id);
        if committed {
            cleaned.extend(remove_multipart_tx(write, upload.id, true)?.parts);
        } else {
            let mut reset = upload;
            reset.state = MultipartUploadState::Active;
            let mut table = write
                .open_table(MULTIPART)
                .map_err(|e| backend("open multipart", e))?;
            let bytes = serde_json::to_vec(&reset)?;
            table
                .insert(reset.id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
                .map_err(|e| backend("reset completion", e))?;
        }
    }
    Ok(MultipartCleanupResult { parts: cleaned })
}

fn put_lifecycle_rule_tx(
    write: &redb::WriteTransaction,
    rule: &LifecycleRule,
) -> Result<(), MetadataError> {
    rule.validate()
        .map_err(|error| MetadataError::InvalidLifecycleRule(error.to_string()))?;
    if read_bucket(write, rule.bucket_id)?.is_none() {
        return Err(MetadataError::BucketNotFound);
    }
    let mut table = write
        .open_table(LIFECYCLE_RULES)
        .map_err(|e| backend("open lifecycle rules", e))?;
    let bytes = serde_json::to_vec(rule)?;
    table
        .insert(rule.id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
        .map_err(|e| backend("put lifecycle rule", e))?;
    Ok(())
}

fn delete_lifecycle_rule_tx(
    write: &redb::WriteTransaction,
    id: LifecycleRuleId,
) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(LIFECYCLE_RULES)
        .map_err(|e| backend("open lifecycle rules", e))?;
    if table
        .remove(id.as_uuid().as_bytes().as_slice())
        .map_err(|e| backend("delete lifecycle rule", e))?
        .is_none()
    {
        return Err(MetadataError::LifecycleRuleNotFound);
    }
    Ok(())
}

fn complete_cleanup_tx(write: &redb::WriteTransaction, id: ObjectId) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(CLEANUP)
        .map_err(|e| backend("open cleanup", e))?;
    table
        .remove(id.as_uuid().as_bytes().as_slice())
        .map_err(|e| backend("complete cleanup", e))?;
    Ok(())
}

/// One raw catalog key/value pair used by consensus snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataEntry {
    /// Table the pair belongs to.
    pub table: String,
    /// Raw key bytes.
    pub key: Vec<u8>,
    /// Raw value bytes.
    pub value: Vec<u8>,
}

const BYTE_TABLES: &[TableDefinition<'static, &'static [u8], &'static [u8]>] = &[
    BUCKETS,
    OBJECTS,
    MARKERS,
    VERSIONS,
    VERSION_ORDER,
    NULL_VERSIONS,
    MULTIPART,
    MULTIPART_ORDER,
    PARTS,
    BUCKET_USAGE,
    LIFECYCLE_RULES,
];

/// Exports the whole object catalog for a consensus snapshot.
///
/// A read transaction is used so that a snapshot is a consistent point-in-time
/// view without blocking concurrent command application.
pub fn export_tx(write: &redb::ReadTransaction) -> Result<Vec<MetadataEntry>, MetadataError> {
    let mut entries = Vec::new();
    for definition in BYTE_TABLES {
        let table = write
            .open_table(*definition)
            .map_err(|e| backend("open snapshot table", e))?;
        for item in table
            .iter()
            .map_err(|e| backend("scan snapshot table", e))?
        {
            let (key, value) = item.map_err(|e| backend("read snapshot record", e))?;
            entries.push(MetadataEntry {
                table: definition.name().to_owned(),
                key: key.value().to_vec(),
                value: value.value().to_vec(),
            });
        }
    }
    {
        let table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open snapshot bucket names", e))?;
        for item in table.iter().map_err(|e| backend("scan bucket names", e))? {
            let (key, value) = item.map_err(|e| backend("read bucket name", e))?;
            entries.push(MetadataEntry {
                table: BUCKET_NAMES.name().to_owned(),
                key: key.value().as_bytes().to_vec(),
                value: value.value().to_vec(),
            });
        }
    }
    {
        let table = write
            .open_table(CLEANUP)
            .map_err(|e| backend("open snapshot cleanup", e))?;
        for item in table.iter().map_err(|e| backend("scan cleanup", e))? {
            let (key, value) = item.map_err(|e| backend("read cleanup", e))?;
            entries.push(MetadataEntry {
                table: CLEANUP.name().to_owned(),
                key: key.value().to_vec(),
                value: vec![value.value()],
            });
        }
    }
    for definition in [COUNTERS, SCHEMA] {
        let table = write
            .open_table(definition)
            .map_err(|e| backend("open snapshot counters", e))?;
        for item in table.iter().map_err(|e| backend("scan counters", e))? {
            let (key, value) = item.map_err(|e| backend("read counter", e))?;
            entries.push(MetadataEntry {
                table: definition.name().to_owned(),
                key: key.value().as_bytes().to_vec(),
                value: value.value().to_be_bytes().to_vec(),
            });
        }
    }
    Ok(entries)
}

/// Replaces the whole object catalog from a consensus snapshot.
pub fn import_tx(
    write: &redb::WriteTransaction,
    entries: &[MetadataEntry],
) -> Result<(), MetadataError> {
    for definition in BYTE_TABLES {
        let mut table = write
            .open_table(*definition)
            .map_err(|e| backend("open snapshot table", e))?;
        table
            .retain(|_, _| false)
            .map_err(|e| backend("clear snapshot table", e))?;
    }
    {
        let mut table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open snapshot bucket names", e))?;
        table
            .retain(|_, _| false)
            .map_err(|e| backend("clear bucket names", e))?;
    }
    {
        let mut table = write
            .open_table(CLEANUP)
            .map_err(|e| backend("open snapshot cleanup", e))?;
        table
            .retain(|_, _| false)
            .map_err(|e| backend("clear cleanup", e))?;
    }
    for definition in [COUNTERS, SCHEMA] {
        let mut table = write
            .open_table(definition)
            .map_err(|e| backend("open snapshot counters", e))?;
        table
            .retain(|_, _| false)
            .map_err(|e| backend("clear counters", e))?;
    }
    let byte_tables: std::collections::BTreeMap<&str, TableDefinition<&[u8], &[u8]>> = BYTE_TABLES
        .iter()
        .map(|definition| (definition.name(), *definition))
        .collect();
    for entry in entries {
        if let Some(definition) = byte_tables.get(entry.table.as_str()) {
            let mut table = write
                .open_table(*definition)
                .map_err(|e| backend("open snapshot table", e))?;
            table
                .insert(entry.key.as_slice(), entry.value.as_slice())
                .map_err(|e| backend("restore snapshot record", e))?;
        } else if entry.table == BUCKET_NAMES.name() {
            let name = std::str::from_utf8(&entry.key).map_err(|_| MetadataError::Database {
                operation: "restore bucket name",
                reason: "bucket name key is not valid UTF-8".into(),
            })?;
            let mut table = write
                .open_table(BUCKET_NAMES)
                .map_err(|e| backend("open snapshot bucket names", e))?;
            table
                .insert(name, entry.value.as_slice())
                .map_err(|e| backend("restore bucket name", e))?;
        } else if entry.table == CLEANUP.name() {
            let [flag] = entry.value[..] else {
                return Err(MetadataError::Database {
                    operation: "restore cleanup record",
                    reason: "cleanup value must be one byte".into(),
                });
            };
            let mut table = write
                .open_table(CLEANUP)
                .map_err(|e| backend("open snapshot cleanup", e))?;
            table
                .insert(entry.key.as_slice(), flag)
                .map_err(|e| backend("restore cleanup record", e))?;
        } else if entry.table == COUNTERS.name() || entry.table == SCHEMA.name() {
            let name = std::str::from_utf8(&entry.key).map_err(|_| MetadataError::Database {
                operation: "restore counter",
                reason: "counter key is not valid UTF-8".into(),
            })?;
            let bytes: [u8; 8] =
                entry
                    .value
                    .as_slice()
                    .try_into()
                    .map_err(|_| MetadataError::Database {
                        operation: "restore counter",
                        reason: "counter value must be eight bytes".into(),
                    })?;
            let definition = if entry.table == COUNTERS.name() {
                COUNTERS
            } else {
                SCHEMA
            };
            let mut table = write
                .open_table(definition)
                .map_err(|e| backend("open snapshot counters", e))?;
            table
                .insert(name, u64::from_be_bytes(bytes))
                .map_err(|e| backend("restore counter", e))?;
        } else {
            return Err(MetadataError::Database {
                operation: "restore snapshot",
                reason: format!("snapshot references unknown table '{}'", entry.table),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oes_core::{Checksum, CorsMethod, CorsPattern, CorsRule, ETag, OrganizationId};
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn bucket(name: &str) -> Bucket {
        Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new(name).expect("bucket"),
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
            durability_policy: None,
            cors: None,
        }
    }
    fn object(bucket: BucketId, key: &str, size: u64) -> ObjectMetadata {
        let now = Utc::now();
        ObjectMetadata {
            id: ObjectId::new(),
            bucket_id: bucket,
            key: ObjectKey::new(key).expect("key"),
            version_id: VersionId::new(),
            size,
            checksum: Checksum::sha256([1; 32]),
            payload_format: oes_core::PayloadFormat::Plaintext,
            durability: oes_core::DurabilityProfile::Single,
            etag: ETag::from_md5([2; 16]),
            content_type: None,
            custom_metadata: BTreeMap::new(),
            created_at: now,
            modified_at: now,
        }
    }

    fn cors_configuration() -> CorsConfiguration {
        CorsConfiguration {
            rules: vec![CorsRule {
                id: Some("browser-upload".into()),
                allowed_origins: vec![
                    CorsPattern::origin("https://app.example.com").expect("origin"),
                ],
                allowed_methods: vec![CorsMethod::Put, CorsMethod::Get],
                allowed_headers: vec![CorsPattern::header("x-amz-*").expect("header")],
                expose_headers: vec!["ETag".into()],
                max_age_seconds: Some(600),
            }],
        }
    }

    #[tokio::test]
    async fn applying_the_same_commands_produces_identical_state() {
        // Determinism is what makes the catalog usable as a consensus state
        // machine: two members replaying one command sequence must agree.
        let bucket_record = bucket("deterministic");
        let first_object = object(bucket_record.id, "alpha", 10);
        let second_object = object(bucket_record.id, "beta", 20);
        let marker = NewDeleteMarker::generate();
        let commands = vec![
            MetadataCommand::CreateBucket {
                bucket: Box::new(bucket_record.clone()),
            },
            MetadataCommand::SetBucketVersioning {
                bucket_id: bucket_record.id,
                state: VersioningState::Enabled,
            },
            MetadataCommand::SetBucketCors {
                bucket_id: bucket_record.id,
                configuration: Some(cors_configuration()),
            },
            MetadataCommand::PutObject {
                metadata: Box::new(first_object.clone()),
            },
            MetadataCommand::PutObject {
                metadata: Box::new(second_object.clone()),
            },
            MetadataCommand::DeleteObject {
                bucket_id: bucket_record.id,
                key: first_object.key.clone(),
                marker,
            },
        ];

        let mut snapshots = Vec::new();
        for _ in 0..2 {
            let dir = tempdir().expect("temp");
            let repo = RedbMetadataRepository::open(dir.path().join("catalog.redb"))
                .await
                .expect("repo");
            for command in commands.clone() {
                repo.command(command).await.expect("apply command");
            }
            let database = repo.database();
            let entries = tokio::task::spawn_blocking(move || {
                let read = database.begin_read().expect("begin");
                export_tx(&read).expect("export")
            })
            .await
            .expect("join");
            snapshots.push(entries);
        }
        assert_eq!(
            snapshots[0], snapshots[1],
            "replaying identical commands must produce identical durable state"
        );
    }

    #[tokio::test]
    async fn snapshot_export_and_import_restore_the_catalog() {
        let dir = tempdir().expect("temp");
        let repo = RedbMetadataRepository::open(dir.path().join("catalog.redb"))
            .await
            .expect("repo");
        let bucket_record = bucket("snapshot-bucket");
        repo.create_bucket(&bucket_record).await.expect("bucket");
        let stored = object(bucket_record.id, "nested/key", 42);
        repo.put_object(&stored).await.expect("put");
        let database = repo.database();
        let entries = tokio::task::spawn_blocking(move || {
            let read = database.begin_read().expect("begin");
            export_tx(&read).expect("export")
        })
        .await
        .expect("join");

        let other_dir = tempdir().expect("temp");
        let other = RedbMetadataRepository::open(other_dir.path().join("catalog.redb"))
            .await
            .expect("repo");
        let other_database = other.database();
        tokio::task::spawn_blocking(move || {
            let write = other_database.begin_write().expect("begin");
            import_tx(&write, &entries).expect("import");
            write.commit().expect("commit");
        })
        .await
        .expect("join");

        let restored = other
            .get_object(bucket_record.id, &stored.key)
            .await
            .expect("read")
            .expect("object must exist after import");
        assert_eq!(restored, stored);
        assert_eq!(
            other
                .get_bucket_by_name(&bucket_record.name)
                .await
                .expect("read")
                .map(|bucket| bucket.id),
            Some(bucket_record.id)
        );
        let usage = other.storage_usage().await.expect("usage");
        assert_eq!(usage.object_count, 1);
        assert_eq!(usage.bytes_used, 42);
    }

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
            payload_format: oes_core::PayloadFormat::Plaintext,
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
