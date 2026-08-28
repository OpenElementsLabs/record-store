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
