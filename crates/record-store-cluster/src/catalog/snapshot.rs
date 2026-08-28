use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, Utc};
use redb::{ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle, WriteTransaction};

use crate::topology::NodeRecord;

use crate::catalog::codec::put_raw;
use crate::catalog::*;

/// Exports every cluster table for a consensus snapshot.
///
/// A read transaction is used so that a snapshot is a consistent point-in-time
/// view without blocking concurrent command application.
pub fn export_tx(write: &redb::ReadTransaction) -> CatalogResult<Vec<CatalogEntry>> {
    let mut entries = Vec::new();
    for definition in CLUSTER_TABLES {
        let table = write
            .open_table(*definition)
            .map_err(|error| backend("open cluster table", error))?;
        if table
            .is_empty()
            .map_err(|error| backend("inspect cluster table", error))?
        {
            continue;
        }
        for entry in table
            .iter()
            .map_err(|error| backend("scan cluster table", error))?
        {
            let (key, value) = entry.map_err(|error| backend("read cluster record", error))?;
            entries.push(CatalogEntry {
                table: definition.name().to_owned(),
                key: key.value().to_vec(),
                value: value.value().to_vec(),
            });
        }
    }
    Ok(entries)
}

/// Replaces every cluster table from a consensus snapshot.
pub fn import_tx(write: &WriteTransaction, entries: &[CatalogEntry]) -> CatalogResult<()> {
    let by_name: BTreeMap<&str, TableDefinition<&[u8], &[u8]>> = CLUSTER_TABLES
        .iter()
        .map(|definition| (definition.name(), *definition))
        .collect();
    for definition in CLUSTER_TABLES {
        let mut table = write
            .open_table(*definition)
            .map_err(|error| backend("open cluster table", error))?;
        table
            .retain(|_, _| false)
            .map_err(|error| backend("clear cluster table", error))?;
    }
    for entry in entries {
        let Some(definition) = by_name.get(entry.table.as_str()) else {
            return Err(ClusterCatalogError::Database {
                operation: "import cluster snapshot",
                reason: format!("snapshot references unknown table '{}'", entry.table),
            });
        };
        put_raw(write, *definition, &entry.key, &entry.value)?;
    }
    Ok(())
}

/// Returns how long a node has been silent, for failure detection.
#[must_use]
pub fn silence(node: &NodeRecord, now: DateTime<Utc>) -> TimeDelta {
    now.signed_duration_since(node.last_heartbeat_at.unwrap_or(node.joined_at))
}
