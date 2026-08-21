//! Supervised cluster background services.
//!
//! Replication, repair, rebalancing, heartbeats, and reconciliation are all
//! long-lived tasks. None of them is spawned and forgotten: each is registered,
//! its liveness is tracked, and a failure is reflected in the node's readiness
//! instead of being silently lost.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use oes_cluster::{ClusterCommand, NodeActivity, NodeCapacity, NodeState, Readiness, ReplicaState};
use oes_consensus::{ClusterWrite, MetadataConsensus};
use oes_core::PayloadFormat;
use oes_storage::ObjectStore;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span, warn};

use crate::{
    context::ClusterContext,
    coordinator::{Coordinator, CoordinatorSettings},
    tasks::{MovementLimits, TaskExecutor},
};

/// Liveness of one supervised task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskStatus {
    /// The task is running normally.
    Running {
        /// Last time the task completed a pass.
        last_pass_at: Option<DateTime<Utc>>,
    },
    /// The task stopped because shutdown was requested.
    Stopped,
    /// The task stopped unexpectedly.
    ///
    /// A node in this state is degraded: it is still serving what it can, but an
    /// operator needs to know that part of the cluster machinery is not running.
    Failed {
        /// Why the task stopped.
        reason: String,
        /// When it stopped.
        at: DateTime<Utc>,
    },
}

impl TaskStatus {
    /// Returns whether the task is healthy.
    #[must_use]
    pub const fn healthy(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// Shared liveness table for supervised tasks.
#[derive(Debug, Default)]
pub struct TaskHealth {
    tasks: Mutex<BTreeMap<String, TaskStatus>>,
}

impl TaskHealth {
    /// Records that a task started.
    pub fn started(&self, name: &str) {
        self.set(name, TaskStatus::Running { last_pass_at: None });
    }

    /// Records a completed pass.
    pub fn pass(&self, name: &str) {
        self.set(
            name,
            TaskStatus::Running {
                last_pass_at: Some(Utc::now()),
            },
        );
    }

    /// Records that a task stopped for shutdown.
    pub fn stopped(&self, name: &str) {
        self.set(name, TaskStatus::Stopped);
    }

    /// Records that a task stopped unexpectedly.
    pub fn failed(&self, name: &str, reason: String) {
        error!(task = name, %reason, "supervised cluster task stopped unexpectedly");
        self.set(
            name,
            TaskStatus::Failed {
                reason,
                at: Utc::now(),
            },
        );
    }

    /// Returns the current liveness table.
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<String, TaskStatus> {
        self.tasks
            .lock()
            .map(|tasks| tasks.clone())
            .unwrap_or_default()
    }

    /// Returns the names of tasks that stopped unexpectedly.
    #[must_use]
    pub fn failures(&self) -> Vec<String> {
        self.snapshot()
            .into_iter()
            .filter(|(_, status)| matches!(status, TaskStatus::Failed { .. }))
            .map(|(name, _)| name)
            .collect()
    }

    fn set(&self, name: &str, status: TaskStatus) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.insert(name.to_owned(), status);
        }
    }
}

/// A registry of supervised tasks with a shared cancellation signal.
pub struct SupervisedTasks {
    cancellation: CancellationToken,
    health: Arc<TaskHealth>,
    handles: Vec<(String, JoinHandle<()>)>,
}

impl SupervisedTasks {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            health: Arc::new(TaskHealth::default()),
            handles: Vec::new(),
        }
    }

    /// Returns the shared liveness table.
    #[must_use]
    pub fn health(&self) -> Arc<TaskHealth> {
        Arc::clone(&self.health)
    }

    /// Returns the cancellation token every task observes.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Spawns a periodic task under supervision.
    pub fn spawn_interval<F, Fut>(&mut self, name: &str, interval: Duration, mut pass: F)
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let cancellation = self.cancellation.clone();
        let health = Arc::clone(&self.health);
        let task_name = name.to_owned();
        health.started(&task_name);
        let span = info_span!("cluster.task", task = %task_name);
        let handle = tokio::spawn(
            async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => {
                            health.stopped(&task_name);
                            info!(task = %task_name, "supervised cluster task stopped");
                            return;
                        }
                        _ = ticker.tick() => {
                            pass().await;
                            health.pass(&task_name);
                        }
                    }
                }
            }
            .instrument(span),
        );
        self.handles.push((name.to_owned(), handle));
    }

    /// Requests shutdown and waits for every task to finish.
    pub async fn shutdown(self) {
        self.cancellation.cancel();
        for (name, handle) in self.handles {
            match handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    self.health.failed(&name, error.to_string());
                }
            }
        }
    }

    /// Watches for tasks that ended unexpectedly and records the failure.
    ///
    /// Called by the readiness probe so a stopped task surfaces to operators
    /// rather than being discovered by its absence.
    pub fn reap(&mut self) {
        for (name, handle) in &self.handles {
            if handle.is_finished() && !self.cancellation.is_cancelled() {
                self.health
                    .failed(name, "task ended before shutdown was requested".to_owned());
            }
        }
    }
}

impl Default for SupervisedTasks {
    fn default() -> Self {
        Self::new()
    }
}

/// Node-local runtime settings for the cluster services.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeSettings {
    /// Physical representation this node writes.
    pub payload_format: PayloadFormat,
    /// Coordination bounds.
    pub coordinator: CoordinatorSettings,
    /// Interval at which this node reconciles its local replicas.
    pub reconcile_interval: Duration,
    /// Local payloads examined per reconciliation pass.
    pub reconcile_batch: usize,
    /// How long an unknown local payload is kept before it is collected.
    ///
    /// A payload the cluster does not know about may belong to a commit that has
    /// not reached this node yet, so age is what separates a real orphan from an
    /// in-flight write. When uncertain, the bytes are kept.
    pub orphan_grace_period: chrono::TimeDelta,
    /// Whether this node stores replicas.
    pub storage_node: bool,
}

impl RuntimeSettings {
    /// Creates settings for a storage node.
    #[must_use]
    pub fn storage(payload_format: PayloadFormat) -> Self {
        Self {
            payload_format,
            coordinator: CoordinatorSettings::default(),
            reconcile_interval: Duration::from_secs(300),
            reconcile_batch: 1_024,
            orphan_grace_period: chrono::TimeDelta::try_hours(24)
                .unwrap_or_else(chrono::TimeDelta::zero),
            storage_node: true,
        }
    }

    /// Creates settings for a control-plane node that stores no replicas.
    #[must_use]
    pub fn control() -> Self {
        Self {
            payload_format: PayloadFormat::Plaintext,
            coordinator: CoordinatorSettings::default(),
            reconcile_interval: Duration::from_secs(3_600),
            reconcile_batch: 0,
            orphan_grace_period: chrono::TimeDelta::try_hours(24)
                .unwrap_or_else(chrono::TimeDelta::zero),
            storage_node: false,
        }
    }
}

/// The cluster background services for one node.
pub struct ClusterRuntime {
    context: Arc<ClusterContext>,
    consensus: Arc<MetadataConsensus>,
    settings: RuntimeSettings,
    storage: Option<Arc<dyn ObjectStore>>,
    tasks: SupervisedTasks,
}

impl ClusterRuntime {
    /// Creates the runtime for one node.
    #[must_use]
    pub fn new(
        context: Arc<ClusterContext>,
        consensus: Arc<MetadataConsensus>,
        settings: RuntimeSettings,
    ) -> Self {
        Self {
            context,
            consensus,
            settings,
            storage: None,
            tasks: SupervisedTasks::new(),
        }
    }

    /// Attaches the object store used for capacity reporting.
    #[must_use]
    pub fn with_storage(mut self, storage: Arc<dyn ObjectStore>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Returns the shared liveness table.
    #[must_use]
    pub fn health(&self) -> Arc<TaskHealth> {
        self.tasks.health()
    }

    /// Starts every background service.
    pub fn start(&mut self, heartbeat_interval: Duration, limits: MovementLimits) {
        let context = Arc::clone(&self.context);
        let consensus = Arc::clone(&self.consensus);
        let storage = self.storage.clone();
        let storage_node = self.settings.storage_node;
        self.tasks
            .spawn_interval("heartbeat", heartbeat_interval, move || {
                let context = Arc::clone(&context);
                let storage = storage.clone();
                async move {
                    if let Err(error) = report_heartbeat(&context, storage.as_ref()).await {
                        warn!(%error, "heartbeat could not be recorded");
                    }
                }
            });

        let coordinator = Arc::new(Coordinator::new(
            Arc::clone(&self.context),
            Arc::clone(&consensus),
            self.settings.coordinator,
        ));
        self.tasks
            .spawn_interval("coordinator", heartbeat_interval, move || {
                let coordinator = Arc::clone(&coordinator);
                async move {
                    coordinator.run_once().await;
                }
            });

        if storage_node {
            let executor = Arc::new(TaskExecutor::new(
                Arc::clone(&self.context),
                self.settings.payload_format,
            ));
            self.tasks
                .spawn_interval("replica-movement", Duration::from_secs(2), move || {
                    let executor = Arc::clone(&executor);
                    async move {
                        executor.run_once(limits).await;
                    }
                });

            let context = Arc::clone(&self.context);
            let payload_format = self.settings.payload_format;
            let batch = self.settings.reconcile_batch;
            let orphan_grace_period = self.settings.orphan_grace_period;
            self.tasks
                .spawn_interval("reconcile", self.settings.reconcile_interval, move || {
                    let context = Arc::clone(&context);
                    async move {
                        if let Err(error) =
                            reconcile(&context, payload_format, batch, orphan_grace_period).await
                        {
                            warn!(%error, "replica reconciliation pass failed");
                        }
                    }
                });
        }
    }

    /// Requests shutdown and waits for the services to stop.
    pub async fn shutdown(self) {
        self.tasks.shutdown().await;
    }

    /// Returns this node's readiness, separating degraded from unavailable.
    pub async fn readiness(&self) -> Readiness {
        let failures = self.tasks.health().failures();
        let quorum = self.consensus.quorum().await;
        if !quorum.status.readable {
            return Readiness::Unavailable;
        }
        if !failures.is_empty() || !quorum.status.writable {
            return Readiness::Degraded;
        }
        match self.context.cluster.node(self.context.node_id).await {
            Ok(Some(node)) if node.state == NodeState::Healthy => Readiness::Ready,
            Ok(Some(_)) => Readiness::Degraded,
            Ok(None) => Readiness::Degraded,
            Err(_) => Readiness::Unavailable,
        }
    }
}

/// Reports this node's capacity and activity to the cluster.
///
/// Heartbeats deliberately carry only bounded aggregates: per-object detail
/// would make heartbeat volume scale with stored data.
pub async fn report_heartbeat(
    context: &ClusterContext,
    storage: Option<&Arc<dyn ObjectStore>>,
) -> Result<(), String> {
    let status = match storage {
        Some(storage) => storage.status().await.ok(),
        None => context.local.local_capacity().await.ok(),
    };
    let replica_bytes = context
        .cluster
        .usage()
        .await
        .map(|usage| usage.physical_bytes)
        .unwrap_or_default();
    let capacity = NodeCapacity {
        total_bytes: status
            .map(|status| status.capacity_bytes)
            .unwrap_or_default(),
        available_bytes: status
            .map(|status| status.available_bytes)
            .unwrap_or_default(),
        replica_bytes,
        temporary_bytes: status
            .map(|status| status.temporary_upload_bytes)
            .unwrap_or_default(),
    };
    let backlog = context
        .cluster
        .usage()
        .await
        .map(|usage| usage.active_tasks)
        .unwrap_or_default();
    context
        .commit(ClusterWrite::cluster(ClusterCommand::Heartbeat {
            node_id: context.node_id,
            capacity,
            activity: NodeActivity {
                replication_backlog: backlog,
                ..NodeActivity::default()
            },
            at: Utc::now(),
        }))
        .await
        .map_err(|error| error.to_string())
}

/// Reconciles this node's local bytes against authoritative placement.
///
/// A node that was offline must not be trusted about its own replicas: the
/// cluster's committed metadata decides what should exist, and anything the node
/// holds that the cluster has deleted is released rather than resurrected.
pub async fn reconcile(
    context: &ClusterContext,
    payload_format: PayloadFormat,
    batch: usize,
    orphan_grace_period: chrono::TimeDelta,
) -> Result<(), String> {
    if batch == 0 {
        return Ok(());
    }
    let now = Utc::now();
    let payloads = context
        .local
        .list_local_payloads(None, batch)
        .await
        .map_err(|error| error.to_string())?;
    let mut released = 0_u64;
    let mut repaired = 0_u64;
    let mut collected = 0_u64;
    for object_id in payloads {
        if let Some(tombstone) = context
            .cluster
            .tombstone(object_id)
            .await
            .map_err(|error| error.to_string())?
        {
            // The cluster deleted this payload while the node was away.
            context
                .local
                .delete_replica(object_id)
                .await
                .map_err(|error| error.to_string())?;
            let _ = context
                .commit(ClusterWrite::cluster(
                    ClusterCommand::AcknowledgeTombstone {
                        object_id: tombstone.object_id,
                        node_id: context.node_id,
                        at: Utc::now(),
                    },
                ))
                .await;
            released += 1;
            continue;
        }
        let Some(placement) = context
            .placement_for(object_id)
            .await
            .map_err(|error| error.to_string())?
        else {
            // Unknown to the cluster. It is only collected once it is older than
            // the grace period, so an in-flight commit is never destroyed.
            let stat = context
                .local
                .stat_replica(object_id)
                .await
                .map_err(|error| error.to_string())?;
            let stale = stat
                .and_then(|stat| stat.modified_at)
                .is_some_and(|modified| now.signed_duration_since(modified) > orphan_grace_period);
            if stale {
                context
                    .local
                    .delete_replica(object_id)
                    .await
                    .map_err(|error| error.to_string())?;
                collected += 1;
            }
            continue;
        };
        let Some(replica) = placement.replica(context.node_id) else {
            continue;
        };
        if replica.state == ReplicaState::Healthy {
            continue;
        }
        let observed = crate::tasks::verify_local(
            context,
            object_id,
            placement.size,
            payload_format,
            &placement.checksum,
        )
        .await
        .map_err(|error| error.to_string())?;
        if observed != replica.state {
            let _ = context
                .commit(ClusterWrite::cluster(ClusterCommand::SetReplicaState {
                    object_id,
                    node_id: context.node_id,
                    state: observed,
                    checksum: Some(placement.checksum.clone()),
                    verified: observed == ReplicaState::Healthy,
                    at: Utc::now(),
                }))
                .await;
            repaired += 1;
        }
    }

    // Replicas the cluster expects here but that are physically absent are
    // reported so repair can rebuild them.
    let expected = context
        .cluster
        .node_replicas(context.node_id, None, batch)
        .await
        .map_err(|error| error.to_string())?;
    let mut missing = 0_u64;
    for object_id in expected {
        if context
            .local
            .stat_replica(object_id)
            .await
            .map_err(|error| error.to_string())?
            .is_some()
        {
            continue;
        }
        let Some(placement) = context
            .placement_for(object_id)
            .await
            .map_err(|error| error.to_string())?
        else {
            continue;
        };
        if placement
            .replica(context.node_id)
            .is_some_and(|replica| replica.state == ReplicaState::Missing)
        {
            continue;
        }
        let _ = context
            .commit(ClusterWrite::cluster(ClusterCommand::SetReplicaState {
                object_id,
                node_id: context.node_id,
                state: ReplicaState::Missing,
                checksum: None,
                verified: false,
                at: Utc::now(),
            }))
            .await;
        missing += 1;
    }

    if released > 0 || repaired > 0 || missing > 0 || collected > 0 {
        info!(
            released,
            repaired, missing, collected, "reconciled local replicas against cluster metadata"
        );
    }
    Ok(())
}
