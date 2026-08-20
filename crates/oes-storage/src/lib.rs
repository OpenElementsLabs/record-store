//! Streaming object storage boundary and local filesystem implementation.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures_core::Stream;
use futures_util::{StreamExt, TryStreamExt};
use oes_core::{
    BucketId, ByteRange, Checksum, CoreError, ObjectId, ObjectKey, ObjectMetadata,
    ResolvedByteRange, VersionId,
};
use oes_metadata::{MetadataError, MetadataRepository};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::RwLock,
};
use tokio_util::io::ReaderStream;
use tracing::warn;
use uuid::Uuid;

/// A fallible, backpressure-aware incoming payload stream.
pub type UploadStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + 'static>>;

/// A fallible, backpressure-aware outgoing payload stream.
pub type DownloadStream = Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send + 'static>>;

/// Creates a boxed upload stream from a compatible stream implementation.
pub fn upload_stream<S>(stream: S) -> UploadStream
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    Box::pin(stream)
}

/// Parameters for committing a streamed object.
pub struct PutObjectRequest {
    /// Destination bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
    /// Optional media type.
    pub content_type: Option<String>,
    /// Caller-supplied metadata.
    pub custom_metadata: BTreeMap<String, String>,
    /// Optional checksum supplied by the caller for end-to-end validation.
    pub expected_checksum: Option<Checksum>,
    /// Incoming payload chunks.
    pub body: UploadStream,
}

/// Result of a committed object upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutObjectResult {
    /// Metadata made visible by the commit.
    pub metadata: ObjectMetadata,
}

/// Parameters for opening a streamed object read.
#[derive(Debug, Clone)]
pub struct GetObjectRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
    /// Optional byte range. The tail is clamped to the payload length.
    pub range: Option<ByteRange>,
}

/// Result of opening an object read.
pub struct GetObjectResult {
    /// Committed object metadata.
    pub metadata: ObjectMetadata,
    /// Resolved range when a partial read was requested.
    pub range: Option<ResolvedByteRange>,
    /// Payload chunks read lazily with backpressure.
    pub body: DownloadStream,
}

/// Parameters for metadata-only object lookup.
#[derive(Debug, Clone)]
pub struct HeadObjectRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
}

/// Parameters for deleting the visible version of an object.
#[derive(Debug, Clone)]
pub struct DeleteObjectRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
}

/// Storage failures that preserve actionable categories.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The target bucket does not exist.
    #[error("bucket was not found")]
    BucketNotFound,
    /// The target object does not exist.
    #[error("object was not found")]
    ObjectNotFound,
    /// The caller supplied an invalid range or domain value.
    #[error("invalid storage request: {0}")]
    InvalidRequest(#[from] CoreError),
    /// A supplied checksum did not match the streamed payload.
    #[error("checksum mismatch: expected {expected}, calculated {actual}")]
    ChecksumMismatch {
        /// Caller-supplied checksum.
        expected: Checksum,
        /// Checksum calculated while streaming.
        actual: Checksum,
    },
    /// An upload stream failed before it was committed.
    #[error("upload stream failed: {0}")]
    UploadStream(#[source] io::Error),
    /// A named filesystem operation failed.
    #[error("storage filesystem operation '{operation}' failed: {source}")]
    Filesystem {
        /// Stable operation name without internal path details.
        operation: &'static str,
        /// Underlying I/O error for internal logs.
        #[source]
        source: io::Error,
    },
    /// Metadata publication failed.
    #[error("storage metadata operation failed: {0}")]
    Metadata(#[from] MetadataError),
    /// Metadata refers to a payload that is absent or unreadable.
    #[error("stored object state is inconsistent")]
    InconsistentState,
    /// A blocking durability operation could not finish.
    #[error("storage task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Storage operations consumed by API and background components.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Streams and atomically commits an object.
    async fn put(&self, request: PutObjectRequest) -> Result<PutObjectResult, StorageError>;

    /// Opens a lazy stream for an object or object range.
    async fn get(&self, request: GetObjectRequest) -> Result<GetObjectResult, StorageError>;

    /// Returns object metadata without opening its payload.
    async fn head(&self, request: HeadObjectRequest) -> Result<ObjectMetadata, StorageError>;

    /// Removes the visible object version.
    async fn delete(&self, request: DeleteObjectRequest) -> Result<(), StorageError>;

    /// Verifies that required storage paths are writable.
    async fn check_ready(&self) -> Result<(), StorageError>;
}

/// Safe directory layout for the local filesystem backend.
#[derive(Debug, Clone)]
struct StorageLayout {
    objects: PathBuf,
    temporary: PathBuf,
    system: PathBuf,
}

impl StorageLayout {
    fn new(data_directory: &Path, temporary_directory: &Path) -> Self {
        Self {
            objects: data_directory.join("objects"),
            temporary: temporary_directory.to_path_buf(),
            system: data_directory.join("system"),
        }
    }

    fn payload_path(&self, object_id: ObjectId) -> PathBuf {
        let encoded = object_id.as_uuid().simple().to_string();
        self.objects
            .join(&encoded[0..2])
            .join(&encoded[2..4])
            .join(encoded)
    }

    fn temporary_path(&self, object_id: ObjectId) -> PathBuf {
        self.temporary
            .join(format!("{}.upload", object_id.as_uuid().simple()))
    }
}

/// A real single-node object store backed by immutable filesystem payloads.
#[derive(Clone)]
pub struct LocalFilesystemStore {
    layout: StorageLayout,
    metadata: Arc<dyn MetadataRepository>,
    publication_lock: Arc<RwLock<()>>,
}

impl LocalFilesystemStore {
    /// Initializes the storage layout under trusted configuration paths.
    pub async fn open(
        data_directory: impl AsRef<Path>,
        temporary_directory: impl AsRef<Path>,
        metadata: Arc<dyn MetadataRepository>,
    ) -> Result<Self, StorageError> {
        let layout = StorageLayout::new(data_directory.as_ref(), temporary_directory.as_ref());
        for (operation, path) in [
            ("create objects directory", &layout.objects),
            ("create temporary directory", &layout.temporary),
            ("create system directory", &layout.system),
        ] {
            fs::create_dir_all(path)
                .await
                .map_err(|source| filesystem(operation, source))?;
        }
        Ok(Self {
            layout,
            metadata,
            publication_lock: Arc::new(RwLock::new(())),
        })
    }

    async fn metadata_for(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<ObjectMetadata, StorageError> {
        self.metadata
            .get_object(bucket_id, key)
            .await?
            .ok_or(StorageError::ObjectNotFound)
    }

    async fn remove_payload_best_effort(&self, metadata: &ObjectMetadata) {
        let path = self.layout.payload_path(metadata.id);
        if let Err(error) = fs::remove_file(path).await
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(
                object_id = %metadata.id,
                bucket_id = %metadata.bucket_id,
                error = %error,
                "failed to remove unreferenced object payload"
            );
        }
    }
}

#[async_trait]
impl ObjectStore for LocalFilesystemStore {
    async fn put(&self, mut request: PutObjectRequest) -> Result<PutObjectResult, StorageError> {
        if self.metadata.get_bucket(request.bucket_id).await?.is_none() {
            return Err(StorageError::BucketNotFound);
        }

        let object_id = ObjectId::new();
        let temporary_path = self.layout.temporary_path(object_id);
        let mut temporary_cleanup = TemporaryFileGuard::new(temporary_path.clone());
        let mut temporary_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .await
            .map_err(|source| filesystem("create upload temporary file", source))?;

        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        while let Some(chunk) = request.body.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    return Err(StorageError::UploadStream(error));
                }
            };
            size = match size.checked_add(chunk.len() as u64) {
                Some(size) => size,
                None => {
                    return Err(StorageError::Filesystem {
                        operation: "count upload bytes",
                        source: io::Error::other("object size exceeds u64"),
                    });
                }
            };
            hasher.update(&chunk);
            if let Err(source) = temporary_file.write_all(&chunk).await {
                return Err(filesystem("write upload", source));
            }
        }

        if let Err(source) = temporary_file.flush().await {
            return Err(filesystem("flush upload", source));
        }
        if let Err(source) = temporary_file.sync_all().await {
            return Err(filesystem("synchronize upload", source));
        }
        drop(temporary_file);

        let actual_checksum = Checksum::sha256(hasher.finalize().into());
        if let Some(expected) = request.expected_checksum
            && expected != actual_checksum
        {
            return Err(StorageError::ChecksumMismatch {
                expected,
                actual: actual_checksum,
            });
        }

        let payload_path = self.layout.payload_path(object_id);
        let payload_parent = payload_path
            .parent()
            .ok_or_else(|| StorageError::Filesystem {
                operation: "resolve payload parent",
                source: io::Error::other("payload path has no parent"),
            })?;
        if let Err(source) = fs::create_dir_all(payload_parent).await {
            return Err(filesystem("create payload shard", source));
        }
        if let Err(source) = fs::rename(&temporary_path, &payload_path).await {
            return Err(filesystem("publish payload", source));
        }
        temporary_cleanup.disarm();
        if let Err(error) = sync_directory(payload_parent.to_path_buf()).await {
            cleanup_file(&payload_path).await;
            return Err(error);
        }

        let now = Utc::now();
        let metadata = ObjectMetadata {
            id: object_id,
            bucket_id: request.bucket_id,
            key: request.key,
            version_id: VersionId::new(),
            size,
            checksum: actual_checksum,
            content_type: request.content_type,
            custom_metadata: request.custom_metadata,
            created_at: now,
            modified_at: now,
        };

        let _publication_guard = self.publication_lock.write().await;
        let previous = match self.metadata.put_object(&metadata).await {
            Ok(previous) => previous,
            Err(error) => {
                cleanup_file(&payload_path).await;
                return Err(StorageError::Metadata(error));
            }
        };
        if let Some(previous) = previous {
            self.remove_payload_best_effort(&previous).await;
        }

        Ok(PutObjectResult { metadata })
    }

    async fn get(&self, request: GetObjectRequest) -> Result<GetObjectResult, StorageError> {
        let publication_guard = self.publication_lock.read().await;
        let metadata = self.metadata_for(request.bucket_id, &request.key).await?;
        let path = self.layout.payload_path(metadata.id);
        let mut file = File::open(path).await.map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => StorageError::InconsistentState,
            _ => filesystem("open payload", error),
        })?;

        let resolved_range = request
            .range
            .map(|range| range.resolve(metadata.size))
            .transpose()?;
        let body: DownloadStream = if let Some(range) = resolved_range {
            file.seek(SeekFrom::Start(range.offset))
                .await
                .map_err(|source| filesystem("seek payload", source))?;
            let stream = ReaderStream::new(file.take(range.length))
                .map_err(|source| filesystem("read payload", source));
            Box::pin(stream)
        } else {
            let stream =
                ReaderStream::new(file).map_err(|source| filesystem("read payload", source));
            Box::pin(stream)
        };
        drop(publication_guard);

        Ok(GetObjectResult {
            metadata,
            range: resolved_range,
            body,
        })
    }

    async fn head(&self, request: HeadObjectRequest) -> Result<ObjectMetadata, StorageError> {
        self.metadata_for(request.bucket_id, &request.key).await
    }

    async fn delete(&self, request: DeleteObjectRequest) -> Result<(), StorageError> {
        let _publication_guard = self.publication_lock.write().await;
        let metadata = self
            .metadata
            .delete_object(request.bucket_id, &request.key)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        self.remove_payload_best_effort(&metadata).await;
        Ok(())
    }

    async fn check_ready(&self) -> Result<(), StorageError> {
        let probe_path = self
            .layout
            .temporary
            .join(format!(".readiness-{}", Uuid::new_v4().simple()));
        let result = async {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe_path)
                .await
                .map_err(|source| filesystem("create readiness probe", source))?;
            file.write_all(b"ready")
                .await
                .map_err(|source| filesystem("write readiness probe", source))?;
            file.sync_all()
                .await
                .map_err(|source| filesystem("synchronize readiness probe", source))?;
            drop(file);
            fs::remove_file(&probe_path)
                .await
                .map_err(|source| filesystem("remove readiness probe", source))?;
            Ok(())
        }
        .await;
        if result.is_err() {
            cleanup_file(&probe_path).await;
        }
        result
    }
}

struct TemporaryFileGuard {
    path: PathBuf,
    active: bool,
}

impl TemporaryFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TemporaryFileGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn cleanup_file(path: &Path) {
    if let Err(error) = fs::remove_file(path).await
        && error.kind() != io::ErrorKind::NotFound
    {
        warn!(error = %error, "failed to clean up storage file");
    }
}

async fn sync_directory(path: PathBuf) -> Result<(), StorageError> {
    tokio::task::spawn_blocking(move || {
        let directory = std::fs::File::open(path)
            .map_err(|source| filesystem("open payload directory", source))?;
        directory
            .sync_all()
            .map_err(|source| filesystem("synchronize payload directory", source))
    })
    .await?
}

fn filesystem(operation: &'static str, source: io::Error) -> StorageError {
    StorageError::Filesystem { operation, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_paths_only_contain_generated_identifier_components() {
        let root = PathBuf::from("/trusted/data");
        let layout = StorageLayout::new(&root, &root.join("tmp"));
        let id = ObjectId::from_uuid(
            Uuid::parse_str("12345678-1234-4234-8234-123456789abc").expect("UUID"),
        );
        assert_eq!(
            layout.payload_path(id),
            root.join("objects/12/34/12345678123442348234123456789abc")
        );
    }
}
