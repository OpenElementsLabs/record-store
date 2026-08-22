//! Streaming object storage boundary and local filesystem implementation.

use std::{
    collections::BTreeMap,
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, Weak},
};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, OsRng, Payload, rand_core::RngCore},
};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures_core::Stream;
use futures_util::{StreamExt, TryStreamExt, stream};
use md5::Md5;
use oes_core::{
    BucketId, ByteRange, Checksum, CoreError, ETag, MultipartUpload, MultipartUploadState,
    ObjectId, ObjectKey, ObjectMetadata, ObjectVersionRecord, PartNumber, PayloadFormat,
    ResolvedByteRange, UploadId, UploadedPart, VersionId,
};
use oes_metadata::{DeleteObjectResult, MetadataError, MetadataRepository, NewDeleteMarker};
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
use zeroize::Zeroizing;

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
    /// Preallocated payload identifier used by crash-recoverable multipart completion.
    pub object_id: Option<ObjectId>,
    /// Protocol ETag override kept independent from the strong checksum.
    pub protocol_etag: Option<ETag>,
    /// Incoming payload chunks.
    pub body: UploadStream,
}

/// Parameters for durably streaming one multipart part.
pub struct PutMultipartPartRequest {
    /// Opaque upload identifier.
    pub upload_id: UploadId,
    /// Validated one-based part number.
    pub number: PartNumber,
    /// Optional strong checksum supplied by the client.
    pub expected_checksum: Option<Checksum>,
    /// Incoming payload chunks.
    pub body: UploadStream,
}

/// Parameters for combining an already validated multipart manifest.
#[derive(Debug, Clone)]
pub struct CompleteMultipartRequest {
    /// Durable upload descriptor.
    pub upload: MultipartUpload,
    /// Ordered durable parts matching the client manifest.
    pub parts: Vec<UploadedPart>,
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

/// Parameters for opening one immutable historical version.
#[derive(Debug, Clone)]
pub struct GetObjectVersionRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical key used to prevent cross-key identifier confusion.
    pub key: ObjectKey,
    /// Stable requested version.
    pub version_id: VersionId,
    /// Optional byte range.
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

/// Parameters for permanently deleting one explicit version.
#[derive(Debug, Clone)]
pub struct DeleteObjectVersionRequest {
    /// Source bucket.
    pub bucket_id: BucketId,
    /// Logical key.
    pub key: ObjectKey,
    /// Stable version identifier.
    pub version_id: VersionId,
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

/// Bounded consistency inspection without exposing filesystem paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StorageInspection {
    pub metadata_payloads_scanned: u64,
    pub data_payloads_scanned: u64,
    pub metadata_without_data: u64,
    pub data_without_metadata: u64,
    pub unknown_data_entries: u64,
    pub recognized_temporary_entries: u64,
    pub unknown_temporary_entries: u64,
    pub truncated: bool,
    pub missing_payload_samples: Vec<ObjectId>,
    pub orphan_payload_samples: Vec<ObjectId>,
}

/// Explicit dry-run-by-default repair input.
#[derive(Debug, Clone, Copy)]
pub struct StorageRepairRequest {
    pub maximum_entries: usize,
    pub dry_run: bool,
}

/// Repair outcome. Suspicious unknown files are never removed automatically.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StorageRepairResult {
    pub inspection: StorageInspection,
    pub removed_orphan_payloads: u64,
    pub dry_run: bool,
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
    /// A requested immutable version is a logical delete marker.
    #[error("requested object version is a delete marker")]
    DeleteMarker {
        /// Stable marker version identifier.
        version_id: VersionId,
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
    /// Encrypted payload state exists but no configured master key can open it.
    #[error("object encryption master key is required")]
    EncryptionKeyRequired,
    /// The configured master key does not match the durable storage key reference.
    #[error("object encryption master key does not match durable storage state")]
    EncryptionKeyMismatch,
    /// Authenticated payload encryption or decryption failed.
    #[error("object payload cryptography failed")]
    Cryptography,
    /// Fine-grained operation coordination was poisoned by a panic.
    #[error("storage operation coordination failed")]
    Coordination,
    /// A blocking durability operation could not finish.
    #[error("storage task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// Cluster state or placement could not be consulted.
    #[error("cluster storage is currently unavailable: {0}")]
    ClusterUnavailable(String),
    /// Fewer replicas became durable than the write policy requires.
    ///
    /// The write is refused rather than acknowledged: an object must never be
    /// reported as stored before its durability requirement is satisfied.
    #[error(
        "write durability was not met: {achieved} of {required} required replica          acknowledgement(s) succeeded ({detail})"
    )]
    DurabilityNotMet {
        /// Acknowledgements the policy required.
        required: u8,
        /// Acknowledgements that succeeded.
        achieved: u8,
        /// Per-target detail for operators.
        detail: String,
    },
    /// No healthy replica of the payload could be read.
    #[error("no healthy replica of the requested object is currently available")]
    NoHealthyReplica,
}

/// Storage operations consumed by API and background components.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Streams and atomically commits an object.
    async fn put(&self, request: PutObjectRequest) -> Result<PutObjectResult, StorageError>;

    /// Streams and durably publishes one resumable multipart part.
    async fn put_multipart_part(
        &self,
        request: PutMultipartPartRequest,
    ) -> Result<UploadedPart, StorageError>;

    /// Streams durable parts into one atomically published logical object.
    async fn complete_multipart(
        &self,
        request: CompleteMultipartRequest,
    ) -> Result<PutObjectResult, StorageError>;

    /// Opens a lazy stream for an object or object range.
    async fn get(&self, request: GetObjectRequest) -> Result<GetObjectResult, StorageError>;

    /// Opens a lazy stream for one immutable object version.
    async fn get_version(
        &self,
        request: GetObjectVersionRequest,
    ) -> Result<GetObjectResult, StorageError>;

    /// Returns object metadata without opening its payload.
    async fn head(&self, request: HeadObjectRequest) -> Result<ObjectMetadata, StorageError>;

    /// Removes the visible object version.
    async fn delete(
        &self,
        request: DeleteObjectRequest,
    ) -> Result<DeleteObjectResult, StorageError>;

    /// Permanently removes one explicitly selected version.
    async fn delete_version(&self, request: DeleteObjectVersionRequest)
    -> Result<(), StorageError>;

    /// Recomputes and verifies the persisted integrity checksum on demand.
    async fn verify(&self, request: VerifyObjectRequest) -> Result<ObjectMetadata, StorageError>;

    /// Returns filesystem capacity and controlled temporary-upload usage.
    async fn status(&self) -> Result<StorageStatus, StorageError>;

    /// Verifies that required storage paths are writable.
    async fn check_ready(&self) -> Result<(), StorageError>;

    /// Inspects bounded metadata/data consistency without mutating state.
    async fn inspect(&self, maximum_entries: usize) -> Result<StorageInspection, StorageError>;

    /// Removes only positively identified unreferenced OES payloads when not a dry run.
    async fn repair(
        &self,
        request: StorageRepairRequest,
    ) -> Result<StorageRepairResult, StorageError>;

    /// Processes a bounded page of durable payload garbage.
    async fn cleanup_pending(&self, limit: usize) -> Result<usize, StorageError>;
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

    fn replica_temporary_path(&self, object_id: ObjectId, operation_id: &str) -> PathBuf {
        // The operation identity is hashed so a peer-supplied value can never
        // influence the filesystem path.
        let scope = hex::encode(&Sha256::digest(operation_id.as_bytes())[..8]);
        self.temporary
            .join(format!("{}-{scope}.replica", object_id.as_uuid().simple()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PublicationRecord {
    object_id: ObjectId,
    /// Present for object commits, absent for replica transfers.
    #[serde(default)]
    bucket_id: Option<BucketId>,
    /// Present for object commits, absent for replica transfers.
    #[serde(default)]
    key: Option<ObjectKey>,
}

const STORAGE_FORMAT_VERSION: u32 = 1;
const OBJECT_ENCRYPTION_FORMAT_VERSION: u32 = 1;
const OBJECT_ENCRYPTION_ALGORITHM: &str = "AES-256-GCM-ENVELOPE-CHUNKED";
const ENCRYPTED_PAYLOAD_MAGIC: &[u8; 8] = b"OESOBJ01";
const ENCRYPTED_PAYLOAD_HEADER_LEN: usize = 124;
const ENCRYPTED_PAYLOAD_CHUNK_SIZE: usize = 64 * 1024;
const AES_GCM_TAG_LEN: usize = 16;

#[derive(Debug, Serialize, Deserialize)]
struct StorageFormatRecord {
    storage_format_version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct ObjectEncryptionRecord {
    encryption_format_version: u32,
    algorithm: String,
    key_reference: String,
}

#[derive(Clone)]
struct ObjectEncryption {
    key_encryption_key: Arc<Zeroizing<[u8; 32]>>,
    key_reference: [u8; 16],
}

/// A real single-node object store backed by immutable filesystem payloads.
#[derive(Clone)]
pub struct LocalFilesystemStore {
    layout: StorageLayout,
    metadata: Arc<dyn MetadataRepository>,
    key_locks: KeyLockRegistry,
    encryption: Option<ObjectEncryption>,
}

impl LocalFilesystemStore {
    /// Initializes the storage layout under trusted configuration paths.
    pub async fn open(
        data_directory: impl AsRef<Path>,
        temporary_directory: impl AsRef<Path>,
        metadata: Arc<dyn MetadataRepository>,
    ) -> Result<Self, StorageError> {
        Self::open_with_master_key(data_directory, temporary_directory, metadata, None).await
    }

    /// Initializes a store for a cluster node.
    ///
    /// Recovery here is deliberately narrower than in a standalone deployment:
    /// only this node's own abandoned staging files are removed. Deciding whether
    /// a published payload is still referenced needs the cluster's committed
    /// metadata, so that decision belongs to reconciliation, not to start-up.
    pub async fn open_for_cluster(
        data_directory: impl AsRef<Path>,
        temporary_directory: impl AsRef<Path>,
        metadata: Arc<dyn MetadataRepository>,
        master_key: Option<&[u8]>,
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
        initialize_storage_format(&layout).await?;
        let encryption = initialize_object_encryption(&layout, master_key).await?;
        let store = Self {
            layout,
            metadata,
            key_locks: Arc::new(Mutex::new(HashMap::new())),
            encryption,
        };
        store.discard_staged_transfers().await?;
        Ok(store)
    }

    /// Initializes a store that encrypts new payloads with independent data
    /// keys wrapped by the supplied master key.
    pub async fn open_encrypted(
        data_directory: impl AsRef<Path>,
        temporary_directory: impl AsRef<Path>,
        metadata: Arc<dyn MetadataRepository>,
        master_key: &[u8],
    ) -> Result<Self, StorageError> {
        Self::open_with_master_key(
            data_directory,
            temporary_directory,
            metadata,
            Some(master_key),
        )
        .await
    }

    async fn open_with_master_key(
        data_directory: impl AsRef<Path>,
        temporary_directory: impl AsRef<Path>,
        metadata: Arc<dyn MetadataRepository>,
        master_key: Option<&[u8]>,
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
        initialize_storage_format(&layout).await?;
        let encryption = initialize_object_encryption(&layout, master_key).await?;
        let store = Self {
            layout,
            metadata,
            key_locks: Arc::new(Mutex::new(HashMap::new())),
            encryption,
        };
        store.recover_publications().await?;
        store.metadata.recover_multipart_completions().await?;
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
        self.remove_queued_payload_id(metadata.id).await;
    }

    async fn remove_queued_payload_id(&self, object_id: ObjectId) {
        let path = self.layout.payload_path(object_id);
        match fs::remove_file(path).await {
            Ok(()) => {
                if let Err(error) = self.metadata.complete_cleanup(object_id).await {
                    warn!(%object_id, error = %error, "payload removed but cleanup record remains");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Err(error) = self.metadata.complete_cleanup(object_id).await {
                    warn!(%object_id, error = %error, "failed to complete absent-payload cleanup");
                }
            }
            Err(error) => {
                warn!(
                    %object_id,
                    error = %error,
                    "payload cleanup deferred for startup retry"
                );
            }
        }
    }

    /// Removes staging files and publication markers that never completed.
    ///
    /// A staged file was never visible to anything, so discarding it is always
    /// safe, in a cluster as much as on a single node.
    async fn discard_staged_transfers(&self) -> Result<(), StorageError> {
        self.recover_incomplete_uploads().await?;
        let mut entries = fs::read_dir(&self.layout.temporary)
            .await
            .map_err(|source| filesystem("scan publication markers", source))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| filesystem("read publication marker", source))?
        {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.ends_with(".publish") {
                continue;
            }
            fs::remove_file(entry.path())
                .await
                .map_err(|source| filesystem("remove publication marker", source))?;
        }
        Ok(())
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
            let committed = self.metadata.payload_referenced(record.object_id).await?;
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
        loop {
            let completed = self.cleanup_pending(10_000).await?;
            if completed == 0 {
                return Ok(());
            }
        }
    }

    async fn write_payload(
        &self,
        file: &mut File,
        object_id: ObjectId,
        body: &mut UploadStream,
    ) -> Result<WrittenPayload, StorageError> {
        match &self.encryption {
            Some(encryption) => write_encrypted_payload(file, object_id, body, encryption).await,
            None => write_plaintext_payload(file, body).await,
        }
    }

    async fn open_payload(
        &self,
        object_id: ObjectId,
        size: u64,
        payload_format: PayloadFormat,
        range: Option<ByteRange>,
    ) -> Result<(Option<ResolvedByteRange>, DownloadStream), StorageError> {
        let mut file = File::open(self.layout.payload_path(object_id))
            .await
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => StorageError::InconsistentState,
                _ => filesystem("open payload", error),
            })?;
        let resolved_range = range.map(|range| range.resolve(size)).transpose()?;
        let body = match payload_format {
            PayloadFormat::Plaintext => {
                if let Some(range) = resolved_range {
                    file.seek(SeekFrom::Start(range.offset))
                        .await
                        .map_err(|source| filesystem("seek payload", source))?;
                    Box::pin(
                        ReaderStream::new(file.take(range.length))
                            .map_err(|source| filesystem("read payload", source)),
                    ) as DownloadStream
                } else {
                    Box::pin(
                        ReaderStream::new(file)
                            .map_err(|source| filesystem("read payload", source)),
                    ) as DownloadStream
                }
            }
            PayloadFormat::Aes256GcmEnvelopeV1 => {
                let encryption = self
                    .encryption
                    .as_ref()
                    .ok_or(StorageError::EncryptionKeyRequired)?;
                open_encrypted_payload(file, object_id, size, resolved_range, encryption).await?
            }
        };
        Ok((resolved_range, body))
    }

    /// Returns the physical size of a stored payload, if it exists.
    async fn payload_size(&self, object_id: ObjectId) -> Result<Option<u64>, StorageError> {
        match fs::metadata(self.layout.payload_path(object_id)).await {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(filesystem("inspect replica", source)),
        }
    }

    /// Returns the physical representation this node writes for new payloads.
    const fn local_payload_format(&self) -> PayloadFormat {
        if self.encryption.is_some() {
            PayloadFormat::Aes256GcmEnvelopeV1
        } else {
            PayloadFormat::Plaintext
        }
    }

    async fn open_metadata(
        &self,
        metadata: ObjectMetadata,
        range: Option<ByteRange>,
    ) -> Result<GetObjectResult, StorageError> {
        let (resolved_range, body) = self
            .open_payload(metadata.id, metadata.size, metadata.payload_format, range)
            .await?;
        Ok(GetObjectResult {
            metadata,
            range: resolved_range,
            body,
        })
    }
}

#[async_trait]
impl ObjectStore for LocalFilesystemStore {
    async fn put(&self, mut request: PutObjectRequest) -> Result<PutObjectResult, StorageError> {
        if self.metadata.get_bucket(request.bucket_id).await?.is_none() {
            return Err(StorageError::BucketNotFound);
        }

        let object_id = request.object_id.unwrap_or_default();
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let temporary_path = self.layout.temporary_path(object_id);
        let mut temporary_cleanup = TemporaryFileGuard::new(temporary_path.clone());
        let mut temporary_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .await
            .map_err(|source| filesystem("create upload temporary file", source))?;

        let written = self
            .write_payload(&mut temporary_file, object_id, &mut request.body)
            .await?;

        if let Err(source) = temporary_file.flush().await {
            return Err(filesystem("flush upload", source));
        }
        if let Err(source) = temporary_file.sync_all().await {
            return Err(filesystem("synchronize upload", source));
        }
        drop(temporary_file);

        let actual_checksum = written.checksum;
        let calculated_etag = written.etag;
        let etag = request.protocol_etag.unwrap_or(calculated_etag);
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
            bucket_id: Some(request.bucket_id),
            key: Some(request.key.clone()),
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
            size: written.size,
            checksum: actual_checksum,
            payload_format: written.payload_format,
            durability: oes_core::DurabilityProfile::Single,
            etag,
            content_type: request.content_type,
            custom_metadata: request.custom_metadata,
            created_at: now,
            modified_at: now,
        };

        let _publication_guard = key_lock.write().await;
        let commit = match self.metadata.put_object(&metadata).await {
            Ok(commit) => commit,
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
        for previous in commit.cleanup {
            self.remove_queued_payload(&previous).await;
        }

        Ok(PutObjectResult { metadata })
    }

    async fn put_multipart_part(
        &self,
        mut request: PutMultipartPartRequest,
    ) -> Result<UploadedPart, StorageError> {
        let upload = self
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
        let temporary_path = self.layout.temporary_path(object_id);
        let mut cleanup = TemporaryFileGuard::new(temporary_path.clone());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .await
            .map_err(|source| filesystem("create multipart part", source))?;
        let written = self
            .write_payload(&mut file, object_id, &mut request.body)
            .await?;
        file.flush()
            .await
            .map_err(|source| filesystem("flush multipart part", source))?;
        file.sync_all()
            .await
            .map_err(|source| filesystem("synchronize multipart part", source))?;
        drop(file);
        let checksum = written.checksum;
        if let Some(expected) = request.expected_checksum
            && expected != checksum
        {
            return Err(StorageError::ChecksumMismatch {
                expected,
                actual: checksum,
            });
        }
        let etag = written.etag;
        let payload_path = self.layout.payload_path(object_id);
        let parent = payload_path.parent().ok_or_else(|| {
            filesystem(
                "resolve multipart payload parent",
                io::Error::other("missing parent"),
            )
        })?;
        fs::create_dir_all(parent)
            .await
            .map_err(|source| filesystem("create multipart payload shard", source))?;
        fs::rename(&temporary_path, &payload_path)
            .await
            .map_err(|source| filesystem("publish multipart part", source))?;
        cleanup.disarm();
        sync_directory(parent.to_path_buf()).await?;
        let part = UploadedPart {
            upload_id: request.upload_id,
            number: request.number,
            object_id,
            size: written.size,
            checksum,
            payload_format: written.payload_format,
            etag,
            modified_at: Utc::now(),
        };
        match self.metadata.put_multipart_part(&part).await {
            Ok(previous) => {
                if let Some(previous) = previous {
                    self.remove_queued_payload_id(previous.object_id).await;
                }
            }
            Err(error) => {
                cleanup_file(&payload_path).await;
                return Err(StorageError::Metadata(error));
            }
        }
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
            .metadata
            .get_multipart_upload(request.upload.id)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        if persisted.bucket_id != request.upload.bucket_id || persisted.key != request.upload.key {
            return Err(StorageError::ObjectNotFound);
        }
        let object_id = match persisted.state {
            MultipartUploadState::Active => {
                let object_id = ObjectId::new();
                self.metadata
                    .begin_multipart_completion(persisted.id, object_id)
                    .await?;
                object_id
            }
            MultipartUploadState::Completing { object_id } => object_id,
        };
        if let Some(metadata) = self
            .metadata
            .get_object(persisted.bucket_id, &persisted.key)
            .await?
            .filter(|metadata| metadata.id == object_id)
        {
            let cleanup = self.metadata.finish_multipart_upload(persisted.id).await?;
            for part in cleanup.parts {
                self.remove_queued_payload_id(part.object_id).await;
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
        let parts = request.parts.clone();
        let part_store = self.clone();
        let body = stream::iter(parts)
            .then(move |part| {
                let part_store = part_store.clone();
                async move {
                    part_store
                        .open_payload(part.object_id, part.size, part.payload_format, None)
                        .await
                        .map(|(_, body)| body.map_err(io::Error::other))
                        .map_err(io::Error::other)
                }
            })
            .try_flatten();
        let result = self
            .put(PutObjectRequest {
                bucket_id: persisted.bucket_id,
                key: persisted.key,
                content_type: persisted.content_type,
                custom_metadata: persisted.custom_metadata,
                expected_checksum: None,
                object_id: Some(object_id),
                protocol_etag: Some(protocol_etag),
                body: upload_stream(body),
            })
            .await?;
        let cleanup = self.metadata.finish_multipart_upload(persisted.id).await?;
        for part in cleanup.parts {
            self.remove_queued_payload_id(part.object_id).await;
        }
        Ok(result)
    }

    async fn get(&self, request: GetObjectRequest) -> Result<GetObjectResult, StorageError> {
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let publication_guard = key_lock.read().await;
        let metadata = self.metadata_for(request.bucket_id, &request.key).await?;
        let result = self.open_metadata(metadata, request.range).await?;
        drop(publication_guard);
        Ok(result)
    }

    async fn get_version(
        &self,
        request: GetObjectVersionRequest,
    ) -> Result<GetObjectResult, StorageError> {
        let record = self
            .metadata
            .get_object_version(request.bucket_id, &request.key, request.version_id)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        match record {
            ObjectVersionRecord::Object { metadata, .. } => {
                self.open_metadata(metadata, request.range).await
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
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let _publication_guard = key_lock.write().await;
        let result = self
            .metadata
            .delete_object(request.bucket_id, &request.key, NewDeleteMarker::generate())
            .await?;
        if !result.previously_visible && result.delete_marker.is_none() {
            return Err(StorageError::ObjectNotFound);
        }
        for metadata in &result.cleanup {
            self.remove_queued_payload(metadata).await;
        }
        Ok(result)
    }

    async fn delete_version(
        &self,
        request: DeleteObjectVersionRequest,
    ) -> Result<(), StorageError> {
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let _guard = key_lock.write().await;
        let result = self
            .metadata
            .delete_object_version(request.bucket_id, &request.key, request.version_id)
            .await?
            .ok_or(StorageError::ObjectNotFound)?;
        if let Some(metadata) = result.cleanup {
            self.remove_queued_payload(&metadata).await;
        }
        Ok(())
    }

    async fn verify(&self, request: VerifyObjectRequest) -> Result<ObjectMetadata, StorageError> {
        let key_lock = self.key_lock(request.bucket_id, &request.key)?;
        let _guard = key_lock.read().await;
        let metadata = self.metadata_for(request.bucket_id, &request.key).await?;
        let opened = self.open_metadata(metadata.clone(), None).await?;
        let mut body = opened.body;
        let mut hasher = Sha256::new();
        while let Some(chunk) = body.next().await {
            hasher.update(chunk?);
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

    async fn inspect(&self, maximum_entries: usize) -> Result<StorageInspection, StorageError> {
        inspect_consistency(self, maximum_entries)
            .await
            .map(|(inspection, _)| inspection)
    }

    async fn repair(
        &self,
        request: StorageRepairRequest,
    ) -> Result<StorageRepairResult, StorageError> {
        let (inspection, orphan_payloads) =
            inspect_consistency(self, request.maximum_entries).await?;
        let mut removed = 0_u64;
        if !request.dry_run {
            for object_id in orphan_payloads {
                match fs::remove_file(self.layout.payload_path(object_id)).await {
                    Ok(()) => removed = removed.saturating_add(1),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => return Err(filesystem("remove orphan payload", source)),
                }
            }
        }
        Ok(StorageRepairResult {
            inspection,
            removed_orphan_payloads: removed,
            dry_run: request.dry_run,
        })
    }

    async fn cleanup_pending(&self, limit: usize) -> Result<usize, StorageError> {
        let mut completed = 0_usize;
        for object_id in self.metadata.pending_cleanup(limit.min(10_000)).await? {
            let path = self.layout.payload_path(object_id);
            match fs::remove_file(path).await {
                Ok(()) => {
                    self.metadata.complete_cleanup(object_id).await?;
                    completed += 1;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.metadata.complete_cleanup(object_id).await?;
                    completed += 1;
                }
                Err(error) => {
                    warn!(object_id = %object_id, error = %error, "payload cleanup deferred");
                }
            }
        }
        Ok(completed)
    }
}

struct WrittenPayload {
    size: u64,
    checksum: Checksum,
    etag: ETag,
    payload_format: PayloadFormat,
}

async fn write_plaintext_payload(
    file: &mut File,
    body: &mut UploadStream,
) -> Result<WrittenPayload, StorageError> {
    let mut strong = Sha256::new();
    let mut md5 = Md5::new();
    let mut size = 0_u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(StorageError::UploadStream)?;
        size = size.checked_add(chunk.len() as u64).ok_or_else(|| {
            filesystem("count upload bytes", io::Error::other("object exceeds u64"))
        })?;
        strong.update(&chunk);
        md5.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|source| filesystem("write upload", source))?;
    }
    Ok(WrittenPayload {
        size,
        checksum: Checksum::sha256(strong.finalize().into()),
        etag: ETag::from_md5(md5.finalize().into()),
        payload_format: PayloadFormat::Plaintext,
    })
}

async fn write_encrypted_payload(
    file: &mut File,
    object_id: ObjectId,
    body: &mut UploadStream,
    encryption: &ObjectEncryption,
) -> Result<WrittenPayload, StorageError> {
    file.write_all(&[0_u8; ENCRYPTED_PAYLOAD_HEADER_LEN])
        .await
        .map_err(|source| filesystem("write encrypted payload header", source))?;
    let data_key = Zeroizing::new(random_array_32());
    let content_nonce = random_array_8();
    let mut strong = Sha256::new();
    let mut md5 = Md5::new();
    let mut size = 0_u64;
    let mut chunk_index = 0_u32;
    let mut pending = Vec::with_capacity(ENCRYPTED_PAYLOAD_CHUNK_SIZE);

    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(StorageError::UploadStream)?;
        size = size.checked_add(chunk.len() as u64).ok_or_else(|| {
            filesystem("count upload bytes", io::Error::other("object exceeds u64"))
        })?;
        strong.update(&chunk);
        md5.update(&chunk);
        let mut remaining = chunk.as_ref();
        while !remaining.is_empty() {
            let take = (ENCRYPTED_PAYLOAD_CHUNK_SIZE - pending.len()).min(remaining.len());
            pending.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if pending.len() == ENCRYPTED_PAYLOAD_CHUNK_SIZE {
                write_encrypted_chunk(
                    file,
                    &data_key,
                    object_id,
                    &content_nonce,
                    chunk_index,
                    &pending,
                )
                .await?;
                pending.clear();
                chunk_index = chunk_index
                    .checked_add(1)
                    .ok_or(StorageError::Cryptography)?;
            }
        }
    }
    if !pending.is_empty() || size == 0 {
        write_encrypted_chunk(
            file,
            &data_key,
            object_id,
            &content_nonce,
            chunk_index,
            &pending,
        )
        .await?;
    }

    let header = encode_encrypted_header(encryption, object_id, size, &data_key, content_nonce)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|source| filesystem("seek encrypted payload header", source))?;
    file.write_all(&header)
        .await
        .map_err(|source| filesystem("finalize encrypted payload header", source))?;
    file.seek(SeekFrom::End(0))
        .await
        .map_err(|source| filesystem("finalize encrypted payload", source))?;

    Ok(WrittenPayload {
        size,
        checksum: Checksum::sha256(strong.finalize().into()),
        etag: ETag::from_md5(md5.finalize().into()),
        payload_format: PayloadFormat::Aes256GcmEnvelopeV1,
    })
}

async fn write_encrypted_chunk(
    file: &mut File,
    data_key: &[u8; 32],
    object_id: ObjectId,
    content_nonce: &[u8; 8],
    index: u32,
    plaintext: &[u8],
) -> Result<(), StorageError> {
    let cipher = Aes256Gcm::new_from_slice(data_key).map_err(|_| StorageError::Cryptography)?;
    let nonce = content_chunk_nonce(content_nonce, index);
    let aad = content_chunk_aad(object_id, index, plaintext.len());
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| StorageError::Cryptography)?;
    file.write_all(&ciphertext)
        .await
        .map_err(|source| filesystem("write encrypted payload chunk", source))
}

fn encode_encrypted_header(
    encryption: &ObjectEncryption,
    object_id: ObjectId,
    size: u64,
    data_key: &[u8; 32],
    content_nonce: [u8; 8],
) -> Result<[u8; ENCRYPTED_PAYLOAD_HEADER_LEN], StorageError> {
    let mut header = [0_u8; ENCRYPTED_PAYLOAD_HEADER_LEN];
    header[..8].copy_from_slice(ENCRYPTED_PAYLOAD_MAGIC);
    header[8..10].copy_from_slice(&(OBJECT_ENCRYPTION_FORMAT_VERSION as u16).to_be_bytes());
    header[10] = 1;
    header[12..16].copy_from_slice(&(ENCRYPTED_PAYLOAD_CHUNK_SIZE as u32).to_be_bytes());
    header[16..24].copy_from_slice(&size.to_be_bytes());
    header[24..40].copy_from_slice(object_id.as_uuid().as_bytes());
    header[40..56].copy_from_slice(&encryption.key_reference);
    let wrap_nonce = random_array_12();
    header[56..68].copy_from_slice(&wrap_nonce);
    header[68..76].copy_from_slice(&content_nonce);
    let cipher = Aes256Gcm::new_from_slice(&encryption.key_encryption_key[..])
        .map_err(|_| StorageError::Cryptography)?;
    let wrapped_key = cipher
        .encrypt(
            Nonce::from_slice(&wrap_nonce),
            Payload {
                msg: data_key,
                aad: &header[..76],
            },
        )
        .map_err(|_| StorageError::Cryptography)?;
    if wrapped_key.len() != 48 {
        return Err(StorageError::Cryptography);
    }
    header[76..].copy_from_slice(&wrapped_key);
    Ok(header)
}

struct EncryptedReadState {
    file: File,
    data_key: Zeroizing<[u8; 32]>,
    object_id: ObjectId,
    content_nonce: [u8; 8],
    plaintext_size: u64,
    next_index: u32,
    end_index: u32,
    first_index: u32,
    first_skip: usize,
    output_remaining: u64,
}

async fn open_encrypted_payload(
    mut file: File,
    object_id: ObjectId,
    size: u64,
    range: Option<ResolvedByteRange>,
    encryption: &ObjectEncryption,
) -> Result<DownloadStream, StorageError> {
    let mut header = [0_u8; ENCRYPTED_PAYLOAD_HEADER_LEN];
    file.read_exact(&mut header)
        .await
        .map_err(|_| StorageError::InconsistentState)?;
    let (data_key, content_nonce) = decode_encrypted_header(&header, encryption, object_id, size)?;
    let chunk_count = encrypted_chunk_count(size)?;
    let expected_file_size = (ENCRYPTED_PAYLOAD_HEADER_LEN as u64)
        .checked_add(size)
        .and_then(|value| value.checked_add(u64::from(chunk_count) * AES_GCM_TAG_LEN as u64))
        .ok_or(StorageError::InconsistentState)?;
    let actual_file_size = file
        .metadata()
        .await
        .map_err(|source| filesystem("inspect encrypted payload", source))?
        .len();
    if actual_file_size != expected_file_size {
        return Err(StorageError::InconsistentState);
    }
    let (offset, length) = range.map_or((0, size), |range| (range.offset, range.length));
    let first_index = u32::try_from(offset / ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64)
        .map_err(|_| StorageError::InconsistentState)?;
    let end_index = if length == 0 {
        first_index
    } else {
        u32::try_from((offset + length - 1) / ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64)
            .map_err(|_| StorageError::InconsistentState)?
    };
    let encrypted_chunk_span = (ENCRYPTED_PAYLOAD_CHUNK_SIZE + AES_GCM_TAG_LEN) as u64;
    let physical_offset = (ENCRYPTED_PAYLOAD_HEADER_LEN as u64)
        .checked_add(u64::from(first_index) * encrypted_chunk_span)
        .ok_or(StorageError::InconsistentState)?;
    file.seek(SeekFrom::Start(physical_offset))
        .await
        .map_err(|source| filesystem("seek encrypted payload", source))?;
    let state = EncryptedReadState {
        file,
        data_key,
        object_id,
        content_nonce,
        plaintext_size: size,
        next_index: first_index,
        end_index,
        first_index,
        first_skip: (offset % ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64) as usize,
        output_remaining: length,
    };
    Ok(Box::pin(stream::try_unfold(
        state,
        |mut state| async move {
            if state.next_index > state.end_index {
                return Ok(None);
            }
            let index = state.next_index;
            let chunk_offset = u64::from(index) * ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64;
            let plaintext_len = if state.plaintext_size == 0 {
                0
            } else {
                usize::try_from(
                    (state.plaintext_size - chunk_offset).min(ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64),
                )
                .map_err(|_| StorageError::InconsistentState)?
            };
            let mut ciphertext = vec![0_u8; plaintext_len + AES_GCM_TAG_LEN];
            state
                .file
                .read_exact(&mut ciphertext)
                .await
                .map_err(|_| StorageError::InconsistentState)?;
            let cipher = Aes256Gcm::new_from_slice(&state.data_key[..])
                .map_err(|_| StorageError::Cryptography)?;
            let nonce = content_chunk_nonce(&state.content_nonce, index);
            let aad = content_chunk_aad(state.object_id, index, plaintext_len);
            let plaintext = cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| StorageError::IntegrityMismatch)?;
            let skip = if index == state.first_index {
                state.first_skip
            } else {
                0
            };
            let available = plaintext.len().saturating_sub(skip);
            let take = available.min(state.output_remaining as usize);
            let output = Bytes::copy_from_slice(&plaintext[skip..skip + take]);
            state.output_remaining -= take as u64;
            state.next_index = state.next_index.saturating_add(1);
            Ok(Some((output, state)))
        },
    )))
}

fn decode_encrypted_header(
    header: &[u8; ENCRYPTED_PAYLOAD_HEADER_LEN],
    encryption: &ObjectEncryption,
    object_id: ObjectId,
    size: u64,
) -> Result<(Zeroizing<[u8; 32]>, [u8; 8]), StorageError> {
    if &header[..8] != ENCRYPTED_PAYLOAD_MAGIC
        || u16::from_be_bytes([header[8], header[9]]) != OBJECT_ENCRYPTION_FORMAT_VERSION as u16
        || header[10] != 1
        || header[11] != 0
    {
        return Err(StorageError::InconsistentState);
    }
    let chunk_size = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);
    let encoded_size = u64::from_be_bytes([
        header[16], header[17], header[18], header[19], header[20], header[21], header[22],
        header[23],
    ]);
    if chunk_size != ENCRYPTED_PAYLOAD_CHUNK_SIZE as u32 || encoded_size != size {
        return Err(StorageError::InconsistentState);
    }
    if &header[24..40] != object_id.as_uuid().as_bytes()
        || header[40..56] != encryption.key_reference
    {
        return Err(StorageError::EncryptionKeyMismatch);
    }
    let mut wrap_nonce = [0_u8; 12];
    wrap_nonce.copy_from_slice(&header[56..68]);
    let mut content_nonce = [0_u8; 8];
    content_nonce.copy_from_slice(&header[68..76]);
    let cipher = Aes256Gcm::new_from_slice(&encryption.key_encryption_key[..])
        .map_err(|_| StorageError::Cryptography)?;
    let unwrapped = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&wrap_nonce),
                Payload {
                    msg: &header[76..],
                    aad: &header[..76],
                },
            )
            .map_err(|_| StorageError::IntegrityMismatch)?,
    );
    let mut data_key = Zeroizing::new([0_u8; 32]);
    if unwrapped.len() != data_key.len() {
        return Err(StorageError::InconsistentState);
    }
    data_key.copy_from_slice(&unwrapped);
    Ok((data_key, content_nonce))
}

fn encrypted_chunk_count(size: u64) -> Result<u32, StorageError> {
    let count = if size == 0 {
        1
    } else {
        size.div_ceil(ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64)
    };
    u32::try_from(count).map_err(|_| StorageError::Cryptography)
}

fn content_chunk_nonce(prefix: &[u8; 8], index: u32) -> [u8; 12] {
    let mut nonce = [0_u8; 12];
    nonce[..8].copy_from_slice(prefix);
    nonce[8..].copy_from_slice(&index.to_be_bytes());
    nonce
}

fn content_chunk_aad(object_id: ObjectId, index: u32, plaintext_len: usize) -> [u8; 32] {
    let mut aad = [0_u8; 32];
    aad[..8].copy_from_slice(ENCRYPTED_PAYLOAD_MAGIC);
    aad[8..24].copy_from_slice(object_id.as_uuid().as_bytes());
    aad[24..28].copy_from_slice(&index.to_be_bytes());
    aad[28..32].copy_from_slice(&(plaintext_len as u32).to_be_bytes());
    aad
}

fn random_array_32() -> [u8; 32] {
    let mut value = [0_u8; 32];
    OsRng.fill_bytes(&mut value);
    value
}

fn random_array_12() -> [u8; 12] {
    let mut value = [0_u8; 12];
    OsRng.fill_bytes(&mut value);
    value
}

fn random_array_8() -> [u8; 8] {
    let mut value = [0_u8; 8];
    OsRng.fill_bytes(&mut value);
    value
}

async fn inspect_consistency(
    store: &LocalFilesystemStore,
    maximum_entries: usize,
) -> Result<(StorageInspection, Vec<ObjectId>), StorageError> {
    if maximum_entries == 0 || maximum_entries > 1_000_000 {
        return Err(StorageError::Filesystem {
            operation: "validate storage inspection bound",
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "maximum_entries must be between 1 and 1000000",
            ),
        });
    }
    let mut report = StorageInspection::default();
    let mut cursor = None;
    loop {
        let remaining = maximum_entries.saturating_sub(report.metadata_payloads_scanned as usize);
        if remaining == 0 {
            report.truncated = true;
            break;
        }
        let page = store
            .metadata
            .list_payload_references(cursor, remaining.min(1_000))
            .await?;
        for object_id in page.object_ids {
            report.metadata_payloads_scanned = report.metadata_payloads_scanned.saturating_add(1);
            if !fs::try_exists(store.layout.payload_path(object_id))
                .await
                .map_err(|source| filesystem("inspect referenced payload", source))?
            {
                report.metadata_without_data = report.metadata_without_data.saturating_add(1);
                if report.missing_payload_samples.len() < 100 {
                    report.missing_payload_samples.push(object_id);
                }
            }
        }
        cursor = page.next_object_id;
        if cursor.is_none() {
            break;
        }
    }

    let mut orphan_payloads = Vec::new();
    let mut first_level = fs::read_dir(&store.layout.objects)
        .await
        .map_err(|source| filesystem("scan object data", source))?;
    'outer: while let Some(first) = first_level
        .next_entry()
        .await
        .map_err(|source| filesystem("read object data directory", source))?
    {
        if !first
            .file_type()
            .await
            .map_err(|source| filesystem("inspect object data directory", source))?
            .is_dir()
        {
            report.unknown_data_entries = report.unknown_data_entries.saturating_add(1);
            continue;
        }
        let mut second_level = fs::read_dir(first.path())
            .await
            .map_err(|source| filesystem("scan object data shard", source))?;
        while let Some(second) = second_level
            .next_entry()
            .await
            .map_err(|source| filesystem("read object data shard", source))?
        {
            if !second
                .file_type()
                .await
                .map_err(|source| filesystem("inspect object data shard", source))?
                .is_dir()
            {
                report.unknown_data_entries = report.unknown_data_entries.saturating_add(1);
                continue;
            }
            let mut payloads = fs::read_dir(second.path())
                .await
                .map_err(|source| filesystem("scan payload shard", source))?;
            while let Some(payload) = payloads
                .next_entry()
                .await
                .map_err(|source| filesystem("read payload shard", source))?
            {
                if report.data_payloads_scanned as usize >= maximum_entries {
                    report.truncated = true;
                    break 'outer;
                }
                let name = payload.file_name();
                let Some(name) = name.to_str() else {
                    report.unknown_data_entries = report.unknown_data_entries.saturating_add(1);
                    continue;
                };
                let Ok(uuid) = Uuid::parse_str(name) else {
                    report.unknown_data_entries = report.unknown_data_entries.saturating_add(1);
                    continue;
                };
                let object_id = ObjectId::from_uuid(uuid);
                if payload.path() != store.layout.payload_path(object_id)
                    || !payload
                        .file_type()
                        .await
                        .map_err(|source| filesystem("inspect payload", source))?
                        .is_file()
                {
                    report.unknown_data_entries = report.unknown_data_entries.saturating_add(1);
                    continue;
                }
                report.data_payloads_scanned = report.data_payloads_scanned.saturating_add(1);
                if !store.metadata.payload_referenced(object_id).await? {
                    report.data_without_metadata = report.data_without_metadata.saturating_add(1);
                    if report.orphan_payload_samples.len() < 100 {
                        report.orphan_payload_samples.push(object_id);
                    }
                    orphan_payloads.push(object_id);
                }
            }
        }
    }

    let mut temporary = fs::read_dir(&store.layout.temporary)
        .await
        .map_err(|source| filesystem("scan temporary state", source))?;
    while let Some(entry) = temporary
        .next_entry()
        .await
        .map_err(|source| filesystem("read temporary state", source))?
    {
        let name = entry.file_name();
        let recognized = name.to_str().is_some_and(|name| {
            is_recognized_upload_name(name)
                || name
                    .strip_suffix(".publish")
                    .is_some_and(|id| Uuid::parse_str(id).is_ok())
        });
        if recognized {
            report.recognized_temporary_entries =
                report.recognized_temporary_entries.saturating_add(1);
        } else {
            report.unknown_temporary_entries = report.unknown_temporary_entries.saturating_add(1);
        }
    }
    Ok((report, orphan_payloads))
}

async fn initialize_storage_format(layout: &StorageLayout) -> Result<(), StorageError> {
    let path = layout.system.join("storage-format.json");
    match fs::read(&path).await {
        Ok(encoded) => {
            if encoded.len() > 4_096 {
                return Err(filesystem(
                    "read storage format",
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "storage format record is oversized",
                    ),
                ));
            }
            let record: StorageFormatRecord = serde_json::from_slice(&encoded)?;
            if record.storage_format_version != STORAGE_FORMAT_VERSION {
                return Err(filesystem(
                    "check storage format",
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "storage format {} is unsupported by format {}",
                            record.storage_format_version, STORAGE_FORMAT_VERSION
                        ),
                    ),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let encoded = serde_json::to_vec(&StorageFormatRecord {
                storage_format_version: STORAGE_FORMAT_VERSION,
            })?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
                .map_err(|source| filesystem("create storage format", source))?;
            file.write_all(&encoded)
                .await
                .map_err(|source| filesystem("write storage format", source))?;
            file.sync_all()
                .await
                .map_err(|source| filesystem("synchronize storage format", source))?;
            sync_directory(layout.system.clone()).await
        }
        Err(source) => Err(filesystem("read storage format", source)),
    }
}

async fn initialize_object_encryption(
    layout: &StorageLayout,
    master_key: Option<&[u8]>,
) -> Result<Option<ObjectEncryption>, StorageError> {
    let encryption = master_key.map(derive_object_encryption).transpose()?;
    let path = layout.system.join("object-encryption.json");
    match fs::read(&path).await {
        Ok(encoded) => {
            if encoded.len() > 4_096 {
                return Err(StorageError::InconsistentState);
            }
            let record: ObjectEncryptionRecord = serde_json::from_slice(&encoded)?;
            if record.encryption_format_version != OBJECT_ENCRYPTION_FORMAT_VERSION
                || record.algorithm != OBJECT_ENCRYPTION_ALGORITHM
            {
                return Err(StorageError::InconsistentState);
            }
            let encryption = encryption.ok_or(StorageError::EncryptionKeyRequired)?;
            if record.key_reference != hex::encode(encryption.key_reference) {
                return Err(StorageError::EncryptionKeyMismatch);
            }
            Ok(Some(encryption))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let Some(encryption) = encryption else {
                return Ok(None);
            };
            let encoded = serde_json::to_vec(&ObjectEncryptionRecord {
                encryption_format_version: OBJECT_ENCRYPTION_FORMAT_VERSION,
                algorithm: OBJECT_ENCRYPTION_ALGORITHM.to_owned(),
                key_reference: hex::encode(encryption.key_reference),
            })?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
                .map_err(|source| filesystem("create object encryption format", source))?;
            file.write_all(&encoded)
                .await
                .map_err(|source| filesystem("write object encryption format", source))?;
            file.sync_all()
                .await
                .map_err(|source| filesystem("synchronize object encryption format", source))?;
            sync_directory(layout.system.clone()).await?;
            Ok(Some(encryption))
        }
        Err(source) => Err(filesystem("read object encryption format", source)),
    }
}

fn derive_object_encryption(master_key: &[u8]) -> Result<ObjectEncryption, StorageError> {
    let derivation = hkdf::Hkdf::<Sha256>::new(Some(b"oes-object-encryption-v1"), master_key);
    let mut key = Zeroizing::new([0_u8; 32]);
    derivation
        .expand(b"object-key-encryption-key", &mut *key)
        .map_err(|_| StorageError::Cryptography)?;
    let digest = Sha256::digest(&key[..]);
    let mut key_reference = [0_u8; 16];
    key_reference.copy_from_slice(&digest[..16]);
    Ok(ObjectEncryption {
        key_encryption_key: Arc::new(key),
        key_reference,
    })
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
    if let Some(id) = name.strip_suffix(".upload") {
        return Uuid::parse_str(id).is_ok();
    }
    // An abandoned replica transfer is recognized so that a restart cleans it up
    // instead of leaking staged bytes.
    name.strip_suffix(".replica")
        .and_then(|scoped| scoped.split_once('-'))
        .is_some_and(|(id, scope)| {
            Uuid::parse_str(id).is_ok()
                && scope.len() == 16
                && scope.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

/// The size and checksum a replica must match to be published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaCommitment {
    /// Logical payload length.
    pub size: u64,
    /// Logical payload checksum.
    pub checksum: Checksum,
}

/// How a replica transfer's expected content is established.
pub enum ReplicaExpectation {
    /// The expectation comes from authoritative metadata before the transfer.
    ///
    /// Repair and rebalance use this: the target validates against committed
    /// metadata rather than against anything the source node says.
    Known(ReplicaCommitment),
    /// The expectation arrives after the last byte.
    ///
    /// A client upload is replicated while it streams, so its checksum is only
    /// known once the upload ends. The receiving node still computes its own
    /// checksum over the bytes it stored and refuses a mismatch.
    Trailing(tokio::sync::oneshot::Receiver<Result<ReplicaCommitment, String>>),
}

/// Parameters for streaming one replica onto this node.
pub struct WriteReplicaRequest {
    /// Stable identity of the replication operation.
    ///
    /// Retrying the same operation must not create a second logical replica, so
    /// the identity is carried explicitly rather than inferred from timing.
    pub operation_id: String,
    /// Immutable payload identifier the replica stores.
    pub object_id: ObjectId,
    /// How the expected content is established.
    pub expectation: ReplicaExpectation,
    /// Incoming payload chunks.
    pub body: UploadStream,
}

impl WriteReplicaRequest {
    /// Creates a transfer whose expectation is already known.
    #[must_use]
    pub fn known(
        operation_id: impl Into<String>,
        object_id: ObjectId,
        size: u64,
        checksum: Checksum,
        body: UploadStream,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            object_id,
            expectation: ReplicaExpectation::Known(ReplicaCommitment { size, checksum }),
            body,
        }
    }

    /// Creates a transfer whose expectation arrives after the last byte.
    #[must_use]
    pub fn trailing(
        operation_id: impl Into<String>,
        object_id: ObjectId,
        commitment: tokio::sync::oneshot::Receiver<Result<ReplicaCommitment, String>>,
        body: UploadStream,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            object_id,
            expectation: ReplicaExpectation::Trailing(commitment),
            body,
        }
    }
}

/// The outcome of writing one replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaWriteResult {
    /// Payload identifier that was stored.
    pub object_id: ObjectId,
    /// Logical bytes written.
    pub size: u64,
    /// Checksum calculated locally while streaming.
    pub checksum: Checksum,
    /// Whether a verified replica already existed and nothing was rewritten.
    pub already_present: bool,
}

/// Parameters for reading a replica.
#[derive(Debug, Clone)]
pub struct ReadReplicaRequest {
    /// Payload identifier to read.
    pub object_id: ObjectId,
    /// Logical payload length recorded in metadata.
    pub size: u64,
    /// Physical representation recorded in metadata.
    pub payload_format: PayloadFormat,
    /// Optional byte range.
    pub range: Option<ByteRange>,
    /// Checksum to verify while streaming a whole payload.
    ///
    /// Verification is only meaningful for a complete read, so a ranged read
    /// carries no expectation.
    pub expected_checksum: Option<Checksum>,
}

/// A replica read that verifies integrity as bytes are produced.
pub struct ReplicaReadResult {
    /// Logical payload length.
    pub size: u64,
    /// Resolved range when a partial read was requested.
    pub range: Option<ResolvedByteRange>,
    /// Payload chunks read lazily with backpressure.
    pub body: DownloadStream,
}

/// Local measurement of one replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaStat {
    /// Physical bytes occupied on disk.
    pub physical_bytes: u64,
    /// Last time the durable bytes changed.
    ///
    /// Used by conservative garbage collection: a payload the cluster does not
    /// know about may simply belong to a commit that has not arrived yet, so age
    /// is what distinguishes a genuine orphan from an in-flight write.
    pub modified_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The result of verifying a replica's stored bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaVerification {
    /// Whether the payload exists locally.
    pub present: bool,
    /// Whether the recomputed checksum matched the expectation.
    pub matches: bool,
    /// Logical bytes read.
    pub size: u64,
    /// Checksum recomputed from the stored bytes.
    pub checksum: Option<Checksum>,
}

/// Local replica operations used by cluster replication, repair, and rebalance.
///
/// These operate on immutable payloads by identifier and never consult object
/// metadata, which is what lets one implementation serve object versions,
/// multipart parts, and repair traffic alike.
#[async_trait]
pub trait ReplicaStore: Send + Sync {
    /// Streams a replica onto this node and verifies it before publishing.
    async fn write_replica(
        &self,
        request: WriteReplicaRequest,
    ) -> Result<ReplicaWriteResult, StorageError>;

    /// Opens a replica for streaming, verifying integrity as bytes are read.
    async fn read_replica(
        &self,
        request: ReadReplicaRequest,
    ) -> Result<ReplicaReadResult, StorageError>;

    /// Removes a replica's bytes, reporting whether anything was removed.
    async fn delete_replica(&self, object_id: ObjectId) -> Result<bool, StorageError>;

    /// Recomputes and compares a replica's checksum.
    async fn verify_replica(
        &self,
        object_id: ObjectId,
        size: u64,
        payload_format: PayloadFormat,
        expected: Checksum,
    ) -> Result<ReplicaVerification, StorageError>;

    /// Measures a replica without reading its contents.
    async fn stat_replica(&self, object_id: ObjectId) -> Result<Option<ReplicaStat>, StorageError>;

    /// Lists payload identifiers this node physically stores.
    ///
    /// Used to reconcile a returning node's local bytes against authoritative
    /// placement metadata.
    async fn list_local_payloads(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, StorageError>;

    /// Measures this node's own filesystem.
    ///
    /// Capacity is reported by the node that owns the disk. Cluster metadata
    /// records what it was told, so the measurement itself must come from here.
    async fn local_capacity(&self) -> Result<StorageStatus, StorageError>;
}

#[async_trait]
impl ReplicaStore for LocalFilesystemStore {
    async fn write_replica(
        &self,
        mut request: WriteReplicaRequest,
    ) -> Result<ReplicaWriteResult, StorageError> {
        let object_id = request.object_id;
        // A verified replica already present means a retried transfer, not a
        // second replica: report success without touching durable bytes.
        if let ReplicaExpectation::Known(commitment) = &request.expectation
            && self.payload_size(object_id).await?.is_some()
        {
            let verification = self
                .verify_replica(
                    object_id,
                    commitment.size,
                    self.local_payload_format(),
                    commitment.checksum.clone(),
                )
                .await?;
            if verification.present && verification.matches {
                return Ok(ReplicaWriteResult {
                    object_id,
                    size: verification.size,
                    checksum: commitment.checksum.clone(),
                    already_present: true,
                });
            }
        }

        let temporary_path = self
            .layout
            .replica_temporary_path(object_id, &request.operation_id);
        let mut temporary_cleanup = TemporaryFileGuard::new(temporary_path.clone());
        if let Some(parent) = temporary_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|source| filesystem("create replica staging directory", source))?;
        }
        let mut temporary_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary_path)
            .await
            .map_err(|source| filesystem("create replica staging file", source))?;
        let written = self
            .write_payload(&mut temporary_file, object_id, &mut request.body)
            .await?;
        temporary_file
            .flush()
            .await
            .map_err(|source| filesystem("flush replica", source))?;
        temporary_file
            .sync_all()
            .await
            .map_err(|source| filesystem("synchronize replica", source))?;
        drop(temporary_file);

        // The receiving node verifies independently: a source node's claim about
        // its own bytes is never sufficient to accept a replica.
        let commitment = match request.expectation {
            ReplicaExpectation::Known(commitment) => commitment,
            ReplicaExpectation::Trailing(receiver) => match receiver.await {
                Ok(Ok(commitment)) => commitment,
                Ok(Err(reason)) => {
                    return Err(StorageError::UploadStream(io::Error::other(reason)));
                }
                Err(_) => {
                    return Err(StorageError::UploadStream(io::Error::other(
                        "replica transfer ended without a commitment",
                    )));
                }
            },
        };
        if written.checksum != commitment.checksum || written.size != commitment.size {
            return Err(StorageError::ChecksumMismatch {
                expected: commitment.checksum,
                actual: written.checksum,
            });
        }

        let publication_path = self.layout.publication_path(object_id);
        let mut publication_cleanup = TemporaryFileGuard::new(publication_path.clone());
        write_publication_record(
            &publication_path,
            &PublicationRecord {
                object_id,
                bucket_id: None,
                key: None,
            },
        )
        .await?;

        let payload_path = self.layout.payload_path(object_id);
        let payload_parent = payload_path
            .parent()
            .ok_or_else(|| StorageError::Filesystem {
                operation: "resolve replica parent",
                source: io::Error::other("payload path has no parent"),
            })?;
        fs::create_dir_all(payload_parent)
            .await
            .map_err(|source| filesystem("create replica shard", source))?;
        fs::rename(&temporary_path, &payload_path)
            .await
            .map_err(|source| filesystem("publish replica", source))?;
        temporary_cleanup.disarm();
        publication_cleanup.disarm();
        if let Err(error) = sync_directory(payload_parent.to_path_buf()).await {
            if cleanup_file(&payload_path).await {
                cleanup_file(&publication_path).await;
            }
            return Err(error);
        }
        Ok(ReplicaWriteResult {
            object_id,
            size: written.size,
            checksum: written.checksum,
            already_present: false,
        })
    }

    async fn read_replica(
        &self,
        request: ReadReplicaRequest,
    ) -> Result<ReplicaReadResult, StorageError> {
        let (range, body) = self
            .open_payload(
                request.object_id,
                request.size,
                request.payload_format,
                request.range,
            )
            .await?;
        let body = match request.expected_checksum {
            Some(expected) if range.is_none() => verifying_stream(body, expected),
            _ => body,
        };
        Ok(ReplicaReadResult {
            size: request.size,
            range,
            body,
        })
    }

    async fn delete_replica(&self, object_id: ObjectId) -> Result<bool, StorageError> {
        match fs::remove_file(self.layout.payload_path(object_id)).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(filesystem("remove replica", source)),
        }
    }

    async fn verify_replica(
        &self,
        object_id: ObjectId,
        size: u64,
        payload_format: PayloadFormat,
        expected: Checksum,
    ) -> Result<ReplicaVerification, StorageError> {
        let opened = self
            .open_payload(object_id, size, payload_format, None)
            .await;
        let mut body = match opened {
            Ok((_, body)) => body,
            Err(StorageError::InconsistentState) => {
                return Ok(ReplicaVerification {
                    present: false,
                    matches: false,
                    size: 0,
                    checksum: None,
                });
            }
            Err(error) => return Err(error),
        };
        let mut hasher = Sha256::new();
        let mut read = 0_u64;
        while let Some(chunk) = body.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                // A payload that cannot be decoded is corrupt, not a failure of
                // the verification operation itself.
                Err(StorageError::Cryptography | StorageError::IntegrityMismatch) => {
                    return Ok(ReplicaVerification {
                        present: true,
                        matches: false,
                        size: read,
                        checksum: None,
                    });
                }
                Err(error) => return Err(error),
            };
            read = read.saturating_add(chunk.len() as u64);
            hasher.update(&chunk);
        }
        let checksum = Checksum::sha256(hasher.finalize().into());
        Ok(ReplicaVerification {
            matches: checksum == expected && read == size,
            present: true,
            size: read,
            checksum: Some(checksum),
        })
    }

    async fn stat_replica(&self, object_id: ObjectId) -> Result<Option<ReplicaStat>, StorageError> {
        match fs::metadata(self.layout.payload_path(object_id)).await {
            Ok(metadata) => Ok(Some(ReplicaStat {
                physical_bytes: metadata.len(),
                modified_at: metadata
                    .modified()
                    .ok()
                    .map(chrono::DateTime::<chrono::Utc>::from),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(filesystem("inspect replica", source)),
        }
    }

    async fn local_capacity(&self) -> Result<StorageStatus, StorageError> {
        ObjectStore::status(self).await
    }

    async fn list_local_payloads(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, StorageError> {
        let limit = limit.clamp(1, 100_000);
        let mut found = std::collections::BTreeSet::new();
        let mut shards = fs::read_dir(&self.layout.objects)
            .await
            .map_err(|source| filesystem("scan replica shards", source))?;
        while let Some(shard) = shards
            .next_entry()
            .await
            .map_err(|source| filesystem("read replica shard", source))?
        {
            if !shard
                .file_type()
                .await
                .map_err(|source| filesystem("inspect replica shard", source))?
                .is_dir()
            {
                continue;
            }
            let mut inner = fs::read_dir(shard.path())
                .await
                .map_err(|source| filesystem("scan replica subshard", source))?;
            while let Some(subshard) = inner
                .next_entry()
                .await
                .map_err(|source| filesystem("read replica subshard", source))?
            {
                if !subshard
                    .file_type()
                    .await
                    .map_err(|source| filesystem("inspect replica subshard", source))?
                    .is_dir()
                {
                    continue;
                }
                let mut payloads = fs::read_dir(subshard.path())
                    .await
                    .map_err(|source| filesystem("scan replicas", source))?;
                while let Some(payload) = payloads
                    .next_entry()
                    .await
                    .map_err(|source| filesystem("read replica", source))?
                {
                    let name = payload.file_name();
                    let Some(name) = name.to_str() else { continue };
                    let Ok(uuid) = Uuid::parse_str(name) else {
                        continue;
                    };
                    let object_id = ObjectId::from_uuid(uuid);
                    if after.is_some_and(|after| object_id <= after) {
                        continue;
                    }
                    found.insert(object_id);
                    if found.len() > limit {
                        found.pop_last();
                    }
                }
            }
        }
        Ok(found.into_iter().collect())
    }
}

/// Wraps a download stream so a mismatch fails the read instead of the client
/// silently receiving corrupt bytes.
fn verifying_stream(body: DownloadStream, expected: Checksum) -> DownloadStream {
    struct State {
        hasher: Sha256,
        expected: Checksum,
    }
    let state = Arc::new(Mutex::new(Some(State {
        hasher: Sha256::new(),
        expected,
    })));
    let finish = Arc::clone(&state);
    let verified = body
        .map(move |chunk| {
            let chunk = chunk?;
            let mut guard = state.lock().map_err(|_| StorageError::Coordination)?;
            if let Some(state) = guard.as_mut() {
                state.hasher.update(&chunk);
            }
            Ok(chunk)
        })
        .chain(stream::once(async move {
            let taken = finish
                .lock()
                .map_err(|_| StorageError::Coordination)?
                .take();
            match taken {
                Some(state) => {
                    let actual = Checksum::sha256(state.hasher.finalize().into());
                    if actual == state.expected {
                        Ok(Bytes::new())
                    } else {
                        Err(StorageError::IntegrityMismatch)
                    }
                }
                None => Ok(Bytes::new()),
            }
        }))
        .try_filter(|chunk| std::future::ready(!chunk.is_empty()));
    Box::pin(verified)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use oes_core::{Bucket, BucketName, BucketQuota, OrganizationId, VersioningState};
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
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
            durability_policy: None,
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
                bucket_id: Some(bucket.id),
                key: Some(ObjectKey::new("never-visible").expect("key")),
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

    #[tokio::test]
    async fn inspection_and_explicit_repair_handle_only_owned_orphan_payloads() {
        let directory = tempdir().expect("temporary directory");
        let metadata = Arc::new(
            RedbMetadataRepository::open(directory.path().join("catalog.redb"))
                .await
                .expect("metadata"),
        );
        let metadata_dependency: Arc<dyn MetadataRepository> = metadata;
        let store = LocalFilesystemStore::open(
            directory.path().join("data"),
            directory.path().join("tmp"),
            metadata_dependency,
        )
        .await
        .expect("store");
        let orphan = ObjectId::new();
        let path = store.layout.payload_path(orphan);
        fs::create_dir_all(path.parent().expect("payload parent"))
            .await
            .expect("payload directory");
        fs::write(&path, b"orphan").await.expect("orphan payload");

        let inspection = store.inspect(100).await.expect("inspection");
        assert_eq!(inspection.data_without_metadata, 1);
        assert_eq!(inspection.orphan_payload_samples, vec![orphan]);

        let dry_run = store
            .repair(StorageRepairRequest {
                maximum_entries: 100,
                dry_run: true,
            })
            .await
            .expect("dry run");
        assert_eq!(dry_run.removed_orphan_payloads, 0);
        assert!(fs::try_exists(&path).await.expect("exists"));

        let repaired = store
            .repair(StorageRepairRequest {
                maximum_entries: 100,
                dry_run: false,
            })
            .await
            .expect("repair");
        assert_eq!(repaired.removed_orphan_payloads, 1);
        assert!(!fs::try_exists(path).await.expect("removed"));
    }
}
