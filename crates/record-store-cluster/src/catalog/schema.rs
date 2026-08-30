use redb::{Database, ReadableTable, TableDefinition};

use crate::version::CLUSTER_FORMAT_VERSION;

use crate::catalog::codec::decode_u32;
use crate::catalog::*;

pub(crate) const IDENTITY: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.identity.v1");
pub(crate) const CONFIG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.config.v1");
pub(crate) const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.nodes.v1");
pub(crate) const NODE_BY_MEMBER: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.node_by_member.v1");
pub(crate) const COUNTERS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.counters.v1");
pub(crate) const PLACEMENTS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.placements.v1");
pub(crate) const NODE_REPLICAS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.node_replicas.v1");
pub(crate) const TOMBSTONES: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.tombstones.v1");
pub(crate) const TASKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.tasks.v1");
pub(crate) const TASK_QUEUE: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.task_queue.v1");
pub(crate) const TASK_BY_OBJECT: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.task_by_object.v1");
pub(crate) const OPERATIONS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.operations.v1");
pub(crate) const JOIN_TOKENS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.join_tokens.v1");
pub(crate) const NODE_CREDENTIALS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.node_credentials.v1");
pub(crate) const STORAGE_POLICIES: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("cluster.storage_policies.v1");
pub(crate) const SCHEMA: TableDefinition<&[u8], &[u8]> = TableDefinition::new("cluster.schema.v1");

/// Every cluster table, used by consensus snapshot export and import.
pub const CLUSTER_TABLES: &[TableDefinition<'static, &'static [u8], &'static [u8]>] = &[
    IDENTITY,
    CONFIG,
    NODES,
    NODE_BY_MEMBER,
    COUNTERS,
    PLACEMENTS,
    NODE_REPLICAS,
    TOMBSTONES,
    TASKS,
    TASK_QUEUE,
    TASK_BY_OBJECT,
    OPERATIONS,
    JOIN_TOKENS,
    NODE_CREDENTIALS,
    STORAGE_POLICIES,
    SCHEMA,
];

pub(crate) const SINGLETON: &[u8] = b"singleton";
pub(crate) const SCHEMA_VERSION_KEY: &[u8] = b"cluster_format_version";
pub(crate) const NEXT_MEMBER_ID: &[u8] = b"next_member_id";
pub(crate) const PLACEMENT_COUNT: &[u8] = b"placements";
pub(crate) const LOGICAL_BYTES: &[u8] = b"logical_bytes";
pub(crate) const PHYSICAL_BYTES: &[u8] = b"physical_bytes";
pub(crate) const TOMBSTONE_COUNT: &[u8] = b"tombstones";
pub(crate) const ACTIVE_TASKS: &[u8] = b"active_tasks";
pub(crate) const PARKED_TASKS: &[u8] = b"parked_tasks";
pub(crate) const UNDER_REPLICATED: &[u8] = b"under_replicated";
pub(crate) const UNAVAILABLE_PAYLOADS: &[u8] = b"unavailable_payloads";
pub(crate) const CLUSTER_MAP_EPOCH: &[u8] = b"cluster_map_epoch";

/// Creates every cluster table so that read transactions never fail on a fresh
/// database, and records the durable layout version.
pub fn initialize_tables(database: &Database) -> CatalogResult<()> {
    let write = database
        .begin_write()
        .map_err(|error| backend("begin cluster schema", error))?;
    for table in CLUSTER_TABLES {
        write
            .open_table(*table)
            .map_err(|error| backend("open cluster table", error))?;
    }
    {
        let mut schema = write
            .open_table(SCHEMA)
            .map_err(|error| backend("open cluster schema", error))?;
        let recorded = schema
            .get(SCHEMA_VERSION_KEY)
            .map_err(|error| backend("read cluster schema", error))?
            .map(|value| value.value().to_vec());
        match recorded {
            Some(encoded) => {
                let found = decode_u32(&encoded)?;
                if found > CLUSTER_FORMAT_VERSION {
                    return Err(ClusterCatalogError::IncompatibleFormat {
                        found,
                        expected: CLUSTER_FORMAT_VERSION,
                    });
                }
                if found < CLUSTER_FORMAT_VERSION {
                    drop(schema);
                    migrate_to_v2(&write, found)?;
                    let mut schema = write
                        .open_table(SCHEMA)
                        .map_err(|error| backend("open cluster schema", error))?;
                    schema
                        .insert(
                            SCHEMA_VERSION_KEY,
                            CLUSTER_FORMAT_VERSION.to_be_bytes().as_slice(),
                        )
                        .map_err(|error| backend("write cluster schema", error))?;
                }
            }
            None => {
                schema
                    .insert(
                        SCHEMA_VERSION_KEY,
                        CLUSTER_FORMAT_VERSION.to_be_bytes().as_slice(),
                    )
                    .map_err(|error| backend("write cluster schema", error))?;
            }
        }
    }
    write
        .commit()
        .map_err(|error| backend("commit cluster schema", error))
}

fn migrate_to_v2(write: &redb::WriteTransaction, found: u32) -> CatalogResult<()> {
    if found != 1 {
        return Err(ClusterCatalogError::IncompatibleFormat {
            found,
            expected: CLUSTER_FORMAT_VERSION,
        });
    }

    // V2 adds explicit devices and placement epochs. Decode through the
    // backward-compatible wire models, then persist the fully explicit V2
    // representation so the migration is one-time and auditable.
    let nodes = {
        let table = write
            .open_table(NODES)
            .map_err(|error| backend("open nodes for v2 migration", error))?;
        let mut records = Vec::new();
        for entry in table
            .iter()
            .map_err(|error| backend("scan nodes for v2 migration", error))?
        {
            let (key, value) = entry.map_err(|error| backend("read v1 node", error))?;
            let mut node: crate::topology::NodeRecord = serde_json::from_slice(value.value())?;
            node.ensure_legacy_device();
            records.push((key.value().to_vec(), node));
        }
        records
    };
    for (key, node) in nodes {
        crate::catalog::codec::put(write, NODES, &key, &node)?;
    }

    let placements = {
        let table = write
            .open_table(PLACEMENTS)
            .map_err(|error| backend("open placements for v2 migration", error))?;
        let mut records = Vec::new();
        for entry in table
            .iter()
            .map_err(|error| backend("scan placements for v2 migration", error))?
        {
            let (key, value) = entry.map_err(|error| backend("read v1 placement", error))?;
            let placement: crate::replica::PayloadPlacement =
                serde_json::from_slice(value.value())?;
            records.push((key.value().to_vec(), placement));
        }
        records
    };
    {
        let mut index = write
            .open_table(NODE_REPLICAS)
            .map_err(|error| backend("open replica index for v2 migration", error))?;
        index
            .retain(|_, _| false)
            .map_err(|error| backend("clear v1 replica index", error))?;
    }
    for (key, placement) in placements {
        crate::catalog::codec::put(write, PLACEMENTS, &key, &placement)?;
        for replica in &placement.replicas {
            crate::catalog::codec::put_raw(
                write,
                NODE_REPLICAS,
                &crate::catalog::keys::node_replica_key(
                    replica.node_id,
                    placement.object_id,
                    replica.device_id,
                ),
                &[crate::catalog::keys::replica_state_code(replica.state)],
            )?;
        }
    }
    crate::catalog::codec::set_counter(write, CLUSTER_MAP_EPOCH, 1)?;
    Ok(())
}
