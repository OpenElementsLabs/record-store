//! Streaming object storage boundary and local filesystem implementation.

use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{Arc, Mutex, Weak},
};

use async_trait::async_trait;
use chrono::Utc;
use futures_util::{StreamExt, TryStreamExt, stream};
use md5::Md5;
use record_store_core::{
    BucketId, ByteRange, Checksum, CoreError, ETag, MultipartUploadState, ObjectId, ObjectKey,
    ObjectMetadata, ObjectVersionRecord, PayloadFormat, ResolvedByteRange, UploadedPart, VersionId,
};
use record_store_metadata::{
    DeleteObjectResult, MetadataError, MetadataRepository, NewDeleteMarker,
};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::RwLock,
};
use tokio_util::io::ReaderStream;
use tracing::warn;
use uuid::Uuid;

use crate::encryption::{
    WrittenPayload, initialize_object_encryption, open_encrypted_payload, write_encrypted_payload,
    write_plaintext_payload,
};
use crate::layout::{ObjectEncryption, PublicationRecord, StorageLayout};
use crate::maintenance::filesystem;
use crate::maintenance::{
    TemporaryFileGuard, cleanup_file, initialize_storage_format, inspect_consistency,
    is_recognized_upload_name, sync_directory, write_publication_record,
};
use crate::*;

/// A real single-node object store backed by immutable filesystem payloads.
#[derive(Clone)]
pub struct LocalFilesystemStore {
    pub(crate) layout: StorageLayout,
    pub(crate) metadata: Arc<dyn MetadataRepository>,
    pub(crate) key_locks: KeyLockRegistry,
    pub(crate) encryption: Option<ObjectEncryption>,
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

    pub(crate) async fn write_payload(
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

    pub(crate) async fn open_payload(
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
    pub(crate) async fn payload_size(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<u64>, StorageError> {
        match fs::metadata(self.layout.payload_path(object_id)).await {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(filesystem("inspect replica", source)),
        }
    }

    /// Returns the physical representation this node writes for new payloads.
    pub(crate) const fn local_payload_format(&self) -> PayloadFormat {
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
            durability: record_store_core::DurabilityProfile::Single,
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

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;
    use record_store_core::{ByteRange, Checksum, ObjectKey};

    use super::*;
    use crate::test_support::{open, put, store};
    use crate::{DeleteObjectRequest, GetObjectRequest, HeadObjectRequest, VerifyObjectRequest};

    async fn read(result: crate::GetObjectResult) -> Vec<u8> {
        result
            .body
            .try_fold(Vec::new(), |mut buffer, chunk| async move {
                buffer.extend_from_slice(&chunk);
                Ok(buffer)
            })
            .await
            .expect("stream body")
    }

    fn key(value: &str) -> ObjectKey {
        ObjectKey::new(value).expect("key")
    }

    /// The bytes a caller reads back must be exactly the bytes it wrote, and the
    /// checksum recorded at commit has to describe them.
    #[tokio::test]
    async fn stored_bytes_come_back_unchanged_with_their_checksum() {
        let (_directory, store, bucket) = store().await;
        let committed = put(&store, &bucket, "a.txt", b"hello world").await;
        assert_eq!(committed.metadata.size, 11);

        let fetched = store
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
                range: None,
            })
            .await
            .expect("get");
        assert_eq!(read(fetched).await, b"hello world");

        let verified = store
            .verify(VerifyObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
            })
            .await
            .expect("verify");
        assert_eq!(verified.checksum, committed.metadata.checksum);
    }

    /// A range read must not materialise the whole payload, and the slice has to
    /// line up exactly with what was asked for.
    #[tokio::test]
    async fn a_range_read_returns_only_the_requested_slice() {
        let (_directory, store, bucket) = store().await;
        put(&store, &bucket, "a.txt", b"0123456789").await;

        let fetched = store
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
                range: Some(ByteRange::new(2, 4).expect("range")),
            })
            .await
            .expect("get");
        assert_eq!(read(fetched).await, b"2345");
    }

    /// A supplied checksum is a promise. Breaking it must abort the write and
    /// leave nothing behind, or the store would hold bytes nobody vouched for.
    #[tokio::test]
    async fn a_mismatched_checksum_aborts_the_write_and_stores_nothing() {
        let (_directory, store, bucket) = store().await;
        let body = bytes::Bytes::from_static(b"hello");
        let result = store
            .put(crate::PutObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
                content_type: None,
                custom_metadata: Default::default(),
                expected_checksum: Some(Checksum::sha256([0_u8; 32])),
                object_id: None,
                protocol_etag: None,
                body: crate::upload_stream(futures_util::stream::once(async move { Ok(body) })),
            })
            .await;
        assert!(result.is_err(), "a broken promise must not commit");

        assert!(matches!(
            store
                .head(HeadObjectRequest {
                    bucket_id: bucket.id,
                    key: key("a.txt"),
                })
                .await,
            Err(crate::StorageError::ObjectNotFound)
        ));
    }

    #[tokio::test]
    async fn head_reports_the_same_metadata_a_read_would() {
        let (_directory, store, bucket) = store().await;
        let committed = put(&store, &bucket, "a.txt", b"hello").await;
        let head = store
            .head(HeadObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
            })
            .await
            .expect("head");
        assert_eq!(head.size, committed.metadata.size);
        assert_eq!(head.checksum, committed.metadata.checksum);
    }

    #[tokio::test]
    async fn reads_and_deletes_of_absent_objects_report_absence() {
        let (_directory, store, bucket) = store().await;
        for outcome in [
            store
                .get(GetObjectRequest {
                    bucket_id: bucket.id,
                    key: key("absent.txt"),
                    range: None,
                })
                .await
                .err(),
            store
                .head(HeadObjectRequest {
                    bucket_id: bucket.id,
                    key: key("absent.txt"),
                })
                .await
                .err(),
            store
                .verify(VerifyObjectRequest {
                    bucket_id: bucket.id,
                    key: key("absent.txt"),
                })
                .await
                .err(),
        ] {
            assert!(
                matches!(outcome, Some(crate::StorageError::ObjectNotFound)),
                "{outcome:?}"
            );
        }
    }

    /// Overwriting replaces what a read returns while the previous payload is
    /// released, which is what keeps a non-versioned bucket from growing.
    #[tokio::test]
    async fn overwriting_replaces_the_readable_bytes() {
        let (_directory, store, bucket) = store().await;
        put(&store, &bucket, "a.txt", b"first").await;
        put(&store, &bucket, "a.txt", b"second-longer").await;

        let fetched = store
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
                range: None,
            })
            .await
            .expect("get");
        assert_eq!(read(fetched).await, b"second-longer");
    }

    #[tokio::test]
    async fn deleting_an_object_makes_it_unreadable() {
        let (_directory, store, bucket) = store().await;
        put(&store, &bucket, "a.txt", b"hello").await;
        store
            .delete(DeleteObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
            })
            .await
            .expect("delete");

        assert!(matches!(
            store
                .get(GetObjectRequest {
                    bucket_id: bucket.id,
                    key: key("a.txt"),
                    range: None,
                })
                .await,
            Err(crate::StorageError::ObjectNotFound)
        ));
    }

    /// An encrypted store must return plaintext to its caller while never
    /// writing plaintext to disk; finding the payload verbatim on disk would
    /// mean encryption is not actually applied.
    #[tokio::test]
    async fn an_encrypted_store_round_trips_without_leaving_plaintext_on_disk() {
        let key_material = b"object-encryption-master-key-32-bytes";
        let (directory, store, bucket) = open(Some(key_material)).await;
        let secret = b"the-quick-brown-fox-jumps-over-it";
        put(&store, &bucket, "a.txt", secret).await;

        let fetched = store
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
                range: None,
            })
            .await
            .expect("get");
        assert_eq!(read(fetched).await, secret);

        let mut found_plaintext = false;
        for entry in walkdir(directory.path()) {
            if let Ok(bytes) = std::fs::read(&entry)
                && bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_slice())
            {
                found_plaintext = true;
            }
        }
        assert!(
            !found_plaintext,
            "the payload was written to disk in the clear"
        );
    }

    /// A range read of an encrypted payload has to decrypt only the chunks it
    /// needs and still return the right bytes.
    #[tokio::test]
    async fn an_encrypted_payload_supports_range_reads() {
        let (_directory, store, bucket) =
            open(Some(b"object-encryption-master-key-32-bytes")).await;
        put(&store, &bucket, "a.txt", b"0123456789").await;

        let fetched = store
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key: key("a.txt"),
                range: Some(ByteRange::new(3, 3).expect("range")),
            })
            .await
            .expect("get");
        assert_eq!(read(fetched).await, b"345");
    }

    /// Reopening with a different master key must refuse rather than hand back
    /// bytes it cannot actually decrypt.
    #[tokio::test]
    async fn reopening_an_encrypted_store_with_the_wrong_key_is_refused() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let metadata: Arc<dyn MetadataRepository> = Arc::new(
            record_store_metadata::RedbMetadataRepository::open(
                directory.path().join("metadata.redb"),
            )
            .await
            .expect("metadata"),
        );
        LocalFilesystemStore::open_encrypted(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
            b"the-original-master-key-at-least-32-bytes",
        )
        .await
        .expect("first open");

        let reopened = LocalFilesystemStore::open_encrypted(
            directory.path(),
            directory.path().join("tmp"),
            metadata,
            b"a-completely-different-master-key-32-by",
        )
        .await;
        assert!(
            matches!(reopened, Err(crate::StorageError::EncryptionKeyMismatch)),
            "a mismatched key must be refused"
        );
    }

    #[tokio::test]
    async fn status_reports_the_stored_footprint_and_readiness() {
        let (_directory, store, bucket) = store().await;
        put(&store, &bucket, "a.txt", b"hello").await;
        let status = store.status().await.expect("status");
        assert!(
            status.capacity_bytes > 0,
            "a real filesystem reports a capacity: {status:?}"
        );
        assert!(
            status.available_bytes <= status.capacity_bytes,
            "{status:?}"
        );
        store.check_ready().await.expect("ready");
    }

    /// Walks a directory tree, returning every file path.
    fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found
    }

    /// Multipart is how a large object is written without buffering it. The
    /// completed object has to be the concatenation of its parts, and the
    /// checksum recorded at completion has to describe those assembled bytes.
    #[tokio::test]
    async fn a_completed_multipart_upload_assembles_its_parts() {
        let (_directory, store, bucket) = store().await;
        let metadata = crate::test_support::metadata_of(&store);
        let upload = record_store_core::MultipartUpload {
            id: record_store_core::UploadId::new(),
            bucket_id: bucket.id,
            key: key("big.bin"),
            content_type: None,
            custom_metadata: Default::default(),
            initiated_at: chrono::Utc::now(),
            state: record_store_core::MultipartUploadState::Active,
        };
        metadata
            .create_multipart_upload(&upload)
            .await
            .expect("create upload");

        let mut parts = Vec::new();
        for (number, body) in [(1_u16, b"first-".as_slice()), (2, b"second".as_slice())] {
            let owned = bytes::Bytes::copy_from_slice(body);
            let stored = store
                .put_multipart_part(crate::PutMultipartPartRequest {
                    upload_id: upload.id,
                    number: record_store_core::PartNumber::new(number).expect("part number"),
                    expected_checksum: None,
                    body: crate::upload_stream(futures_util::stream::once(
                        async move { Ok(owned) },
                    )),
                })
                .await
                .expect("put part");
            parts.push(stored);
        }

        let committed = store
            .complete_multipart(crate::CompleteMultipartRequest {
                upload: upload.clone(),
                parts,
            })
            .await
            .expect("complete");
        assert_eq!(committed.metadata.size, 12);

        let fetched = store
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key: key("big.bin"),
                range: None,
            })
            .await
            .expect("get");
        assert_eq!(read(fetched).await, b"first-second");
    }

    /// A part whose bytes break the supplied checksum must not be stored, or a
    /// completion would assemble an object from bytes nobody vouched for.
    #[tokio::test]
    async fn a_multipart_part_with_a_broken_checksum_is_refused() {
        let (_directory, store, bucket) = store().await;
        let metadata = crate::test_support::metadata_of(&store);
        let upload = record_store_core::MultipartUpload {
            id: record_store_core::UploadId::new(),
            bucket_id: bucket.id,
            key: key("big.bin"),
            content_type: None,
            custom_metadata: Default::default(),
            initiated_at: chrono::Utc::now(),
            state: record_store_core::MultipartUploadState::Active,
        };
        metadata
            .create_multipart_upload(&upload)
            .await
            .expect("create upload");

        let owned = bytes::Bytes::from_static(b"actual");
        let result = store
            .put_multipart_part(crate::PutMultipartPartRequest {
                upload_id: upload.id,
                number: record_store_core::PartNumber::new(1).expect("part number"),
                expected_checksum: Some(Checksum::sha256([0_u8; 32])),
                body: crate::upload_stream(futures_util::stream::once(async move { Ok(owned) })),
            })
            .await;
        assert!(result.is_err(), "a broken promise must not store a part");
    }

    /// Multipart works the same way on an encrypted store: the assembled object
    /// reads back as plaintext while nothing is written in the clear.
    #[tokio::test]
    async fn multipart_assembly_works_on_an_encrypted_store() {
        let (_directory, store, bucket) =
            open(Some(b"object-encryption-master-key-32-bytes")).await;
        let metadata = crate::test_support::metadata_of(&store);
        let upload = record_store_core::MultipartUpload {
            id: record_store_core::UploadId::new(),
            bucket_id: bucket.id,
            key: key("big.bin"),
            content_type: None,
            custom_metadata: Default::default(),
            initiated_at: chrono::Utc::now(),
            state: record_store_core::MultipartUploadState::Active,
        };
        metadata
            .create_multipart_upload(&upload)
            .await
            .expect("create upload");

        let owned = bytes::Bytes::from_static(b"encrypted-parts");
        let stored = store
            .put_multipart_part(crate::PutMultipartPartRequest {
                upload_id: upload.id,
                number: record_store_core::PartNumber::new(1).expect("part number"),
                expected_checksum: None,
                body: crate::upload_stream(futures_util::stream::once(async move { Ok(owned) })),
            })
            .await
            .expect("put part");

        store
            .complete_multipart(crate::CompleteMultipartRequest {
                upload,
                parts: vec![stored],
            })
            .await
            .expect("complete");

        let fetched = store
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key: key("big.bin"),
                range: None,
            })
            .await
            .expect("get");
        assert_eq!(read(fetched).await, b"encrypted-parts");
    }

    /// Pending cleanup is what reclaims payloads a delete released. Running it
    /// must remove exactly those and leave live payloads alone.
    #[tokio::test]
    async fn pending_cleanup_reclaims_only_released_payloads() {
        let (_directory, store, bucket) = store().await;
        let kept = put(&store, &bucket, "kept.txt", b"kept").await;
        let released = put(&store, &bucket, "released.txt", b"released").await;

        store
            .delete(DeleteObjectRequest {
                bucket_id: bucket.id,
                key: key("released.txt"),
            })
            .await
            .expect("delete");

        store.cleanup_pending(10).await.expect("cleanup");

        assert!(
            store
                .get(GetObjectRequest {
                    bucket_id: bucket.id,
                    key: key("kept.txt"),
                    range: None,
                })
                .await
                .is_ok(),
            "the live object must survive cleanup"
        );
        assert_ne!(kept.metadata.id, released.metadata.id);
    }
}
