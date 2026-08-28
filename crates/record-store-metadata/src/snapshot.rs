//! Durable single-node metadata catalog.

use redb::{ReadableTable, TableDefinition, TableHandle};
use serde::{Deserialize, Serialize};

use crate::error::backend;
use crate::schema::{
    BUCKET_NAMES, BUCKET_USAGE, BUCKETS, CLEANUP, COUNTERS, LIFECYCLE_RULES, MARKERS, MULTIPART,
    MULTIPART_ORDER, NULL_VERSIONS, OBJECTS, PARTS, SCHEMA, VERSION_ORDER, VERSIONS,
};
use crate::*;

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

pub(crate) const BYTE_TABLES: &[TableDefinition<'static, &'static [u8], &'static [u8]>] = &[
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
    use tempfile::tempdir;

    use super::*;
    use crate::test_support::*;
    use crate::{MetadataRepository, RedbMetadataRepository};

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

    use crate::commands::NewDeleteMarker;
    use record_store_core::{BucketName, ObjectKey, VersioningState};

    /// A snapshot is how a lagging consensus member catches up without replaying
    /// the whole log, so an export must carry everything an import needs to
    /// reproduce the catalog exactly.
    #[tokio::test]
    async fn a_snapshot_reproduces_the_catalog_it_was_taken_from() {
        let (source_directory, source, source_bucket) = catalog_with_bucket("snapshotted").await;
        source
            .set_bucket_versioning(source_bucket.id, VersioningState::Enabled)
            .await
            .expect("enable versioning");
        source
            .put_object(&object(source_bucket.id, "a.txt", 10))
            .await
            .expect("put");
        source
            .put_object(&object(source_bucket.id, "b.txt", 20))
            .await
            .expect("put");
        let upload_record = upload(source_bucket.id, "big.bin");
        source
            .create_multipart_upload(&upload_record)
            .await
            .expect("create upload");
        source
            .put_multipart_part(&part(upload_record.id, 1, 64))
            .await
            .expect("put part");

        let expected_objects = source.storage_usage().await.expect("usage").object_count;
        drop(source);

        let entries = {
            let database = redb::Database::open(source_directory.path().join("metadata.redb"))
                .expect("open source");
            let read = database.begin_read().expect("read transaction");
            export_tx(&read).expect("export")
        };
        assert!(!entries.is_empty());

        let target_directory = tempfile::tempdir().expect("temporary directory");
        let target_path = target_directory.path().join("metadata.redb");
        RedbMetadataRepository::open(&target_path)
            .await
            .expect("initialise target");
        {
            let database = redb::Database::open(&target_path).expect("open target");
            let write = database.begin_write().expect("write transaction");
            import_tx(&write, &entries).expect("import");
            write.commit().expect("commit");
        }

        let target = RedbMetadataRepository::open(&target_path)
            .await
            .expect("reopen target");
        assert_eq!(
            target
                .get_bucket(source_bucket.id)
                .await
                .expect("read")
                .expect("bucket")
                .versioning,
            VersioningState::Enabled
        );
        assert!(
            target
                .get_object(source_bucket.id, &ObjectKey::new("a.txt").expect("key"))
                .await
                .expect("read")
                .is_some()
        );
        assert!(
            target
                .get_multipart_upload(upload_record.id)
                .await
                .expect("read")
                .is_some(),
            "in-flight uploads have to survive a snapshot"
        );
        assert_eq!(
            target.storage_usage().await.expect("usage").object_count,
            expected_objects,
            "the counters have to match too"
        );
    }

    /// Importing replaces whatever the target held. A snapshot that merged into
    /// existing state would leave a catching-up member with data the leader
    /// already deleted.
    #[tokio::test]
    async fn an_import_replaces_the_targets_existing_state() {
        let (source_directory, source, source_bucket) = catalog_with_bucket("kept").await;
        source
            .put_object(&object(source_bucket.id, "kept.txt", 1))
            .await
            .expect("put");
        drop(source);
        let entries = {
            let database = redb::Database::open(source_directory.path().join("metadata.redb"))
                .expect("open source");
            let read = database.begin_read().expect("read transaction");
            export_tx(&read).expect("export")
        };

        let (target_directory, target, stale_bucket) = catalog_with_bucket("stale").await;
        target
            .put_object(&object(stale_bucket.id, "stale.txt", 1))
            .await
            .expect("put");
        drop(target);

        let target_path = target_directory.path().join("metadata.redb");
        {
            let database = redb::Database::open(&target_path).expect("open target");
            let write = database.begin_write().expect("write transaction");
            import_tx(&write, &entries).expect("import");
            write.commit().expect("commit");
        }

        let target = RedbMetadataRepository::open(&target_path)
            .await
            .expect("reopen");
        assert!(
            target
                .get_bucket_by_name(&BucketName::new("stale").expect("name"))
                .await
                .expect("read")
                .is_none(),
            "state the snapshot does not describe must be gone"
        );
        assert!(
            target
                .get_bucket_by_name(&BucketName::new("kept").expect("name"))
                .await
                .expect("read")
                .is_some()
        );
    }

    /// A snapshot naming a table this build does not have is a version mismatch,
    /// and importing it blindly would corrupt the catalog.
    #[tokio::test]
    async fn a_snapshot_referencing_an_unknown_table_is_refused() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("metadata.redb");
        RedbMetadataRepository::open(&path).await.expect("open");

        let database = redb::Database::open(&path).expect("open");
        let write = database.begin_write().expect("write transaction");
        let result = import_tx(
            &write,
            &[MetadataEntry {
                table: "not_a_table.v9".to_owned(),
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }],
        );
        assert!(
            matches!(result, Err(MetadataError::Database { .. })),
            "{result:?}"
        );
    }

    /// An empty snapshot is a legitimate state — a cluster that has committed
    /// nothing yet — and importing it must leave an empty catalog, not fail.
    #[tokio::test]
    async fn an_empty_snapshot_imports_into_an_empty_catalog() {
        let (directory, catalog, existing) = catalog_with_bucket("wiped").await;
        catalog
            .delete_object(
                existing.id,
                &ObjectKey::new("absent").expect("key"),
                NewDeleteMarker::generate(),
            )
            .await
            .ok();
        drop(catalog);

        let path = directory.path().join("metadata.redb");
        {
            let database = redb::Database::open(&path).expect("open");
            let write = database.begin_write().expect("write transaction");
            import_tx(&write, &[]).expect("import an empty snapshot");
            write.commit().expect("commit");
        }

        let catalog = RedbMetadataRepository::open(&path).await.expect("reopen");
        assert!(catalog.list_buckets().await.expect("list").is_empty());
        let _ = bucket("unused");
    }
}
