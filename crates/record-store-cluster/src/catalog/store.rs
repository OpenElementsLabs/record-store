use std::{path::Path, sync::Arc};

use chrono::{DateTime, Utc};
use record_store_core::{
    ClusterOperationId, JoinTokenId, NodeCredentialId, NodeId, ObjectId, ReplicaTaskId,
};
use redb::{Database, ReadableTable, WriteTransaction};

use crate::{
    command::{ClusterCommand, ClusterIdentity, ClusterOutcome},
    config::ClusterConfig,
    credentials::{JoinToken, NodeCredential},
    identity::RaftNodeId,
    replica::{PayloadPlacement, Tombstone},
    tasks::{ClusterOperation, ReplicaTask},
    topology::{ClusterTopology, NodeRecord},
};

use crate::catalog::codec::{get, get_raw, read_counter, read_nodes};
use crate::catalog::commands::{node_replica_count, recount_durability};
use crate::catalog::keys::{node_replica_key, prefix_successor};
use crate::catalog::schema::{
    ACTIVE_TASKS, CONFIG, IDENTITY, JOIN_TOKENS, LOGICAL_BYTES, NODE_BY_MEMBER, NODE_CREDENTIALS,
    NODE_REPLICAS, NODES, OPERATIONS, PARKED_TASKS, PHYSICAL_BYTES, PLACEMENT_COUNT, PLACEMENTS,
    SINGLETON, TASK_QUEUE, TASKS, TOMBSTONE_COUNT, TOMBSTONES, UNAVAILABLE_PAYLOADS,
    UNDER_REPLICATED,
};
use crate::catalog::*;

/// Aggregated cluster-wide storage accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterUsage {
    /// Payloads with placement metadata.
    pub payloads: u64,
    /// Logical bytes stored once, ignoring replication.
    pub logical_bytes: u64,
    /// Physical bytes across all replicas.
    pub physical_bytes: u64,
    /// Outstanding tombstones.
    pub tombstones: u64,
    /// Active replica movement tasks.
    pub active_tasks: u64,
    /// Tasks parked after exhausting their retries.
    pub parked_tasks: u64,
    /// Payloads below their desired replica count.
    pub under_replicated_payloads: u64,
    /// Payloads with no healthy replica.
    pub unavailable_payloads: u64,
}

/// Bounded page of replica movement tasks in priority order.
#[derive(Debug, Clone, Default)]
pub struct TaskPage {
    /// Tasks in priority order.
    pub tasks: Vec<ReplicaTask>,
    /// Whether more tasks matched than were returned.
    pub truncated: bool,
}

/// Bounded page of payload placements.
#[derive(Debug, Clone, Default)]
pub struct PlacementPage {
    /// Placements in payload-identifier order.
    pub placements: Vec<PayloadPlacement>,
    /// Continuation cursor.
    pub next_object_id: Option<ObjectId>,
}

/// Durable cluster catalog handle.
#[derive(Clone)]
pub struct ClusterCatalog {
    database: Arc<Database>,
}

impl ClusterCatalog {
    /// Opens a catalog in its own database file.
    pub async fn open(path: impl AsRef<Path>) -> CatalogResult<Self> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| backend("create directory", error))?;
            }
            let database =
                Database::create(path).map_err(|error| backend("open catalog", error))?;
            initialize_tables(&database)?;
            Ok(Self {
                database: Arc::new(database),
            })
        })
        .await?
    }

    /// Opens a catalog that shares a database with other Record Store state.
    ///
    /// Sharing one database is what lets a consensus state machine commit
    /// object metadata, cluster metadata, and the applied log position together.
    pub fn from_database(database: Arc<Database>) -> CatalogResult<Self> {
        initialize_tables(&database)?;
        Ok(Self { database })
    }

    /// Returns the shared database handle.
    #[must_use]
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.database)
    }

    /// Applies one command in its own transaction.
    ///
    /// Cluster mode routes commands through consensus instead; this entry point
    /// serves standalone deployments and tests.
    pub async fn apply(&self, command: ClusterCommand) -> CatalogResult<ClusterOutcome> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin cluster command", error))?;
            let outcome = apply_command_tx(&write, command)?;
            write
                .commit()
                .map_err(|error| backend("commit cluster command", error))?;
            Ok(outcome)
        })
        .await?
    }

    /// Returns the cluster identity, when the cluster has been initialized.
    pub async fn identity(&self) -> CatalogResult<Option<ClusterIdentity>> {
        self.read(|write| get(write, IDENTITY, SINGLETON)).await
    }

    /// Returns the cluster-wide configuration.
    pub async fn config(&self) -> CatalogResult<Option<ClusterConfig>> {
        self.read(|write| get(write, CONFIG, SINGLETON)).await
    }

    /// Returns one node record.
    pub async fn node(&self, node_id: NodeId) -> CatalogResult<Option<NodeRecord>> {
        self.read(move |write| get(write, NODES, node_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns the node owning a consensus member identifier.
    pub async fn node_by_member(&self, raft_id: RaftNodeId) -> CatalogResult<Option<NodeRecord>> {
        self.read(move |write| {
            let Some(encoded) = get_raw(write, NODE_BY_MEMBER, &raft_id.to_be_bytes())? else {
                return Ok(None);
            };
            get(write, NODES, &encoded)
        })
        .await
    }

    /// Returns every node record ordered by node identifier.
    pub async fn nodes(&self) -> CatalogResult<Vec<NodeRecord>> {
        self.read(read_nodes).await.map(|mut nodes| {
            nodes.sort_by_key(|node| node.node_id);
            nodes
        })
    }

    /// Returns a topology view suitable for placement decisions.
    pub async fn topology(&self) -> CatalogResult<ClusterTopology> {
        let (identity, config, nodes) =
            tokio::try_join!(self.identity(), self.config(), self.nodes())?;
        let identity = identity.ok_or(ClusterCatalogError::NotInitialized)?;
        let config = config.ok_or(ClusterCatalogError::NotInitialized)?;
        Ok(ClusterTopology::new(identity.cluster_id, config, nodes))
    }

    /// Returns placement metadata for one payload.
    pub async fn placement(&self, object_id: ObjectId) -> CatalogResult<Option<PayloadPlacement>> {
        self.read(move |write| get(write, PLACEMENTS, object_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns a bounded page of placements ordered by payload identifier.
    pub async fn list_placements(
        &self,
        after: Option<ObjectId>,
        limit: usize,
    ) -> CatalogResult<PlacementPage> {
        let limit = limit.clamp(1, 10_000);
        self.read(move |write| {
            let table = write
                .open_table(PLACEMENTS)
                .map_err(|error| backend("open placements", error))?;
            let start = after.map_or_else(Vec::new, |id| {
                let mut key = id.as_uuid().as_bytes().to_vec();
                key.push(0);
                key
            });
            let mut placements = Vec::new();
            for entry in table
                .range(start.as_slice()..)
                .map_err(|error| backend("range placements", error))?
                .take(limit + 1)
            {
                let (_, value) = entry.map_err(|error| backend("read placement", error))?;
                placements.push(serde_json::from_slice::<PayloadPlacement>(value.value())?);
            }
            let next_object_id = if placements.len() > limit {
                placements.pop();
                placements.last().map(|placement| placement.object_id)
            } else {
                None
            };
            Ok(PlacementPage {
                placements,
                next_object_id,
            })
        })
        .await
    }

    /// Returns the payload identifiers a node is recorded as holding.
    pub async fn node_replicas(
        &self,
        node_id: NodeId,
        after: Option<ObjectId>,
        limit: usize,
    ) -> CatalogResult<Vec<ObjectId>> {
        let limit = limit.clamp(1, 100_000);
        self.read(move |write| {
            let table = write
                .open_table(NODE_REPLICAS)
                .map_err(|error| backend("open node replicas", error))?;
            let prefix = node_id.as_uuid().as_bytes().to_vec();
            let mut start =
                after.map_or_else(|| prefix.clone(), |id| node_replica_key(node_id, id));
            if after.is_some() {
                start.push(0);
            }
            let end = prefix_successor(&prefix);
            let mut out = Vec::new();
            for entry in table
                .range(start.as_slice()..end.as_slice())
                .map_err(|error| backend("range node replicas", error))?
                .take(limit)
            {
                let (key, _) = entry.map_err(|error| backend("read node replica", error))?;
                let raw = key.value();
                if raw.len() != 32 {
                    continue;
                }
                let bytes: [u8; 16] =
                    raw[16..32]
                        .try_into()
                        .map_err(|_| ClusterCatalogError::Database {
                            operation: "decode node replica",
                            reason: "payload identifier is malformed".into(),
                        })?;
                out.push(ObjectId::from_uuid(uuid::Uuid::from_bytes(bytes)));
            }
            Ok(out)
        })
        .await
    }

    /// Returns the number of replica records a node holds.
    pub async fn node_replica_count(&self, node_id: NodeId) -> CatalogResult<u64> {
        self.read(move |write| node_replica_count(write, node_id))
            .await
    }

    /// Returns the tombstone for a payload, if one exists.
    pub async fn tombstone(&self, object_id: ObjectId) -> CatalogResult<Option<Tombstone>> {
        self.read(move |write| get(write, TOMBSTONES, object_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns tombstones that still have outstanding nodes.
    pub async fn pending_tombstones(&self, limit: usize) -> CatalogResult<Vec<Tombstone>> {
        let limit = limit.clamp(1, 10_000);
        self.read(move |write| {
            let table = write
                .open_table(TOMBSTONES)
                .map_err(|error| backend("open tombstones", error))?;
            let mut out = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("scan tombstones", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read tombstone", error))?;
                let tombstone: Tombstone = serde_json::from_slice(value.value())?;
                if !tombstone.completed() {
                    out.push(tombstone);
                }
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    /// Returns tombstones that may be purged under the retention policy.
    pub async fn purgeable_tombstones(
        &self,
        retention_hours: u32,
        now: DateTime<Utc>,
        limit: usize,
    ) -> CatalogResult<Vec<ObjectId>> {
        let limit = limit.clamp(1, 10_000);
        self.read(move |write| {
            let table = write
                .open_table(TOMBSTONES)
                .map_err(|error| backend("open tombstones", error))?;
            let mut out = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("scan tombstones", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read tombstone", error))?;
                let tombstone: Tombstone = serde_json::from_slice(value.value())?;
                if tombstone.purgeable(retention_hours, now) {
                    out.push(tombstone.object_id);
                }
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
        .await
    }

    /// Returns one task by identifier.
    pub async fn task(&self, task_id: ReplicaTaskId) -> CatalogResult<Option<ReplicaTask>> {
        self.read(move |write| get(write, TASKS, task_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns active tasks in priority order, most urgent first.
    pub async fn queued_tasks(&self, limit: usize) -> CatalogResult<TaskPage> {
        let limit = limit.clamp(1, 10_000);
        self.read(move |write| {
            let queue = write
                .open_table(TASK_QUEUE)
                .map_err(|error| backend("open task queue", error))?;
            let mut tasks = Vec::new();
            let mut truncated = false;
            for entry in queue
                .iter()
                .map_err(|error| backend("scan task queue", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read task queue", error))?;
                if tasks.len() >= limit {
                    truncated = true;
                    break;
                }
                if let Some(task) = get::<ReplicaTask>(write, TASKS, value.value())? {
                    tasks.push(task);
                }
            }
            Ok(TaskPage { tasks, truncated })
        })
        .await
    }

    /// Returns one long-running operation.
    pub async fn operation(
        &self,
        operation_id: ClusterOperationId,
    ) -> CatalogResult<Option<ClusterOperation>> {
        self.read(move |write| get(write, OPERATIONS, operation_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns every recorded operation, newest first.
    pub async fn operations(&self, limit: usize) -> CatalogResult<Vec<ClusterOperation>> {
        let limit = limit.clamp(1, 1_000);
        self.read(move |write| {
            let table = write
                .open_table(OPERATIONS)
                .map_err(|error| backend("open operations", error))?;
            let mut out = Vec::new();
            for entry in table
                .iter()
                .map_err(|error| backend("scan operations", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read operation", error))?;
                out.push(serde_json::from_slice::<ClusterOperation>(value.value())?);
            }
            out.sort_by_key(|operation| std::cmp::Reverse(operation.started_at));
            out.truncate(limit);
            Ok(out)
        })
        .await
    }

    /// Returns a join token record.
    pub async fn join_token(&self, token_id: JoinTokenId) -> CatalogResult<Option<JoinToken>> {
        self.read(move |write| get(write, JOIN_TOKENS, token_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns a node credential record.
    pub async fn node_credential(&self, node_id: NodeId) -> CatalogResult<Option<NodeCredential>> {
        self.read(move |write| get(write, NODE_CREDENTIALS, node_id.as_uuid().as_bytes()))
            .await
    }

    /// Returns the node credential registered under a credential identifier.
    pub async fn node_credential_by_id(
        &self,
        credential_id: NodeCredentialId,
    ) -> CatalogResult<Option<NodeCredential>> {
        self.read(move |write| {
            let table = write
                .open_table(NODE_CREDENTIALS)
                .map_err(|error| backend("open node credentials", error))?;
            for entry in table
                .iter()
                .map_err(|error| backend("scan node credentials", error))?
            {
                let (_, value) = entry.map_err(|error| backend("read node credential", error))?;
                let credential: NodeCredential = serde_json::from_slice(value.value())?;
                if credential.id == credential_id {
                    return Ok(Some(credential));
                }
            }
            Ok(None)
        })
        .await
    }

    /// Returns aggregated cluster-wide accounting.
    pub async fn usage(&self) -> CatalogResult<ClusterUsage> {
        self.read(|write| {
            Ok(ClusterUsage {
                payloads: read_counter(write, PLACEMENT_COUNT)?,
                logical_bytes: read_counter(write, LOGICAL_BYTES)?,
                physical_bytes: read_counter(write, PHYSICAL_BYTES)?,
                tombstones: read_counter(write, TOMBSTONE_COUNT)?,
                active_tasks: read_counter(write, ACTIVE_TASKS)?,
                parked_tasks: read_counter(write, PARKED_TASKS)?,
                under_replicated_payloads: read_counter(write, UNDER_REPLICATED)?,
                unavailable_payloads: read_counter(write, UNAVAILABLE_PAYLOADS)?,
            })
        })
        .await
    }

    /// Recomputes the summary durability counters from placement records.
    pub async fn refresh_durability_counters(&self) -> CatalogResult<()> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin durability recount", error))?;
            recount_durability(&write)?;
            write
                .commit()
                .map_err(|error| backend("commit durability recount", error))
        })
        .await?
    }

    /// Verifies that the catalog is writable.
    pub async fn check_ready(&self) -> CatalogResult<()> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("cluster readiness", error))?;
            {
                write
                    .open_table(NODES)
                    .map_err(|error| backend("cluster readiness table", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit cluster readiness", error))
        })
        .await?
    }

    async fn read<T, F>(&self, operation: F) -> CatalogResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&WriteTransaction) -> CatalogResult<T> + Send + 'static,
    {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            // A write transaction is used for reads so that helper functions can
            // be shared with the command path without duplicating them.
            let write = database
                .begin_write()
                .map_err(|error| backend("begin cluster read", error))?;
            let value = operation(&write)?;
            write
                .commit()
                .map_err(|error| backend("commit cluster read", error))?;
            Ok(value)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {

    use chrono::Utc;
    use record_store_core::ObjectId;

    use crate::catalog::test_support::*;
    use crate::command::ClusterCommand;
    use crate::tasks::{ReplicaTask, ReplicaTaskKind, ReplicaTaskPriority};

    #[tokio::test]
    async fn queued_tasks_are_returned_in_risk_order() {
        let (_directory, catalog) = initialized().await;
        let now = Utc::now();
        for (kind, priority) in [
            (ReplicaTaskKind::Rebalance, ReplicaTaskPriority::Low),
            (ReplicaTaskKind::Repair, ReplicaTaskPriority::Unavailable),
            (ReplicaTaskKind::Drain, ReplicaTaskPriority::Normal),
        ] {
            catalog
                .apply(ClusterCommand::EnqueueTask {
                    task: Box::new(ReplicaTask::queued(
                        ObjectId::new(),
                        kind,
                        priority,
                        10,
                        now,
                    )),
                })
                .await
                .expect("enqueue");
        }
        let page = catalog.queued_tasks(10).await.expect("queued tasks");
        let priorities: Vec<_> = page.tasks.iter().map(|task| task.priority).collect();
        assert_eq!(
            priorities,
            vec![
                ReplicaTaskPriority::Unavailable,
                ReplicaTaskPriority::Normal,
                ReplicaTaskPriority::Low
            ]
        );
    }
}
