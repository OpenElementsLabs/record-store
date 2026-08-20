//! Durable metadata repository boundaries and a single-node embedded catalog.

use std::{fmt::Display, path::Path, sync::Arc};

use async_trait::async_trait;
use oes_core::{Bucket, BucketId, ObjectKey, ObjectMetadata};
use redb::{Database, ReadableTable, TableDefinition};
use thiserror::Error;

const BUCKETS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("buckets.v1");
const OBJECTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("objects.v1");

/// Durable metadata operations required by the local object store.
#[async_trait]
pub trait MetadataRepository: Send + Sync {
    /// Adds a bucket. Existing bucket identifiers are rejected.
    async fn create_bucket(&self, bucket: &Bucket) -> Result<(), MetadataError>;

    /// Looks up a bucket by its stable identifier.
    async fn get_bucket(&self, bucket_id: BucketId) -> Result<Option<Bucket>, MetadataError>;

    /// Atomically publishes object metadata and returns the previously visible object.
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

    /// Removes and returns the currently visible object for a bucket and key.
    async fn delete_object(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Option<ObjectMetadata>, MetadataError>;

    /// Verifies that the repository can serve transactions.
    async fn check_ready(&self) -> Result<(), MetadataError>;
}

/// Metadata repository failures with stable operational categories.
#[derive(Debug, Error)]
pub enum MetadataError {
    /// A metadata database location could not be prepared.
    #[error("failed to prepare metadata directory: {0}")]
    Directory(#[source] std::io::Error),
    /// A bucket with the same identifier already exists.
    #[error("bucket already exists")]
    BucketAlreadyExists,
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
            let write = database
                .begin_write()
                .map_err(|error| backend("initialize", error))?;
            {
                write
                    .open_table(BUCKETS)
                    .map_err(|error| backend("initialize buckets", error))?;
                write
                    .open_table(OBJECTS)
                    .map_err(|error| backend("initialize objects", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit initialization", error))?;
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
        let key = bucket_key(bucket.id);
        let encoded = serde_json::to_vec(bucket)?;
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin create bucket", error))?;
            {
                let mut table = write
                    .open_table(BUCKETS)
                    .map_err(|error| backend("open buckets", error))?;
                if table
                    .get(key.as_slice())
                    .map_err(|error| backend("read bucket", error))?
                    .is_some()
                {
                    return Err(MetadataError::BucketAlreadyExists);
                }
                table
                    .insert(key.as_slice(), encoded.as_slice())
                    .map_err(|error| backend("insert bucket", error))?;
            }
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
            let read = database
                .begin_read()
                .map_err(|error| backend("begin read bucket", error))?;
            let table = read
                .open_table(BUCKETS)
                .map_err(|error| backend("open buckets", error))?;
            let encoded = table
                .get(key.as_slice())
                .map_err(|error| backend("read bucket", error))?
                .map(|value| value.value().to_vec());
            encoded
                .map(|value| serde_json::from_slice(&value))
                .transpose()
                .map_err(MetadataError::from)
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
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin put object", error))?;
            let previous = {
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
                previous
            };
            write
                .commit()
                .map_err(|error| backend("commit object", error))?;
            previous
                .map(|value| serde_json::from_slice(&value))
                .transpose()
                .map_err(MetadataError::from)
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
            let read = database
                .begin_read()
                .map_err(|error| backend("begin read object", error))?;
            let table = read
                .open_table(OBJECTS)
                .map_err(|error| backend("open objects", error))?;
            let encoded = table
                .get(key.as_slice())
                .map_err(|error| backend("read object", error))?
                .map(|value| value.value().to_vec());
            encoded
                .map(|value| serde_json::from_slice(&value))
                .transpose()
                .map_err(MetadataError::from)
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
            let removed = {
                let mut table = write
                    .open_table(OBJECTS)
                    .map_err(|error| backend("open objects", error))?;
                table
                    .remove(key.as_slice())
                    .map_err(|error| backend("delete object", error))?
                    .map(|value| value.value().to_vec())
            };
            write
                .commit()
                .map_err(|error| backend("commit object deletion", error))?;
            removed
                .map(|value| serde_json::from_slice(&value))
                .transpose()
                .map_err(MetadataError::from)
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

fn bucket_key(bucket_id: BucketId) -> Vec<u8> {
    bucket_id.as_uuid().as_bytes().to_vec()
}

fn object_key(bucket_id: BucketId, key: &ObjectKey) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(16 + key.as_str().len());
    encoded.extend_from_slice(bucket_id.as_uuid().as_bytes());
    encoded.extend_from_slice(key.as_str().as_bytes());
    encoded
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
    use oes_core::{Checksum, ObjectId, OrganizationId, VersionId};
    use tempfile::tempdir;

    use super::*;

    fn bucket() -> Bucket {
        Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: "test-bucket".into(),
            created_at: Utc::now(),
        }
    }

    fn object(bucket_id: BucketId) -> ObjectMetadata {
        let now = Utc::now();
        ObjectMetadata {
            id: ObjectId::new(),
            bucket_id,
            key: ObjectKey::new("documents/report.pdf").expect("valid key"),
            version_id: VersionId::new(),
            size: 123,
            checksum: Checksum::sha256([7; 32]),
            content_type: Some("application/pdf".into()),
            custom_metadata: BTreeMap::new(),
            created_at: now,
            modified_at: now,
        }
    }

    #[tokio::test]
    async fn catalog_is_durable_and_replaces_metadata_atomically() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("catalog.redb");
        let bucket = bucket();
        let first = object(bucket.id);

        {
            let repository = RedbMetadataRepository::open(&path)
                .await
                .expect("open catalog");
            repository
                .create_bucket(&bucket)
                .await
                .expect("create bucket");
            assert!(
                repository
                    .put_object(&first)
                    .await
                    .expect("put object")
                    .is_none()
            );
        }

        let repository = RedbMetadataRepository::open(&path)
            .await
            .expect("reopen catalog");
        assert_eq!(
            repository.get_bucket(bucket.id).await.expect("get bucket"),
            Some(bucket)
        );
        assert_eq!(
            repository
                .get_object(first.bucket_id, &first.key)
                .await
                .expect("get object"),
            Some(first.clone())
        );

        let mut replacement = first.clone();
        replacement.id = ObjectId::new();
        assert_eq!(
            repository
                .put_object(&replacement)
                .await
                .expect("replace object"),
            Some(first)
        );
        assert_eq!(
            repository
                .delete_object(replacement.bucket_id, &replacement.key)
                .await
                .expect("delete object"),
            Some(replacement)
        );
    }
}
