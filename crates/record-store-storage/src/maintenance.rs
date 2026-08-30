//! Streaming object storage boundary and local filesystem implementation.

use std::{
    io,
    path::{Path, PathBuf},
};

use record_store_core::ObjectId;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};
use tracing::warn;
use uuid::Uuid;

use crate::layout::{
    PublicationRecord, STORAGE_FORMAT_VERSION, StorageFormatRecord, StorageLayout,
};
use crate::*;

pub(crate) async fn inspect_consistency(
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

pub(crate) async fn initialize_storage_format(layout: &StorageLayout) -> Result<(), StorageError> {
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

pub(crate) struct TemporaryFileGuard {
    path: PathBuf,
    active: bool,
}

impl TemporaryFileGuard {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }

    pub(crate) fn disarm(&mut self) {
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

pub(crate) async fn cleanup_file(path: &Path) -> bool {
    match fs::remove_file(path).await {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            warn!(error = %error, "failed to clean up storage file");
            false
        }
    }
}

pub(crate) async fn write_publication_record(
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

pub(crate) async fn sync_directory(path: PathBuf) -> Result<(), StorageError> {
    tokio::task::spawn_blocking(move || {
        let directory = std::fs::File::open(path)
            .map_err(|source| filesystem("open payload directory", source))?;
        directory
            .sync_all()
            .map_err(|source| filesystem("synchronize payload directory", source))
    })
    .await?
}

pub(crate) fn filesystem(operation: &'static str, source: io::Error) -> StorageError {
    StorageError::Filesystem { operation, source }
}

pub(crate) fn is_recognized_upload_name(name: &str) -> bool {
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

#[cfg(test)]
mod tests {

    use chrono::Utc;
    use record_store_core::{
        Bucket, BucketId, BucketName, BucketQuota, ObjectId, ObjectKey, OrganizationId,
        VersioningState,
    };
    use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
    use tempfile::tempdir;
    use tokio::fs;

    use super::*;
    use crate::layout::PublicationRecord;

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
            storage_class: None,
            durability_policy: None,
            cors: None,
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

#[cfg(test)]
mod consistency_tests {
    use record_store_core::{Checksum, ObjectId};
    use sha2::{Digest, Sha256};

    use crate::test_support::{put, store};
    use crate::{ObjectStore, upload_stream};
    use crate::{ReadReplicaRequest, ReplicaStore, StorageRepairRequest, WriteReplicaRequest};

    fn commitment(body: &[u8]) -> (u64, Checksum) {
        let digest: [u8; 32] = Sha256::digest(body).into();
        (body.len() as u64, Checksum::sha256(digest))
    }

    fn stream(body: &[u8]) -> crate::UploadStream {
        let owned = bytes::Bytes::copy_from_slice(body);
        upload_stream(futures_util::stream::once(async move { Ok(owned) }))
    }

    /// Inspection is what an operator runs to find storage the catalog no longer
    /// references. On a consistent store it must report nothing, or every run
    /// would propose deleting live data.
    #[tokio::test]
    async fn inspection_of_a_consistent_store_finds_nothing() {
        let (_directory, store, bucket) = store().await;
        put(&store, &bucket, "a.txt", b"hello").await;

        let report = store.inspect(100).await.expect("inspect");
        assert_eq!(report.data_without_metadata, 0, "{report:?}");
        assert_eq!(report.metadata_without_data, 0, "{report:?}");
        assert!(report.orphan_payload_samples.is_empty(), "{report:?}");
        assert!(report.data_payloads_scanned >= 1, "{report:?}");
    }

    /// A payload written as a replica but never committed to the catalog is an
    /// orphan. Finding it is the whole point of inspection, and a dry run must
    /// report it without removing anything.
    #[tokio::test]
    async fn an_uncommitted_replica_is_reported_as_an_orphan() {
        let (_directory, store, _bucket) = store().await;
        let object_id = ObjectId::new();
        let body = b"orphaned bytes";
        let (size, checksum) = commitment(body);

        store
            .write_replica(WriteReplicaRequest::known(
                "op-1",
                object_id,
                size,
                checksum,
                stream(body),
            ))
            .await
            .expect("write replica");

        let report = store.inspect(100).await.expect("inspect");
        assert!(
            report.orphan_payload_samples.contains(&object_id),
            "the uncommitted payload must be reported: {report:?}"
        );

        let dry_run = store
            .repair(StorageRepairRequest {
                maximum_entries: 100,
                dry_run: true,
            })
            .await
            .expect("dry run");
        assert_eq!(dry_run.removed_orphan_payloads, 0, "{dry_run:?}");
        assert!(dry_run.dry_run);

        let applied = store
            .repair(StorageRepairRequest {
                maximum_entries: 100,
                dry_run: false,
            })
            .await
            .expect("repair");
        assert_eq!(
            applied.removed_orphan_payloads, 1,
            "the orphan must be reclaimed: {applied:?}"
        );

        let after = store.inspect(100).await.expect("inspect");
        assert!(after.orphan_payload_samples.is_empty(), "{after:?}");
    }

    /// Repair must never touch a payload the catalog still references, which is
    /// the difference between reclaiming space and destroying data.
    #[tokio::test]
    async fn repair_never_removes_a_referenced_payload() {
        let (_directory, store, bucket) = store().await;
        let committed = put(&store, &bucket, "a.txt", b"live bytes").await;

        store
            .repair(StorageRepairRequest {
                maximum_entries: 100,
                dry_run: false,
            })
            .await
            .expect("repair");

        let read = store
            .read_replica(ReadReplicaRequest {
                object_id: committed.metadata.id,
                size: committed.metadata.size,
                payload_format: committed.metadata.payload_format,
                range: None,
                expected_checksum: Some(committed.metadata.checksum),
            })
            .await;
        assert!(read.is_ok(), "a referenced payload must survive repair");
    }

    /// A replica is only published once its bytes match what the writer
    /// promised, so a mismatched commitment must leave nothing behind.
    #[tokio::test]
    async fn a_replica_whose_bytes_break_the_promise_is_not_published() {
        let (_directory, store, _bucket) = store().await;
        let object_id = ObjectId::new();
        let (size, _) = commitment(b"actual bytes");

        let result = store
            .write_replica(WriteReplicaRequest::known(
                "op-1",
                object_id,
                size,
                Checksum::sha256([0_u8; 32]),
                stream(b"actual bytes"),
            ))
            .await;
        assert!(result.is_err(), "a broken promise must not publish");

        assert!(
            store.stat_replica(object_id).await.expect("stat").is_none(),
            "nothing may remain on disk"
        );
    }

    /// A replica that is written, read back, verified, and deleted is the whole
    /// peer-facing contract; each step has to agree about what is stored.
    #[tokio::test]
    async fn a_replica_round_trips_and_can_be_verified_then_deleted() {
        let (_directory, store, _bucket) = store().await;
        let object_id = ObjectId::new();
        let body = b"replicated bytes";
        let (size, checksum) = commitment(body);

        let written = store
            .write_replica(WriteReplicaRequest::known(
                "op-1",
                object_id,
                size,
                checksum.clone(),
                stream(body),
            ))
            .await
            .expect("write replica");
        assert_eq!(written.size, size);

        let stat = store
            .stat_replica(object_id)
            .await
            .expect("stat")
            .expect("present");
        assert!(stat.physical_bytes >= size, "{stat:?}");

        let verified = store
            .verify_replica(
                object_id,
                size,
                record_store_core::PayloadFormat::Plaintext,
                checksum,
            )
            .await
            .expect("verify");
        assert!(verified.present && verified.matches, "{verified:?}");

        let payloads = store
            .list_local_payloads(None, 10)
            .await
            .expect("list payloads");
        assert!(payloads.contains(&object_id), "{payloads:?}");

        store
            .delete_replica(object_id)
            .await
            .expect("delete replica");
        assert!(store.stat_replica(object_id).await.expect("stat").is_none());
    }

    /// Reading a replica that is not held must report absence rather than
    /// failing in a way a caller would read as a transport error.
    #[tokio::test]
    async fn reading_a_replica_this_node_does_not_hold_reports_absence() {
        let (_directory, store, _bucket) = store().await;
        let object_id = ObjectId::new();
        assert!(store.stat_replica(object_id).await.expect("stat").is_none());
        assert!(
            store
                .read_replica(ReadReplicaRequest {
                    object_id,
                    size: 10,
                    payload_format: record_store_core::PayloadFormat::Plaintext,
                    range: None,
                    expected_checksum: None,
                })
                .await
                .is_err()
        );
    }

    /// Local capacity is what a heartbeat reports, so it has to describe the
    /// real filesystem rather than a placeholder.
    #[tokio::test]
    async fn local_capacity_describes_the_real_filesystem() {
        let (_directory, store, _bucket) = store().await;
        let capacity = store.local_capacity().await.expect("capacity");
        assert!(capacity.capacity_bytes > 0, "{capacity:?}");
        assert!(
            capacity.available_bytes <= capacity.capacity_bytes,
            "{capacity:?}"
        );
    }
}
