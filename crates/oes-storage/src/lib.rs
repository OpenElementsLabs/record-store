//! Streaming object storage boundary and local filesystem implementation.

use std::{
    collections::BTreeMap,
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, Weak},
};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures_core::Stream;
use futures_util::{StreamExt, TryStreamExt};
use md5::Md5;
use oes_core::{
    BucketId, ByteRange, Checksum, CoreError, ETag, ObjectId, ObjectKey, ObjectMetadata,
    ResolvedByteRange, VersionId,
};
use oes_metadata::{MetadataError, MetadataRepository};
use serde::{Deserialize, Serialize};
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

type KeyLockRegistry = Arc<Mutex<HashMap<Vec<u8>, Weak<RwLock<()>>>>>;

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

/// Parameters for an explicit on-demand integrity verification.
#[derive(Debug, Clone)]
pub struct VerifyObjectRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical object key.
    pub key: ObjectKey,
}

/// Cheap local filesystem capacity and temporary-state measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageStatus {
    /// Total filesystem capacity containing the data directory.
    pub capacity_bytes: u64,
    /// Currently available bytes on that filesystem.
    pub available_bytes: u64,
    /// Bytes held by recognized incomplete upload files.
    pub temporary_upload_bytes: u64,
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
    /// A crash-recovery publication record could not be encoded.
    #[error("storage publication record encoding failed: {0}")]
    PublicationEncoding(#[from] serde_json::Error),
    /// Metadata refers to a payload that is absent or unreadable.
    #[error("stored object state is inconsistent")]
    InconsistentState,
    /// Stored bytes no longer match their committed checksum.
    #[error("stored object failed integrity verification")]
    IntegrityMismatch,
    /// Fine-grained operation coordination was poisoned by a panic.
    #[error("storage operation coordination failed")]
    Coordination,
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

    /// Recomputes and verifies the persisted integrity checksum on demand.
    async fn verify(&self, request: VerifyObjectRequest) -> Result<ObjectMetadata, StorageError>;

    /// Returns filesystem capacity and controlled temporary-upload usage.
    async fn status(&self) -> Result<StorageStatus, StorageError>;

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

    fn publication_path(&self, object_id: ObjectId) -> PathBuf {
        self.temporary
            .join(format!("{}.publish", object_id.as_uuid().simple()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PublicationRecord {
    object_id: ObjectId,
    bucket_id: BucketId,
    key: ObjectKey,
}

/// A real single-node object store backed by immutable filesystem payloads.
#[derive(Clone)]
pub struct LocalFilesystemStore {
    layout: StorageLayout,
    metadata: Arc<dyn MetadataRepository>,
    key_locks: KeyLockRegistry,
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
        let store = Self {
            layout,
            metadata,
            key_locks: Arc::new(Mutex::new(HashMap::new())),
        };
        store.recover_publications().await?;
        store.recover_incomplete_uploads().await?;
        store.recover_pending_cleanup().await?;
        Ok(store)
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

    fn key_lock(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Arc<RwLock<()>>, StorageError> {
        let mut encoded = bucket_id.as_uuid().as_bytes().to_vec();
        encoded.extend_from_slice(key.as_str().as_bytes());
        let mut locks = self
            .key_locks
            .lock()
            .map_err(|_| StorageError::Coordination)?;
        if let Some(lock) = locks.get(&encoded).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(RwLock::new(()));
        locks.insert(encoded, Arc::downgrade(&lock));
        Ok(lock)
    }

    async fn remove_queued_payload(&self, metadata: &ObjectMetadata) {
        let path = self.layout.payload_path(metadata.id);
        match fs::remove_file(path).await {
            Ok(()) => {
                if let Err(error) = self.metadata.complete_cleanup(metadata.id).await {
                    warn!(object_id = %metadata.id, error = %error, "payload removed but cleanup record remains");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Err(error) = self.metadata.complete_cleanup(metadata.id).await {
                    warn!(object_id = %metadata.id, error = %error, "failed to complete absent-payload cleanup");
                }
            }
            Err(error) => {
                warn!(
                    object_id = %metadata.id,
                    bucket_id = %metadata.bucket_id,
                    error = %error,
                    "payload cleanup deferred for startup retry"
                );
            }
        }
    }

    async fn recover_incomplete_uploads(&self) -> Result<(), StorageError> {
        let mut entries = fs::read_dir(&self.layout.temporary)
            .await
            .map_err(|source| filesystem("scan temporary uploads", source))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| filesystem("read temporary upload entry", source))?
        {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_recognized_upload_name(name) {
                continue;
            }
            let file_type = entry
                .file_type()
                .await
                .map_err(|source| filesystem("inspect temporary upload", source))?;
            if file_type.is_file() {
                fs::remove_file(entry.path())
                    .await
                    .map_err(|source| filesystem("remove abandoned upload", source))?;
            }
        }
        Ok(())
    }

    async fn recover_publications(&self) -> Result<(), StorageError> {
        let mut entries = fs::read_dir(&self.layout.temporary)
            .await
            .map_err(|source| filesystem("scan publication records", source))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| filesystem("read publication record entry", source))?
        {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(encoded_id) = name.strip_suffix(".publish") else {
                continue;
            };
            let Ok(filename_id) = Uuid::parse_str(encoded_id) else {
                continue;
            };
            let file_type = entry
                .file_type()
                .await
                .map_err(|source| filesystem("inspect publication record", source))?;
            if !file_type.is_file() {
                continue;
            }
            let file_metadata = entry
                .metadata()
                .await
                .map_err(|source| filesystem("inspect publication record size", source))?;
            if file_metadata.len() > 4_096 {
                return Err(filesystem(
                    "decode publication record",
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "publication record is oversized",
                    ),
                ));
            }
            let encoded = fs::read(entry.path())
                .await
                .map_err(|source| filesystem("read publication record", source))?;
            let record: PublicationRecord = serde_json::from_slice(&encoded)?;
            if record.object_id.as_uuid() != filename_id {
                return Err(filesystem(
                    "decode publication record",
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "publication filename and record do not match",
                    ),
                ));
            }
            let committed = self
                .metadata
                .get_object(record.bucket_id, &record.key)
                .await?
                .is_some_and(|metadata| metadata.id == record.object_id);
            if !committed {
                let path = self.layout.payload_path(record.object_id);
                match fs::remove_file(path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => return Err(filesystem("recover unpublished payload", source)),
                }
            }
            fs::remove_file(entry.path())
                .await
                .map_err(|source| filesystem("complete publication recovery", source))?;
        }
        Ok(())
    }

    async fn recover_pending_cleanup(&self) -> Result<(), StorageError> {
        for object_id in self.metadata.pending_cleanup(10_000).await? {
            let path = self.layout.payload_path(object_id);
            match fs::remove_file(path).await {
                Ok(()) => self.metadata.complete_cleanup(object_id).await?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.metadata.complete_cleanup(object_id).await?;
                }
                Err(error) => {
                    warn!(object_id = %object_id, error = %error, "startup payload cleanup deferred");
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for LocalFilesystemStore {
    async fn put(&self, mut request: PutObjectRequest) -> Result<PutObjectResult, StorageError> {
        if self.metadata.get_bucket(request.bucket_id).await?.is_none() {
            return Err(StorageError::BucketNotFound);
        }

        let object_id = ObjectId::new();
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let temporary_path = self.layout.temporary_path(object_id);
        let mut temporary_cleanup = TemporaryFileGuard::new(temporary_path.clone());
        let mut temporary_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .await
            .map_err(|source| filesystem("create upload temporary file", source))?;

        let mut hasher = Sha256::new();
        let mut etag_hasher = Md5::new();
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
            etag_hasher.update(&chunk);
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
        let etag = ETag::from_md5(etag_hasher.finalize().into());
        if let Some(expected) = request.expected_checksum
            && expected != actual_checksum
        {
            return Err(StorageError::ChecksumMismatch {
                expected,
                actual: actual_checksum,
            });
        }

        let publication_path = self.layout.publication_path(object_id);
        let publication_record = PublicationRecord {
            object_id,
            bucket_id: request.bucket_id,
            key: request.key.clone(),
        };
        let mut publication_cleanup = TemporaryFileGuard::new(publication_path.clone());
        write_publication_record(&publication_path, &publication_record).await?;

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
        publication_cleanup.disarm();
        if let Err(error) = sync_directory(payload_parent.to_path_buf()).await {
            if cleanup_file(&payload_path).await {
                cleanup_file(&publication_path).await;
            }
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
            etag,
            content_type: request.content_type,
            custom_metadata: request.custom_metadata,
            created_at: now,
            modified_at: now,
        };

        let _publication_guard = key_lock.write().await;
        let previous = match self.metadata.put_object(&metadata).await {
            Ok(previous) => previous,
            Err(error) => {
                if cleanup_file(&payload_path).await {
                    cleanup_file(&publication_path).await;
                }
                return Err(StorageError::Metadata(error));
            }
        };
        if !cleanup_file(&publication_path).await {
            warn!(object_id = %object_id, "committed object publication record remains for startup recovery");
        }
        if let Some(previous) = previous {
            self.remove_queued_payload(&previous).await;
        }

        Ok(PutObjectResult { metadata })
    }

    async fn get(&self, request: GetObjectRequest) -> Result<GetObjectResult, StorageError> {
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let publication_guard = key_lock.read().await;
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
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let _publication_guard = key_lock.write().await;
        let metadata = self
            .metadata
            .delete_object(request.bucket_id, &request.key)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        self.remove_queued_payload(&metadata).await;
        Ok(())
    }

    async fn verify(&self, request: VerifyObjectRequest) -> Result<ObjectMetadata, StorageError> {
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let _guard = key_lock.read().await;
        let metadata = self.metadata_for(request.bucket_id, &request.key).await?;
        let mut file = File::open(self.layout.payload_path(metadata.id))
            .await
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => StorageError::InconsistentState,
                _ => filesystem("open payload for verification", error),
            })?;
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut hasher = Sha256::new();
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|source| filesystem("verify payload", source))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = Checksum::sha256(hasher.finalize().into());
        if actual != metadata.checksum {
            return Err(StorageError::IntegrityMismatch);
        }
        Ok(metadata)
    }

    async fn status(&self) -> Result<StorageStatus, StorageError> {
        let objects = self.layout.objects.clone();
        let temporary = self.layout.temporary.clone();
        tokio::task::spawn_blocking(move || {
            let capacity_bytes = fs2::total_space(&objects)
                .map_err(|source| filesystem("read filesystem capacity", source))?;
            let available_bytes = fs2::available_space(&objects)
                .map_err(|source| filesystem("read available filesystem space", source))?;
            let mut temporary_upload_bytes = 0_u64;
            for entry in std::fs::read_dir(temporary)
                .map_err(|source| filesystem("scan temporary upload usage", source))?
            {
                let entry = entry.map_err(|source| filesystem("read temporary upload", source))?;
                let name = entry.file_name();
                if name.to_str().is_some_and(is_recognized_upload_name)
                    && entry
                        .file_type()
                        .map_err(|source| filesystem("inspect temporary upload", source))?
                        .is_file()
                {
                    temporary_upload_bytes = temporary_upload_bytes.saturating_add(
                        entry
                            .metadata()
                            .map_err(|source| filesystem("read temporary upload metadata", source))?
                            .len(),
                    );
                }
            }
            Ok(StorageStatus {
                capacity_bytes,
                available_bytes,
                temporary_upload_bytes,
            })
        })
        .await?
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

async fn cleanup_file(path: &Path) -> bool {
    match fs::remove_file(path).await {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            warn!(error = %error, "failed to clean up storage file");
            false
        }
    }
}

async fn write_publication_record(
    path: &Path,
    record: &PublicationRecord,
) -> Result<(), StorageError> {
    let encoded = serde_json::to_vec(record)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|source| filesystem("create publication record", source))?;
    file.write_all(&encoded)
        .await
        .map_err(|source| filesystem("write publication record", source))?;
    file.sync_all()
        .await
        .map_err(|source| filesystem("synchronize publication record", source))?;
    drop(file);
    let parent = path.parent().ok_or_else(|| {
        filesystem(
            "resolve publication directory",
            io::Error::other("publication path has no parent"),
        )
    })?;
    sync_directory(parent.to_path_buf()).await
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

fn is_recognized_upload_name(name: &str) -> bool {
    name.strip_suffix(".upload")
        .is_some_and(|id| Uuid::parse_str(id).is_ok())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use oes_core::{Bucket, BucketName, OrganizationId};
    use oes_metadata::RedbMetadataRepository;
    use tempfile::tempdir;

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

    #[tokio::test]
    async fn startup_publication_journal_removes_an_uncommitted_payload() {
        let directory = tempdir().expect("temporary directory");
        let metadata = Arc::new(
            RedbMetadataRepository::open(directory.path().join("metadata.redb"))
                .await
                .expect("metadata repository"),
        );
        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new("journal-bucket").expect("bucket"),
            created_at: Utc::now(),
        };
        metadata
            .create_bucket(&bucket)
            .await
            .expect("create bucket");
        let repository: Arc<dyn MetadataRepository> = metadata;
        let store = LocalFilesystemStore::open(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&repository),
        )
        .await
        .expect("store");
        let object_id = ObjectId::new();
        let payload = store.layout.payload_path(object_id);
        fs::create_dir_all(payload.parent().expect("payload parent"))
            .await
            .expect("payload parent directory");
        fs::write(&payload, b"unpublished")
            .await
            .expect("unpublished payload");
        let publication = store.layout.publication_path(object_id);
        write_publication_record(
            &publication,
            &PublicationRecord {
                object_id,
                bucket_id: bucket.id,
                key: ObjectKey::new("never-visible").expect("key"),
            },
        )
        .await
        .expect("publication record");
        drop(store);

        LocalFilesystemStore::open(directory.path(), directory.path().join("tmp"), repository)
            .await
            .expect("recover store");
        assert!(!payload.exists());
        assert!(!publication.exists());
    }
}
