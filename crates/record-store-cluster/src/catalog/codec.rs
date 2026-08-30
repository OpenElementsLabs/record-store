use record_store_core::{NodeId, ObjectId, ReplicaTaskId};
use redb::{ReadableTable, TableDefinition, WriteTransaction};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    config::ClusterConfig, identity::RaftNodeId, replica::PayloadPlacement, tasks::ReplicaTask,
    topology::NodeRecord,
};

use crate::catalog::schema::{
    CONFIG, COUNTERS, NEXT_MEMBER_ID, NODES, PLACEMENTS, SINGLETON, STORAGE_POLICIES, TASKS,
};
use crate::catalog::*;

pub(crate) fn put<T: Serialize>(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    value: &T,
) -> CatalogResult<()> {
    let encoded = serde_json::to_vec(value)?;
    put_raw(write, definition, key, &encoded)
}

pub(crate) fn put_raw(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    value: &[u8],
) -> CatalogResult<()> {
    let mut table = write
        .open_table(definition)
        .map_err(|error| backend("open cluster table", error))?;
    table
        .insert(key, value)
        .map_err(|error| backend("write cluster record", error))?;
    Ok(())
}

pub(crate) fn get<T: DeserializeOwned>(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
) -> CatalogResult<Option<T>> {
    match get_raw(write, definition, key)? {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        None => Ok(None),
    }
}

pub(crate) fn get_raw(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
) -> CatalogResult<Option<Vec<u8>>> {
    let table = write
        .open_table(definition)
        .map_err(|error| backend("open cluster table", error))?;
    Ok(table
        .get(key)
        .map_err(|error| backend("read cluster record", error))?
        .map(|value| value.value().to_vec()))
}

pub(crate) fn remove(
    write: &WriteTransaction,
    definition: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
) -> CatalogResult<bool> {
    let mut table = write
        .open_table(definition)
        .map_err(|error| backend("open cluster table", error))?;
    Ok(table
        .remove(key)
        .map_err(|error| backend("remove cluster record", error))?
        .is_some())
}

pub(crate) fn require_node(write: &WriteTransaction, node_id: NodeId) -> CatalogResult<NodeRecord> {
    get(write, NODES, node_id.as_uuid().as_bytes())?
        .ok_or(ClusterCatalogError::NodeNotFound(node_id))
}

pub(crate) fn require_placement(
    write: &WriteTransaction,
    object_id: ObjectId,
) -> CatalogResult<PayloadPlacement> {
    get(write, PLACEMENTS, object_id.as_uuid().as_bytes())?
        .ok_or(ClusterCatalogError::PlacementNotFound(object_id))
}

pub(crate) fn require_task(
    write: &WriteTransaction,
    task_id: ReplicaTaskId,
) -> CatalogResult<ReplicaTask> {
    get(write, TASKS, task_id.as_uuid().as_bytes())?
        .ok_or(ClusterCatalogError::TaskNotFound(task_id))
}

pub(crate) fn require_config(write: &WriteTransaction) -> CatalogResult<ClusterConfig> {
    get(write, CONFIG, SINGLETON)?.ok_or(ClusterCatalogError::NotInitialized)
}

pub(crate) fn read_nodes(write: &WriteTransaction) -> CatalogResult<Vec<NodeRecord>> {
    let table = write
        .open_table(NODES)
        .map_err(|error| backend("open nodes", error))?;
    let mut nodes = Vec::new();
    for entry in table.iter().map_err(|error| backend("scan nodes", error))? {
        let (_, value) = entry.map_err(|error| backend("read node", error))?;
        nodes.push(serde_json::from_slice(value.value())?);
    }
    Ok(nodes)
}

pub(crate) fn read_storage_policies(
    write: &WriteTransaction,
) -> CatalogResult<Vec<crate::policy::StoragePolicy>> {
    let table = write
        .open_table(STORAGE_POLICIES)
        .map_err(|error| backend("open storage policies", error))?;
    let mut policies = Vec::new();
    for entry in table
        .iter()
        .map_err(|error| backend("scan storage policies", error))?
    {
        let (_, value) = entry.map_err(|error| backend("read storage policy", error))?;
        policies.push(serde_json::from_slice(value.value())?);
    }
    Ok(policies)
}

pub(crate) fn count_voters(write: &WriteTransaction) -> CatalogResult<u32> {
    Ok(u32::try_from(
        read_nodes(write)?
            .iter()
            .filter(|node| node.metadata_voter && !node.state.is_terminal())
            .count(),
    )
    .unwrap_or(u32::MAX))
}

pub(crate) fn next_member_id(write: &WriteTransaction) -> CatalogResult<RaftNodeId> {
    let current = read_counter(write, NEXT_MEMBER_ID)?.max(1);
    set_counter(write, NEXT_MEMBER_ID, current.saturating_add(1))?;
    Ok(current)
}

pub(crate) fn read_counter(write: &WriteTransaction, key: &[u8]) -> CatalogResult<u64> {
    match get_raw(write, COUNTERS, key)? {
        Some(bytes) => decode_u64(&bytes),
        None => Ok(0),
    }
}

pub(crate) fn set_counter(write: &WriteTransaction, key: &[u8], value: u64) -> CatalogResult<()> {
    put_raw(write, COUNTERS, key, value.to_be_bytes().as_slice())
}

pub(crate) fn adjust_counter(
    write: &WriteTransaction,
    key: &[u8],
    delta: i128,
) -> CatalogResult<()> {
    let current = i128::from(read_counter(write, key)?);
    let next = u64::try_from(current.saturating_add(delta).max(0)).unwrap_or(u64::MAX);
    set_counter(write, key, next)
}

pub(crate) fn decode_u64(bytes: &[u8]) -> CatalogResult<u64> {
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| ClusterCatalogError::Database {
            operation: "decode counter",
            reason: "counter value is not eight bytes".into(),
        })?;
    Ok(u64::from_be_bytes(array))
}

pub(crate) fn decode_u32(bytes: &[u8]) -> CatalogResult<u32> {
    let array: [u8; 4] = bytes
        .try_into()
        .map_err(|_| ClusterCatalogError::Database {
            operation: "decode version",
            reason: "version value is not four bytes".into(),
        })?;
    Ok(u32::from_be_bytes(array))
}
