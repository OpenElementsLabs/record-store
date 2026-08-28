//! Streaming object storage boundary and local filesystem implementation.

use std::{
    io,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use record_store_core::{ByteRange, Checksum, ObjectId, PayloadFormat, ResolvedByteRange};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};
use uuid::Uuid;

use crate::layout::PublicationRecord;
use crate::maintenance::filesystem;
use crate::maintenance::{
    TemporaryFileGuard, cleanup_file, sync_directory, write_publication_record,
};
use crate::*;

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
pub(crate) fn verifying_stream(body: DownloadStream, expected: Checksum) -> DownloadStream {
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
