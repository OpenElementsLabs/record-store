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
