//! Replica movement execution.
//!
//! Repair, drain, rebalance, and tombstone cleanup all move or release replica
//! bytes, so they share one executor. The ordering is the same in every case:
//! copy, verify, commit the new replica, and only then release the old one. That
//! order is what makes a movement safe to interrupt at any point.

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use futures_util::TryStreamExt;
use oes_cluster::{
    ClusterCommand, PayloadPlacement, Replica, ReplicaState, ReplicaTask, ReplicaTaskKind,
    ReplicaTaskState,
};
use oes_consensus::ClusterWrite;
use oes_core::{Checksum, NodeId, ObjectId, PayloadFormat};
use oes_storage::{StorageError, WriteReplicaRequest, upload_stream};
use tracing::{debug, info, warn};

use crate::{context::ClusterContext, read::open_specific_replica};

/// Bounds applied to background replica movement.
#[derive(Debug, Clone, Copy)]
pub struct MovementLimits {
    /// Movements this node runs at once.
    pub concurrency: usize,
    /// Byte-per-second ceiling for one movement. Zero disables throttling.
    pub bytes_per_second: u64,
    /// Lease held on a claimed task.
    pub lease: Duration,
    /// Attempts before a task is parked for operator attention.
    pub maximum_attempts: u32,
}

impl Default for MovementLimits {
    fn default() -> Self {
        Self {
            concurrency: 4,
            bytes_per_second: 64 * 1024 * 1024,
            lease: Duration::from_secs(600),
            maximum_attempts: 8,
        }
    }
}

/// Executes replica movement tasks assigned to this node.
pub struct TaskExecutor {
    context: Arc<ClusterContext>,
    payload_format: PayloadFormat,
}

impl TaskExecutor {
    /// Creates an executor for one node.
    #[must_use]
    pub const fn new(context: Arc<ClusterContext>, payload_format: PayloadFormat) -> Self {
        Self {
            context,
            payload_format,
        }
    }

    /// Claims and runs the work currently assigned to this node.
    ///
    /// Returns how many tasks were executed. Concurrency is bounded so a failed
    /// node cannot turn into thousands of simultaneous transfers.
    pub async fn run_once(&self, limits: MovementLimits) -> usize {
        let page = match self.context.cluster.queued_tasks(512).await {
            Ok(page) => page,
            Err(error) => {
                warn!(%error, "could not read the replica movement queue");
                return 0;
            }
        };
        let mine: Vec<ReplicaTask> = page
            .tasks
            .into_iter()
            .filter(|task| matches!(task.state, ReplicaTaskState::Queued))
            .filter(|task| executor_of(task) == Some(self.context.node_id))
            .take(limits.concurrency)
            .collect();
        if mine.is_empty() {
            return 0;
        }
        let mut executed = 0;
        let mut running = Vec::new();
        for task in mine {
            let claimed = self
                .context
                .commit(ClusterWrite::cluster(ClusterCommand::ClaimTask {
                    task_id: task.id,
                    node_id: self.context.node_id,
                    lease_seconds: limits.lease.as_secs(),
                    at: Utc::now(),
                }))
                .await;
            if let Err(error) = claimed {
                debug!(task = %task.id, %error, "another node claimed the task first");
                continue;
            }
            running.push(task);
        }
        for task in running {
            let outcome = self.execute(&task, limits).await;
            let command = match outcome {
                Ok(()) => {
                    executed += 1;
                    ClusterCommand::CompleteTask {
                        task_id: task.id,
                        at: Utc::now(),
                    }
                }
                Err(reason) => {
                    warn!(task = %task.id, kind = %task.kind, %reason, "replica movement failed");
                    ClusterCommand::FailTask {
                        task_id: task.id,
                        reason,
                        maximum_attempts: limits.maximum_attempts,
                        at: Utc::now(),
                    }
                }
            };
            if let Err(error) = self.context.commit(ClusterWrite::cluster(command)).await {
                warn!(task = %task.id, %error, "could not record the movement outcome");
            }
        }
        executed
    }

    async fn execute(&self, task: &ReplicaTask, limits: MovementLimits) -> Result<(), String> {
        match task.kind {
            ReplicaTaskKind::Delete => self.execute_delete(task).await,
            _ => self.execute_copy(task, limits).await,
        }
    }

    async fn execute_delete(&self, task: &ReplicaTask) -> Result<(), String> {
        let object_id = task.object_id;
        self.context
            .local
            .delete_replica(object_id)
            .await
            .map_err(|error| error.to_string())?;
        // Acknowledging the tombstone is what lets the cluster eventually stop
        // tracking the deletion. Until every holder acknowledges, the tombstone
        // stays so a returning node cannot resurrect the payload.
        self.context
            .commit(ClusterWrite::cluster(
                ClusterCommand::AcknowledgeTombstone {
                    object_id,
                    node_id: self.context.node_id,
                    at: Utc::now(),
                },
            ))
            .await
            .map_err(|error| error.to_string())?;
        info!(%object_id, "released a tombstoned replica");
        Ok(())
    }

    async fn execute_copy(&self, task: &ReplicaTask, limits: MovementLimits) -> Result<(), String> {
        let placement = self
            .context
            .placement_for(task.object_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "placement metadata no longer exists".to_owned())?;
        let sources = repair_sources(&placement, self.context.node_id);
        if sources.is_empty() {
            return Err("no healthy replica remains to copy from".to_owned());
        }
        let mut last_error = String::from("no source could be read");
        for source in sources {
            match self.copy_from(&placement, source, task, limits).await {
                Ok(()) => {
                    self.commit_replica(&placement).await?;
                    if task.kind.removes_source()
                        && let Some(release) = task.source_node
                    {
                        self.release_source(&placement, release).await?;
                    }
                    return Ok(());
                }
                Err(reason) => {
                    warn!(
                        object = %task.object_id,
                        source = %source,
                        %reason,
                        "movement source failed; trying another replica"
                    );
                    last_error = reason;
                }
            }
        }
        Err(last_error)
    }

    async fn copy_from(
        &self,
        placement: &PayloadPlacement,
        source: NodeId,
        task: &ReplicaTask,
        limits: MovementLimits,
    ) -> Result<(), String> {
        let read = open_specific_replica(
            &self.context,
            source,
            placement.object_id,
            placement.size,
            &placement.checksum,
            self.payload_format,
        )
        .await
        .map_err(|error| error.to_string())?;
        let body = throttled(read.body, limits.bytes_per_second);
        // The destination validates against committed placement metadata rather
        // than against anything the source says about itself.
        self.context
            .local
            .write_replica(WriteReplicaRequest::known(
                operation_id(task),
                placement.object_id,
                placement.size,
                placement.checksum.clone(),
                upload_stream(body),
            ))
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn commit_replica(&self, placement: &PayloadPlacement) -> Result<(), String> {
        let replica = Replica::healthy(
            self.context.node_id,
            placement.size,
            placement.checksum.clone(),
            Utc::now(),
        );
        self.context
            .commit(ClusterWrite::cluster(ClusterCommand::UpsertReplica {
                object_id: placement.object_id,
                replica: Box::new(replica),
                at: Utc::now(),
            }))
            .await
            .map_err(|error| error.to_string())
    }

    async fn release_source(
        &self,
        placement: &PayloadPlacement,
        source: NodeId,
    ) -> Result<(), String> {
        // The source is only released after the destination replica is written,
        // verified, and committed, so a failure here never loses durability.
        if source == self.context.node_id {
            self.context
                .local
                .delete_replica(placement.object_id)
                .await
                .map_err(|error| error.to_string())?;
        } else {
            let target = self
                .context
                .target(source)
                .await
                .map_err(|error| error.to_string())?;
            self.context
                .transport
                .delete_replica(&target, placement.object_id)
                .await
                .map_err(|error| error.to_string())?;
        }
        self.context
            .commit(ClusterWrite::cluster(ClusterCommand::RemoveReplica {
                object_id: placement.object_id,
                node_id: source,
                at: Utc::now(),
            }))
            .await
            .map_err(|error| error.to_string())
    }
}

/// Returns which node is responsible for executing a task.
///
/// Copies are pulled by the destination, which spreads load and means a
/// donor node under pressure is never asked to push.
#[must_use]
pub fn executor_of(task: &ReplicaTask) -> Option<NodeId> {
    if task.kind.is_deletion() {
        task.source_node
    } else {
        task.target_node
    }
}

/// Returns healthy replicas that may be read as a movement source.
#[must_use]
pub fn repair_sources(placement: &PayloadPlacement, exclude: NodeId) -> Vec<NodeId> {
    placement
        .replicas
        .iter()
        .filter(|replica| replica.state.usable_as_source() && replica.node_id != exclude)
        .map(|replica| replica.node_id)
        .collect()
}

fn operation_id(task: &ReplicaTask) -> String {
    // The operation identity is derived from the task, so retrying a task after
    // a crash reuses the same staging identity instead of leaking a new one.
    format!("{}-{}", task.kind, task.id.as_uuid().simple())
}

/// Applies a byte-per-second ceiling to a movement stream.
///
/// Throttling here rather than in the transport keeps foreground traffic
/// unaffected: only background movement is slowed.
fn throttled(
    body: oes_storage::DownloadStream,
    bytes_per_second: u64,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send {
    body.map_err(std::io::Error::other).and_then(move |chunk| {
        let len = chunk.len() as u64;
        async move {
            if bytes_per_second > 0 {
                let micros = len.saturating_mul(1_000_000) / bytes_per_second.max(1);
                if micros > 0 {
                    tokio::time::sleep(Duration::from_micros(micros.min(1_000_000))).await;
                }
            }
            Ok(chunk)
        }
    })
}

/// Returns whether a replica record needs repair on a live node.
#[must_use]
pub fn needs_repair(placement: &PayloadPlacement, node_id: NodeId) -> bool {
    placement
        .replica(node_id)
        .is_some_and(|replica| replica.state.needs_repair())
}

/// Returns whether the payload has any replica record on a node.
#[must_use]
pub fn holds_replica(placement: &PayloadPlacement, node_id: NodeId) -> bool {
    placement.replica(node_id).is_some()
}

/// Returns the replica state a verification result implies.
#[must_use]
pub const fn state_for(present: bool, matches: bool) -> ReplicaState {
    if !present {
        ReplicaState::Missing
    } else if matches {
        ReplicaState::Healthy
    } else {
        ReplicaState::Corrupt
    }
}

/// Recomputes a payload checksum locally, used by scrubbing.
pub async fn verify_local(
    context: &ClusterContext,
    object_id: ObjectId,
    size: u64,
    payload_format: PayloadFormat,
    expected: &Checksum,
) -> Result<ReplicaState, StorageError> {
    let verification = context
        .local
        .verify_replica(object_id, size, payload_format, expected.clone())
        .await?;
    Ok(state_for(verification.present, verification.matches))
}
