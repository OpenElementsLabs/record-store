//! Replica movement execution.
//!
//! Repair, drain, rebalance, and tombstone cleanup all move or release replica
//! bytes, so they share one executor. The ordering is the same in every case:
//! copy, verify, commit the new replica, and only then release the old one. That
//! order is what makes a movement safe to interrupt at any point.

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use futures_util::TryStreamExt;
use record_store_cluster::{
    ClusterCommand, DeviceRecord, PayloadPlacement, Replica, ReplicaState, ReplicaTask,
    ReplicaTaskKind, ReplicaTaskState,
};
use record_store_consensus::ClusterWrite;
use record_store_core::{Checksum, DeviceId, NodeId, ObjectId, PayloadFormat};
use record_store_storage::{StorageError, WriteReplicaRequest, upload_stream};
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
            .delete_everywhere(object_id)
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
        for (source, source_device) in sources {
            match self
                .copy_from(&placement, source, source_device, task, limits)
                .await
            {
                Ok(destination) => {
                    self.commit_replica(&placement, destination).await?;
                    if task.kind.removes_source()
                        && let Some(release) = task.source_node
                    {
                        self.release_source(&placement, release, task.source_device)
                            .await?;
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

    /// Copies one replica and returns the device that received it.
    async fn copy_from(
        &self,
        placement: &PayloadPlacement,
        source: NodeId,
        source_device: DeviceId,
        task: &ReplicaTask,
        limits: MovementLimits,
    ) -> Result<DeviceId, String> {
        let read = open_specific_replica(
            &self.context,
            source,
            source_device,
            placement.object_id,
            placement.size,
            &placement.checksum,
            self.payload_format,
        )
        .await
        .map_err(|error| error.to_string())?;
        let body = throttled(read.body, limits.bytes_per_second);
        // Placement chose an exact device, so the copy lands there rather than
        // on whichever device happens to be this node's default.
        let destination = task
            .target_device
            .unwrap_or_else(|| self.context.local.default_device_id());
        let store = self
            .context
            .local
            .for_device(destination)
            .map_err(|error| error.to_string())?;
        // The destination validates against committed placement metadata rather
        // than against anything the source says about itself.
        store
            .write_replica(WriteReplicaRequest::known(
                operation_id(task),
                placement.object_id,
                placement.size,
                placement.checksum.clone(),
                upload_stream(body),
            ))
            .await
            .map(|_| destination)
            .map_err(|error| error.to_string())
    }

    async fn commit_replica(
        &self,
        placement: &PayloadPlacement,
        device_id: DeviceId,
    ) -> Result<(), String> {
        // The committed record names the device that actually holds the bytes,
        // which is what later repair, drain, and rebalance decisions read.
        let replica = Replica::healthy_on(
            self.context.node_id,
            device_id,
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
        source_device: Option<DeviceId>,
    ) -> Result<(), String> {
        // Prefer the device the task named, then the committed replica record.
        // Guessing is not an option: releasing the wrong device would delete a
        // replica that still counts toward durability.
        let device = source_device.or_else(|| {
            placement
                .replicas
                .iter()
                .find(|replica| replica.node_id == source)
                .map(|replica| replica.device_id)
        });
        // The source is only released after the destination replica is written,
        // verified, and committed, so a failure here never loses durability.
        if source == self.context.node_id {
            match device {
                Some(device) => {
                    self.context
                        .local
                        .for_device(device)
                        .map_err(|error| error.to_string())?
                        .delete_replica(placement.object_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                None => {
                    self.context
                        .local
                        .delete_everywhere(placement.object_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
        } else {
            let device = device.unwrap_or_else(|| DeviceRecord::legacy_id(source));
            let target = self
                .context
                .target(source, device)
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
pub fn repair_sources(placement: &PayloadPlacement, exclude: NodeId) -> Vec<(NodeId, DeviceId)> {
    placement
        .replicas
        .iter()
        .filter(|replica| replica.state.usable_as_source() && replica.node_id != exclude)
        .map(|replica| (replica.node_id, replica.device_id))
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
    body: record_store_storage::DownloadStream,
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
    device_id: DeviceId,
    object_id: ObjectId,
    size: u64,
    payload_format: PayloadFormat,
    expected: &Checksum,
) -> Result<ReplicaState, StorageError> {
    let verification = context
        .local
        .for_device(device_id)?
        .verify_replica(object_id, size, payload_format, expected.clone())
        .await?;
    Ok(state_for(verification.present, verification.matches))
}

#[cfg(test)]
mod tests {
    use record_store_cluster::{ReplicaState, ReplicaTaskKind};
    use record_store_core::{NodeId, ObjectId};

    use super::*;
    use crate::test_support::{placement, replica, task};

    /// Only a verified replica may be read as a movement source. Repairing from
    /// a stale or corrupt copy would spread the damage instead of fixing it.
    #[test]
    fn only_healthy_replicas_are_offered_as_repair_sources() {
        let healthy = NodeId::new();
        let excluded = NodeId::new();
        let unusable = [
            ReplicaState::Pending,
            ReplicaState::Repairing,
            ReplicaState::Stale,
            ReplicaState::Missing,
            ReplicaState::Deleting,
            ReplicaState::Corrupt,
        ];

        let mut replicas = vec![replica(healthy, ReplicaState::Healthy)];
        replicas.extend(
            unusable
                .into_iter()
                .map(|state| replica(NodeId::new(), state)),
        );
        replicas.push(replica(excluded, ReplicaState::Healthy));

        let sources = repair_sources(&placement(ObjectId::new(), replicas), excluded);
        assert_eq!(
            sources.iter().map(|(node, _)| *node).collect::<Vec<_>>(),
            vec![healthy],
            "only the healthy replica that is not excluded may be a source"
        );
        assert_eq!(
            sources.len(),
            1,
            "a source must carry exactly one device to read from"
        );
    }

    /// The node being repaired must never be its own source, even when its
    /// record still claims to be healthy.
    #[test]
    fn the_excluded_node_is_never_its_own_repair_source() {
        let node = NodeId::new();
        let sources = repair_sources(
            &placement(ObjectId::new(), vec![replica(node, ReplicaState::Healthy)]),
            node,
        );
        assert!(sources.is_empty());
    }

    /// Deletions run on the node that still holds the bytes; every other kind of
    /// movement runs on the node receiving them. Getting this backwards would
    /// ask an idle node to delete nothing while the real holder kept its copy.
    #[test]
    fn a_deletion_executes_on_the_holder_and_a_transfer_on_the_recipient() {
        let source = NodeId::new();
        let target = NodeId::new();

        assert_eq!(
            executor_of(&task(ReplicaTaskKind::Delete, source, target)),
            Some(source)
        );

        for kind in [
            ReplicaTaskKind::Repair,
            ReplicaTaskKind::RepairCorrupt,
            ReplicaTaskKind::Rebalance,
            ReplicaTaskKind::RebalanceDomain,
            ReplicaTaskKind::Drain,
        ] {
            assert_eq!(
                executor_of(&task(kind, source, target)),
                Some(target),
                "{kind:?} must execute on the recipient"
            );
        }
    }

    /// A replica that is absent, corrupt, or out of date needs rebuilding; one
    /// that is merely mid-transfer does not, or the repair loop would restart
    /// work already in flight.
    #[test]
    fn repair_is_needed_only_for_replicas_that_cannot_serve_the_payload() {
        let node = NodeId::new();
        for (state, expected) in [
            (ReplicaState::Missing, true),
            (ReplicaState::Corrupt, true),
            (ReplicaState::Stale, true),
            (ReplicaState::Healthy, false),
            (ReplicaState::Pending, false),
            (ReplicaState::Repairing, false),
            (ReplicaState::Deleting, false),
        ] {
            let placement = placement(ObjectId::new(), vec![replica(node, state)]);
            assert_eq!(
                needs_repair(&placement, node),
                expected,
                "{state:?} was classified incorrectly"
            );
        }
    }

    /// A node with no record at all is not "in need of repair" — it is simply
    /// not a holder, and treating it as damaged would queue endless work.
    #[test]
    fn a_node_without_a_replica_record_neither_holds_nor_needs_repair() {
        let holder = NodeId::new();
        let stranger = NodeId::new();
        let placement = placement(
            ObjectId::new(),
            vec![replica(holder, ReplicaState::Healthy)],
        );
        assert!(holds_replica(&placement, holder));
        assert!(!holds_replica(&placement, stranger));
        assert!(!needs_repair(&placement, stranger));
    }

    /// A record still counts as held while it is being deleted or rebuilt, so
    /// placement never double-assigns the same payload to one node.
    #[test]
    fn a_replica_record_counts_as_held_in_every_state() {
        let node = NodeId::new();
        for state in [
            ReplicaState::Pending,
            ReplicaState::Healthy,
            ReplicaState::Repairing,
            ReplicaState::Stale,
            ReplicaState::Missing,
            ReplicaState::Deleting,
            ReplicaState::Corrupt,
        ] {
            let placement = placement(ObjectId::new(), vec![replica(node, state)]);
            assert!(holds_replica(&placement, node), "{state:?}");
        }
    }

    /// Verification has three outcomes and each maps to exactly one state.
    /// Absent bytes must not be reported as corruption: the repair paths differ.
    #[test]
    fn verification_results_map_to_distinct_replica_states() {
        assert_eq!(state_for(true, true), ReplicaState::Healthy);
        assert_eq!(state_for(true, false), ReplicaState::Corrupt);
        assert_eq!(state_for(false, false), ReplicaState::Missing);
        assert_eq!(
            state_for(false, true),
            ReplicaState::Missing,
            "absent bytes are missing regardless of any stale checksum match"
        );
    }
}
