//! Durable metadata repository boundaries and a single-node embedded catalog.

use std::{collections::BTreeMap, fmt::Display, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oes_core::{
    Bucket, BucketId, BucketName, Checksum, ETag, ObjectId, ObjectKey, ObjectMetadata,
    StorageUsage, VersionId,
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::Deserialize;
use thiserror::Error;

const BUCKETS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("buckets.v1");
const BUCKET_NAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("bucket_names.v1");
const OBJECTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("objects.v1");
const CLEANUP: TableDefinition<&[u8], u8> = TableDefinition::new("payload_cleanup.v1");
const COUNTERS: TableDefinition<&str, u64> = TableDefinition::new("counters.v1");
const SCHEMA: TableDefinition<&str, u64> = TableDefinition::new("schema.v1");
const SCHEMA_VERSION: &str = "metadata";
const CURRENT_SCHEMA_VERSION: u64 = 2;
const OBJECT_COUNT: &str = "objects";
const BUCKET_COUNT: &str = "buckets";
const BYTE_COUNT: &str = "bytes";

/// Bounded ordered object-listing input.
#[derive(Debug, Clone)]
pub struct ListObjectsRequest {
    /// Bucket being listed.
    pub bucket_id: BucketId,
    /// Optional lexical key prefix.
    pub prefix: String,
    /// Exclusive key after which iteration begins.
    pub start_after: Option<String>,
    /// Maximum records returned by this repository page.
    pub limit: usize,
}

/// Bounded ordered object metadata page.
#[derive(Debug, Clone)]
pub struct ObjectMetadataPage {
    /// Records in ascending key order.
    pub objects: Vec<ObjectMetadata>,
    /// Last returned key when more matching records exist.
    pub next_key: Option<String>,
}

/// Durable metadata operations required by storage and service layers.
#[async_trait]
pub trait MetadataRepository: Send + Sync {
    /// Adds a bucket with globally unique name and identifier.
    async fn create_bucket(&self, bucket: &Bucket) -> Result<(), MetadataError>;
    /// Looks up a bucket by its stable identifier.
    async fn get_bucket(&self, bucket_id: BucketId) -> Result<Option<Bucket>, MetadataError>;
    /// Looks up a bucket by its validated name.
    async fn get_bucket_by_name(&self, name: &BucketName) -> Result<Option<Bucket>, MetadataError>;
    /// Lists buckets in ascending name order.
    async fn list_buckets(&self) -> Result<Vec<Bucket>, MetadataError>;
    /// Deletes an empty bucket atomically.
    async fn delete_bucket(&self, name: &BucketName) -> Result<Bucket, MetadataError>;
    /// Atomically publishes object metadata and queues replaced payload cleanup.
    async fn put_object(
        &self,
        metadata: &ObjectMetadata,
    ) -> Result<Option<ObjectMetadata>, MetadataError>;
    /// Looks up the currently visible object for a bucket and key.
    async fn get_object(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectMetadata>, MetadataError>;
    /// Removes metadata and queues physical payload cleanup.
    async fn delete_object(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectMetadata>, MetadataError>;
    /// Lists a bounded ordered page using an indexed key range.
    async fn list_objects(
        &self,
        request: ListObjectsRequest,
    ) -> Result<ObjectMetadataPage, MetadataError>;
    /// Returns transactionally maintained aggregate counters.
    async fn storage_usage(&self) -> Result<StorageUsage, MetadataError>;
    /// Returns payload identifiers whose physical deletion must be retried.
    async fn pending_cleanup(&self, limit: usize) -> Result<Vec<ObjectId>, MetadataError>;
    /// Marks a queued physical payload deletion complete.
    async fn complete_cleanup(&self, object_id: ObjectId) -> Result<(), MetadataError>;
    /// Verifies that the repository can commit a write transaction.
    async fn check_ready(&self) -> Result<(), MetadataError>;
}

/// Metadata repository failures with stable operational categories.
#[derive(Debug, Error)]
pub enum MetadataError {
    /// A metadata database location could not be prepared.
    #[error("failed to prepare metadata directory: {0}")]
    Directory(#[source] std::io::Error),
    /// A bucket with the same name or identifier already exists.
    #[error("bucket already exists")]
    BucketAlreadyExists,
    /// The requested bucket does not exist.
    #[error("bucket was not found")]
    BucketNotFound,
    /// A bucket still contains committed objects.
    #[error("bucket is not empty")]
    BucketNotEmpty,
    /// Metadata serialization or deserialization failed.
    #[error("metadata encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    /// The embedded database failed during a named operation.
    #[error("metadata database operation '{operation}' failed: {reason}")]
    Database {
        /// Stable operation name.
        operation: &'static str,
        /// Backend failure detail intended for internal logs.
        reason: String,
    },
    /// A blocking database task could not be completed.
    #[error("metadata task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// A Redb-backed repository suitable for a durable standalone node.
#[derive(Clone)]
pub struct RedbMetadataRepository {
    database: Arc<Database>,
}

impl RedbMetadataRepository {
    /// Opens or creates the catalog and initializes versioned tables.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, MetadataError> {
        let path = path.as_ref().to_path_buf();
        let parent = path.parent().map(Path::to_path_buf);
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = parent {
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
}

#[async_trait]
impl MetadataRepository for RedbMetadataRepository {
    async fn create_bucket(&self, bucket: &Bucket) -> Result<(), MetadataError> {
        let database = Arc::clone(&self.database);
        let id_key = bucket_key(bucket.id);
        let name = bucket.name.as_str().to_owned();
        let encoded = serde_json::to_vec(bucket)?;
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin create bucket", error))?;
            {
                let by_id = write
                    .open_table(BUCKETS)
                    .map_err(|error| backend("open buckets", error))?;
                let by_name = write
                    .open_table(BUCKET_NAMES)
                    .map_err(|error| backend("open bucket names", error))?;
                if by_id
                    .get(id_key.as_slice())
                    .map_err(|error| backend("read bucket", error))?
                    .is_some()
                    || by_name
                        .get(name.as_str())
                        .map_err(|error| backend("read bucket name", error))?
                        .is_some()
                {
                    return Err(MetadataError::BucketAlreadyExists);
                }
            }
            {
                let mut by_id = write
                    .open_table(BUCKETS)
                    .map_err(|error| backend("open buckets", error))?;
                by_id
                    .insert(id_key.as_slice(), encoded.as_slice())
                    .map_err(|error| backend("insert bucket", error))?;
            }
            {
                let mut by_name = write
                    .open_table(BUCKET_NAMES)
                    .map_err(|error| backend("open bucket names", error))?;
                by_name
                    .insert(name.as_str(), encoded.as_slice())
                    .map_err(|error| backend("index bucket name", error))?;
            }
            adjust_counter(&write, BUCKET_COUNT, 1)?;
            write
                .commit()
                .map_err(|error| backend("commit bucket", error))
        })
        .await?
    }

    async fn get_bucket(&self, bucket_id: BucketId) -> Result<Option<Bucket>, MetadataError> {
        let database = Arc::clone(&self.database);
        let key = bucket_key(bucket_id);
        tokio::task::spawn_blocking(move || {
            read_encoded(&database, BUCKETS, key.as_slice(), "bucket")
        })
        .await?
    }

    async fn get_bucket_by_name(&self, name: &BucketName) -> Result<Option<Bucket>, MetadataError> {
        let database = Arc::clone(&self.database);
        let name = name.as_str().to_owned();
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| backend("begin read bucket name", error))?;
            let table = read
                .open_table(BUCKET_NAMES)
                .map_err(|error| backend("open bucket names", error))?;
            decode_optional(
                table
                    .get(name.as_str())
                    .map_err(|error| backend("read bucket name", error))?
                    .map(|value| value.value().to_vec()),
            )
        })
        .await?
    }

    async fn list_buckets(&self) -> Result<Vec<Bucket>, MetadataError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| backend("begin list buckets", error))?;
            let table = read
                .open_table(BUCKET_NAMES)
                .map_err(|error| backend("open bucket names", error))?;
            let mut buckets = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("iterate buckets", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read bucket entry", error))?;
                buckets.push(serde_json::from_slice(value.value())?);
            }
            Ok(buckets)
        })
        .await?
    }

    async fn delete_bucket(&self, name: &BucketName) -> Result<Bucket, MetadataError> {
        let database = Arc::clone(&self.database);
        let name = name.as_str().to_owned();
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin delete bucket", error))?;
            let bucket: Bucket = {
                let names = write
                    .open_table(BUCKET_NAMES)
                    .map_err(|error| backend("open bucket names", error))?;
                let encoded = names
                    .get(name.as_str())
                    .map_err(|error| backend("read bucket name", error))?
                    .map(|value| value.value().to_vec())
                    .ok_or(MetadataError::BucketNotFound)?;
                serde_json::from_slice(&encoded)?
            };
            {
                let objects = write
                    .open_table(OBJECTS)
                    .map_err(|error| backend("open objects", error))?;
                let start = bucket_prefix(bucket.id);
                let end = prefix_successor(&start);
                let mut range = objects
                    .range(start.as_slice()..end.as_slice())
                    .map_err(|error| backend("check bucket contents", error))?;
                if range.next().is_some() {
                    return Err(MetadataError::BucketNotEmpty);
                }
            }
            {
                let mut names = write
                    .open_table(BUCKET_NAMES)
                    .map_err(|error| backend("open bucket names", error))?;
                names
                    .remove(name.as_str())
                    .map_err(|error| backend("remove bucket name", error))?;
            }
            {
                let mut buckets = write
                    .open_table(BUCKETS)
                    .map_err(|error| backend("open buckets", error))?;
                let key = bucket_key(bucket.id);
                buckets
                    .remove(key.as_slice())
                    .map_err(|error| backend("remove bucket", error))?;
            }
            adjust_counter(&write, BUCKET_COUNT, -1)?;
            write
                .commit()
                .map_err(|error| backend("commit bucket deletion", error))?;
            Ok(bucket)
        })
        .await?
    }

    async fn put_object(
        &self,
        metadata: &ObjectMetadata,
    ) -> Result<Option<ObjectMetadata>, MetadataError> {
        let database = Arc::clone(&self.database);
        let key = object_key(metadata.bucket_id, &metadata.key);
        let encoded = serde_json::to_vec(metadata)?;
        let new_size = metadata.size;
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin put object", error))?;
            let previous: Option<ObjectMetadata> = {
                let mut table = write
                    .open_table(OBJECTS)
                    .map_err(|error| backend("open objects", error))?;
                let previous = table
                    .get(key.as_slice())
                    .map_err(|error| backend("read object", error))?
                    .map(|value| value.value().to_vec());
                table
                    .insert(key.as_slice(), encoded.as_slice())
                    .map_err(|error| backend("insert object", error))?;
                decode_optional(previous)?
            };
            if let Some(previous) = &previous {
                queue_cleanup(&write, previous.id)?;
                adjust_counter(
                    &write,
                    BYTE_COUNT,
                    i128::from(new_size) - i128::from(previous.size),
                )?;
            } else {
                adjust_counter(&write, OBJECT_COUNT, 1)?;
                adjust_counter(&write, BYTE_COUNT, i128::from(new_size))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit object", error))?;
            Ok(previous)
        })
        .await?
    }

    async fn get_object(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectMetadata>, MetadataError> {
        let database = Arc::clone(&self.database);
        let key = object_key(bucket_id, key);
        tokio::task::spawn_blocking(move || {
            read_encoded(&database, OBJECTS, key.as_slice(), "object")
        })
        .await?
    }

    async fn delete_object(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectMetadata>, MetadataError> {
        let database = Arc::clone(&self.database);
        let key = object_key(bucket_id, key);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin delete object", error))?;
            let removed: Option<ObjectMetadata> = {
                let mut table = write
                    .open_table(OBJECTS)
                    .map_err(|error| backend("open objects", error))?;
                let removed = table
                    .remove(key.as_slice())
                    .map_err(|error| backend("delete object", error))?
                    .map(|value| value.value().to_vec());
                decode_optional(removed)?
            };
            if let Some(metadata) = &removed {
                queue_cleanup(&write, metadata.id)?;
                adjust_counter(&write, OBJECT_COUNT, -1)?;
                adjust_counter(&write, BYTE_COUNT, -i128::from(metadata.size))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit object deletion", error))?;
            Ok(removed)
        })
        .await?
    }

    async fn list_objects(
        &self,
        request: ListObjectsRequest,
    ) -> Result<ObjectMetadataPage, MetadataError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            if request.limit == 0 {
                return Ok(ObjectMetadataPage {
                    objects: Vec::new(),
                    next_key: None,
                });
            }
            let read = database
                .begin_read()
                .map_err(|error| backend("begin list objects", error))?;
            let table = read
                .open_table(OBJECTS)
                .map_err(|error| backend("open objects", error))?;
            let prefix = object_key_prefix(request.bucket_id, &request.prefix);
            let mut start = request.start_after.as_ref().map_or_else(
                || prefix.clone(),
                |key| object_key_prefix(request.bucket_id, key),
            );
            if request.start_after.is_some() {
                start.push(0);
            }
            let end = prefix_successor(&prefix);
            let mut objects = Vec::with_capacity(request.limit.min(1_000));
            let mut range = table
                .range(start.as_slice()..end.as_slice())
                .map_err(|error| backend("range objects", error))?;
            while objects.len() < request.limit + 1 {
                let Some(entry) = range.next() else {
                    break;
                };
                let (_, value) = entry.map_err(|error| backend("read object entry", error))?;
                objects.push(serde_json::from_slice::<ObjectMetadata>(value.value())?);
            }
            let next_key = if objects.len() > request.limit {
                objects.pop();
                objects
                    .last()
                    .map(|metadata| metadata.key.as_str().to_owned())
            } else {
                None
            };
            Ok(ObjectMetadataPage { objects, next_key })
        })
        .await?
    }

    async fn storage_usage(&self) -> Result<StorageUsage, MetadataError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| backend("begin read counters", error))?;
            let counters = read
                .open_table(COUNTERS)
                .map_err(|error| backend("open counters", error))?;
            Ok(StorageUsage {
                object_count: read_counter(&counters, OBJECT_COUNT)?,
                bytes_used: read_counter(&counters, BYTE_COUNT)?,
                bucket_count: read_counter(&counters, BUCKET_COUNT)?,
            })
        })
        .await?
    }

    async fn pending_cleanup(&self, limit: usize) -> Result<Vec<ObjectId>, MetadataError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| backend("begin list cleanup", error))?;
            let table = read
                .open_table(CLEANUP)
                .map_err(|error| backend("open cleanup", error))?;
            let mut ids = Vec::with_capacity(limit.min(1_000));
            for entry in table
                .iter()
                .map_err(|error| backend("iterate cleanup", error))?
                .take(limit)
            {
                let (key, _) = entry.map_err(|error| backend("read cleanup entry", error))?;
                let bytes: [u8; 16] =
                    key.value()
                        .try_into()
                        .map_err(|_| MetadataError::Database {
                            operation: "decode cleanup identifier",
                            reason: "cleanup key length is invalid".into(),
                        })?;
                ids.push(ObjectId::from_uuid(uuid::Uuid::from_bytes(bytes)));
            }
            Ok(ids)
        })
        .await?
    }

    async fn complete_cleanup(&self, object_id: ObjectId) -> Result<(), MetadataError> {
        let database = Arc::clone(&self.database);
        let key = object_id.as_uuid().as_bytes().to_vec();
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin complete cleanup", error))?;
            {
                let mut table = write
                    .open_table(CLEANUP)
                    .map_err(|error| backend("open cleanup", error))?;
                table
                    .remove(key.as_slice())
                    .map_err(|error| backend("remove cleanup", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit cleanup", error))
        })
        .await?
    }

    async fn check_ready(&self) -> Result<(), MetadataError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("readiness transaction", error))?;
            {
                write
                    .open_table(OBJECTS)
                    .map_err(|error| backend("readiness table", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit readiness transaction", error))
        })
        .await?
    }
}

fn initialize_schema(database: &Database) -> Result<(), MetadataError> {
    let write = database
        .begin_write()
        .map_err(|error| backend("initialize", error))?;
    {
        write
            .open_table(BUCKETS)
            .map_err(|error| backend("initialize buckets", error))?;
        write
            .open_table(BUCKET_NAMES)
            .map_err(|error| backend("initialize bucket names", error))?;
        write
            .open_table(OBJECTS)
            .map_err(|error| backend("initialize objects", error))?;
        write
            .open_table(CLEANUP)
            .map_err(|error| backend("initialize cleanup", error))?;
        write
            .open_table(COUNTERS)
            .map_err(|error| backend("initialize counters", error))?;
        write
            .open_table(SCHEMA)
            .map_err(|error| backend("initialize schema version", error))?;
    }
    let version = {
        let schema = write
            .open_table(SCHEMA)
            .map_err(|error| backend("open schema version", error))?;
        schema
            .get(SCHEMA_VERSION)
            .map_err(|error| backend("read schema version", error))?
            .map_or(0, |value| value.value())
    };
    if version < CURRENT_SCHEMA_VERSION {
        let buckets = {
            let table = write
                .open_table(BUCKETS)
                .map_err(|error| backend("open buckets for migration", error))?;
            let mut buckets = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("iterate buckets for migration", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read migration bucket", error))?;
                buckets.push(serde_json::from_slice::<Bucket>(value.value())?);
            }
            buckets
        };
        let objects = {
            let table = write
                .open_table(OBJECTS)
                .map_err(|error| backend("open objects for migration", error))?;
            let mut objects = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("iterate objects for migration", error))?
            {
                let (key, value) =
                    entry.map_err(|error| backend("read migration object", error))?;
                objects.push((
                    key.value().to_vec(),
                    decode_migrating_object(value.value())?,
                ));
            }
            objects
        };
        {
            let mut names = write
                .open_table(BUCKET_NAMES)
                .map_err(|error| backend("open bucket names for migration", error))?;
            for bucket in &buckets {
                let encoded = serde_json::to_vec(bucket)?;
                names
                    .insert(bucket.name.as_str(), encoded.as_slice())
                    .map_err(|error| backend("index migrated bucket", error))?;
            }
        }
        {
            let mut table = write
                .open_table(OBJECTS)
                .map_err(|error| backend("open objects for migration update", error))?;
            for (key, metadata) in &objects {
                let encoded = serde_json::to_vec(metadata)?;
                table
                    .insert(key.as_slice(), encoded.as_slice())
                    .map_err(|error| backend("update migrated object", error))?;
            }
        }
        let bytes_used = objects.iter().try_fold(0_u64, |total, (_, metadata)| {
            total
                .checked_add(metadata.size)
                .ok_or_else(|| MetadataError::Database {
                    operation: "migrate counters",
                    reason: "stored byte counter overflow".into(),
                })
        })?;
        {
            let mut counters = write
                .open_table(COUNTERS)
                .map_err(|error| backend("open counters for migration", error))?;
            for (name, value) in [
                (OBJECT_COUNT, objects.len() as u64),
                (BUCKET_COUNT, buckets.len() as u64),
                (BYTE_COUNT, bytes_used),
            ] {
                counters
                    .insert(name, &value)
                    .map_err(|error| backend("write migrated counter", error))?;
            }
        }
        {
            let mut schema = write
                .open_table(SCHEMA)
                .map_err(|error| backend("open schema version for migration", error))?;
            schema
                .insert(SCHEMA_VERSION, &CURRENT_SCHEMA_VERSION)
                .map_err(|error| backend("write schema version", error))?;
        }
    }
    write
        .commit()
        .map_err(|error| backend("commit initialization", error))
}

#[derive(Deserialize)]
struct LegacyObjectMetadata {
    id: ObjectId,
    bucket_id: BucketId,
    key: ObjectKey,
    version_id: VersionId,
    size: u64,
    checksum: Checksum,
    content_type: Option<String>,
    custom_metadata: BTreeMap<String, String>,
    created_at: DateTime<Utc>,
    modified_at: DateTime<Utc>,
}

fn decode_migrating_object(encoded: &[u8]) -> Result<ObjectMetadata, MetadataError> {
    if let Ok(metadata) = serde_json::from_slice::<ObjectMetadata>(encoded) {
        return Ok(metadata);
    }
    let legacy: LegacyObjectMetadata = serde_json::from_slice(encoded)?;
    let checksum = legacy.checksum.to_string();
    let etag = checksum
        .split_once(':')
        .and_then(|(_, digest)| ETag::new(digest.to_owned()).ok())
        .ok_or_else(|| MetadataError::Database {
            operation: "migrate object ETag",
            reason: "legacy checksum cannot form an ETag".into(),
        })?;
    Ok(ObjectMetadata {
        id: legacy.id,
        bucket_id: legacy.bucket_id,
        key: legacy.key,
        version_id: legacy.version_id,
        size: legacy.size,
        checksum: legacy.checksum,
        etag,
        content_type: legacy.content_type,
        custom_metadata: legacy.custom_metadata,
        created_at: legacy.created_at,
        modified_at: legacy.modified_at,
    })
}

fn read_encoded<T>(
    database: &Database,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    entity: &'static str,
) -> Result<Option<T>, MetadataError>
where
    T: serde::de::DeserializeOwned,
{
    let read = database
        .begin_read()
        .map_err(|error| backend("begin read", error))?;
    let table = read
        .open_table(definition)
        .map_err(|error| backend("open table", error))?;
    decode_optional(
        table
            .get(key)
            .map_err(|error| backend(entity, error))?
            .map(|value| value.value().to_vec()),
    )
}

fn decode_optional<T>(encoded: Option<Vec<u8>>) -> Result<Option<T>, MetadataError>
where
    T: serde::de::DeserializeOwned,
{
    encoded
        .map(|value| serde_json::from_slice(&value))
        .transpose()
        .map_err(MetadataError::from)
}

fn bucket_key(bucket_id: BucketId) -> Vec<u8> {
    bucket_id.as_uuid().as_bytes().to_vec()
}

fn bucket_prefix(bucket_id: BucketId) -> Vec<u8> {
    bucket_key(bucket_id)
}

fn object_key(bucket_id: BucketId, key: &ObjectKey) -> Vec<u8> {
    object_key_prefix(bucket_id, key.as_str())
}

fn object_key_prefix(bucket_id: BucketId, prefix: &str) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(16 + prefix.len());
    encoded.extend_from_slice(bucket_id.as_uuid().as_bytes());
    encoded.extend_from_slice(prefix.as_bytes());
    encoded
}

fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut successor = prefix.to_vec();
    for index in (0..successor.len()).rev() {
        if successor[index] != u8::MAX {
            successor[index] += 1;
            successor.truncate(index + 1);
            return successor;
        }
    }
    let mut maximum = prefix.to_vec();
    maximum.push(u8::MAX);
    maximum
}

fn queue_cleanup(write: &redb::WriteTransaction, object_id: ObjectId) -> Result<(), MetadataError> {
    let key = object_id.as_uuid().as_bytes().to_vec();
    let mut table = write
        .open_table(CLEANUP)
        .map_err(|error| backend("open cleanup", error))?;
    table
        .insert(key.as_slice(), &1)
        .map_err(|error| backend("queue cleanup", error))?;
    Ok(())
}

fn read_counter(
    table: &impl ReadableTable<&'static str, u64>,
    name: &'static str,
) -> Result<u64, MetadataError> {
    Ok(table
        .get(name)
        .map_err(|error| backend("read counter", error))?
        .map_or(0, |value| value.value()))
}

fn adjust_counter(
    write: &redb::WriteTransaction,
    name: &'static str,
    change: impl Into<i128>,
) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(COUNTERS)
        .map_err(|error| backend("open counters", error))?;
    let current = read_counter(&table, name)?;
    let updated = i128::from(current)
        .checked_add(change.into())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| MetadataError::Database {
            operation: "adjust counter",
            reason: format!("counter '{name}' overflow or underflow"),
        })?;
    table
        .insert(name, &updated)
        .map_err(|error| backend("write counter", error))?;
    Ok(())
}

fn backend(operation: &'static str, error: impl Display) -> MetadataError {
    MetadataError::Database {
        operation,
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use oes_core::{Checksum, ETag, ObjectId, OrganizationId, VersionId};
    use tempfile::tempdir;

    use super::*;

    fn bucket(name: &str) -> Bucket {
        Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new(name).expect("valid bucket"),
            created_at: Utc::now(),
        }
    }

    fn object(bucket_id: BucketId, key: &str, size: u64) -> ObjectMetadata {
        let now = Utc::now();
        ObjectMetadata {
            id: ObjectId::new(),
            bucket_id,
            key: ObjectKey::new(key).expect("valid key"),
            version_id: VersionId::new(),
            size,
            checksum: Checksum::sha256([7; 32]),
            etag: ETag::from_md5([8; 16]),
            content_type: Some("application/octet-stream".into()),
            custom_metadata: BTreeMap::new(),
            created_at: now,
            modified_at: now,
        }
    }

    #[tokio::test]
    async fn catalog_persists_buckets_objects_listing_counters_and_cleanup() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("catalog.redb");
        let bucket = bucket("durable-bucket");
        let first = object(bucket.id, "a/first", 10);
        let second = object(bucket.id, "a/second", 20);
        {
            let repository = RedbMetadataRepository::open(&path)
                .await
                .expect("open catalog");
            repository
                .create_bucket(&bucket)
                .await
                .expect("create bucket");
            assert!(repository.create_bucket(&bucket).await.is_err());
            repository.put_object(&first).await.expect("first object");
            repository.put_object(&second).await.expect("second object");
        }
        let repository = RedbMetadataRepository::open(&path)
            .await
            .expect("reopen catalog");
        assert_eq!(
            repository
                .get_bucket_by_name(&bucket.name)
                .await
                .expect("bucket"),
            Some(bucket.clone())
        );
        let page = repository
            .list_objects(ListObjectsRequest {
                bucket_id: bucket.id,
                prefix: "a/".into(),
                start_after: None,
                limit: 1,
            })
            .await
            .expect("list");
        assert_eq!(page.objects, vec![first.clone()]);
        assert_eq!(page.next_key, Some(first.key.as_str().to_owned()));
        assert_eq!(
            repository.storage_usage().await.expect("usage"),
            StorageUsage {
                object_count: 2,
                bytes_used: 30,
                bucket_count: 1
            }
        );
        assert!(matches!(
            repository.delete_bucket(&bucket.name).await,
            Err(MetadataError::BucketNotEmpty)
        ));
        repository
            .delete_object(bucket.id, &first.key)
            .await
            .expect("delete first");
        assert_eq!(
            repository.pending_cleanup(10).await.expect("cleanup"),
            vec![first.id]
        );
        repository
            .complete_cleanup(first.id)
            .await
            .expect("complete cleanup");
        repository
            .delete_object(bucket.id, &second.key)
            .await
            .expect("delete second");
        repository
            .delete_bucket(&bucket.name)
            .await
            .expect("delete bucket");
        assert_eq!(
            repository.storage_usage().await.expect("usage"),
            StorageUsage::default()
        );
    }

    #[tokio::test]
    async fn startup_migrates_foundation_records_and_rebuilds_indexes_and_counters() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("legacy.redb");
        let bucket = bucket("legacy-bucket");
        let object = object(bucket.id, "legacy/object", 42);
        {
            let database = Database::create(&path).expect("legacy database");
            let write = database.begin_write().expect("legacy transaction");
            {
                let mut buckets = write.open_table(BUCKETS).expect("legacy buckets");
                let bucket_bytes = serde_json::to_vec(&bucket).expect("bucket encoding");
                buckets
                    .insert(bucket_key(bucket.id).as_slice(), bucket_bytes.as_slice())
                    .expect("legacy bucket insert");
            }
            {
                let mut objects = write.open_table(OBJECTS).expect("legacy objects");
                let mut legacy = serde_json::to_value(&object).expect("object value");
                legacy.as_object_mut().expect("object map").remove("etag");
                let object_bytes = serde_json::to_vec(&legacy).expect("legacy object encoding");
                objects
                    .insert(
                        object_key(bucket.id, &object.key).as_slice(),
                        object_bytes.as_slice(),
                    )
                    .expect("legacy object insert");
            }
            write.commit().expect("legacy commit");
        }

        let repository = RedbMetadataRepository::open(&path)
            .await
            .expect("migrated repository");
        assert_eq!(
            repository
                .get_bucket_by_name(&bucket.name)
                .await
                .expect("bucket lookup"),
            Some(bucket)
        );
        let migrated = repository
            .get_object(object.bucket_id, &object.key)
            .await
            .expect("object lookup")
            .expect("migrated object");
        assert_eq!(migrated.etag.as_str(), "07".repeat(32));
        assert_eq!(
            repository.storage_usage().await.expect("migrated usage"),
            StorageUsage {
                object_count: 1,
                bytes_used: 42,
                bucket_count: 1,
            }
        );
    }
}
