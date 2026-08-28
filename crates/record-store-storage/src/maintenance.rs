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
