//! Cluster coordination.
//!
//! Exactly one node coordinates at a time: whichever node currently holds
//! metadata leadership. That gives a single scheduler without a separate
//! election mechanism and without making an external process a dependency of
//! the data plane. All coordination state lives in replicated metadata, so a
//! leadership change resumes work instead of restarting it.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use record_store_cluster::{
    ClusterCommand, ClusterOperationKind, ClusterOperationState, ClusterTopology,
    DecommissionSafety, ObjectPlacementRequest, OperationProgress, PayloadPlacement,
    RebalanceCandidate, ReplicaState, ReplicaTask, ReplicaTaskKind, ReplicaTaskPriority,
    ReplicaTaskState, evaluate_node, plan_rebalance,
};
use record_store_consensus::{ClusterWrite, MetadataConsensus};
use record_store_core::{NodeId, ObjectId};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::context::ClusterContext;

/// Bounds on one coordination pass.
#[derive(Debug, Clone, Copy)]
pub struct CoordinatorSettings {
    /// Placements examined per repair scan.
    pub scan_batch: usize,
    /// Tasks created per repair scan.
    pub enqueue_batch: usize,
    /// Tombstones examined per collection pass.
    pub tombstone_batch: usize,
}

impl Default for CoordinatorSettings {
    fn default() -> Self {
        Self {
            scan_batch: 512,
            enqueue_batch: 128,
            tombstone_batch: 256,
        }
    }
}

/// The leader-elected cluster coordinator.
pub struct Coordinator {
    context: Arc<ClusterContext>,
    consensus: Arc<MetadataConsensus>,
    settings: CoordinatorSettings,
    cursor: Mutex<Option<ObjectId>>,
    /// Leadership term the current pass is fenced to.
    term: Mutex<Option<u64>>,
}

impl Coordinator {
    /// Creates a coordinator for one node.
    #[must_use]
    pub fn new(
        context: Arc<ClusterContext>,
        consensus: Arc<MetadataConsensus>,
        settings: CoordinatorSettings,
    ) -> Self {
        Self {
            context,
            consensus,
            settings,
            cursor: Mutex::new(None),
            term: Mutex::new(None),
        }
    }

    /// Runs one coordination pass when this node holds metadata leadership.
    ///
    /// Returns whether the pass ran. A node that is not the leader does nothing,
    /// which is what keeps exactly one scheduler active.
    pub async fn run_once(&self) -> bool {
        let Some(term) = self.consensus.leadership_term().await else {
            return false;
        };
        // The term is pinned for the whole pass. A pass can outlive the
        // leadership that started it, and without this every decision made after
        // that point would simply be forwarded to the new leader — two
        // coordinators scheduling the same cluster, each unaware of the other.
        // Losing the term aborts the pass instead; the new leader re-derives the
        // same work from replicated state on its own next pass.
        *self.term.lock().await = Some(term);
        if let Err(error) = self.detect_failures().await {
            warn!(%error, "failure detection pass failed");
        }
        if let Err(error) = self.reclaim_expired_leases().await {
            warn!(%error, "movement lease reclamation failed");
        }
        if let Err(error) = self.schedule_repairs().await {
            warn!(%error, "repair scheduling pass failed");
        }
        if let Err(error) = self.collect_tombstones().await {
            warn!(%error, "tombstone collection pass failed");
        }
        if let Err(error) = self.progress_operations().await {
            warn!(%error, "cluster operation progress pass failed");
        }
        if let Err(error) = self.context.cluster.refresh_durability_counters().await {
            debug!(%error, "durability counters could not be refreshed");
        }
        true
    }

    /// Applies failure-detection transitions to every member.
    pub async fn detect_failures(&self) -> Result<(), String> {
        let topology = self.context.topology().await.map_err(display)?;
        let policy = topology.config.failure_detection;
        let now = Utc::now();
        for node in &topology.nodes {
            let Some(transition) = evaluate_node(node, &policy, now) else {
                continue;
            };
            info!(
                node = %node.node_id,
                from = %node.state,
                to = %transition.state,
                reason = %transition.reason,
                "node state transition"
            );
            self.apply(ClusterCommand::SetNodeState {
                node_id: node.node_id,
                state: transition.state,
                reason: Some(transition.reason),
                at: now,
            })
            .await?;
        }
        Ok(())
    }

    /// Returns claimed tasks whose lease expired to the queue.
    pub async fn reclaim_expired_leases(&self) -> Result<(), String> {
        let page = self
            .context
            .cluster
            .queued_tasks(self.settings.scan_batch)
            .await
            .map_err(display)?;
        let now = Utc::now();
        for task in page.tasks {
            if task.lease_expired(now) {
                info!(task = %task.id, "reclaiming a movement task whose lease expired");
                self.apply(ClusterCommand::RequeueTask {
                    task_id: task.id,
                    reason: Some("execution lease expired".into()),
                    at: now,
                })
                .await?;
            }
        }
        Ok(())
    }

    /// Enqueues repair work for payloads that are not fully durable.
    pub async fn schedule_repairs(&self) -> Result<(), String> {
        let topology = self.context.topology().await.map_err(display)?;
        let mut cursor = self.cursor.lock().await;
        let page = self
            .context
            .cluster
            .list_placements(*cursor, self.settings.scan_batch)
            .await
            .map_err(display)?;
        // The cursor rotates so a large cluster is scanned incrementally rather
        // than being walked completely on every pass.
        *cursor = page.next_object_id;
        drop(cursor);

        let mut created = 0_usize;
        for placement in page.placements {
            if created >= self.settings.enqueue_batch {
                break;
            }
            if let Some(task) = self.plan_repair(&placement, &topology).await? {
                self.apply(ClusterCommand::EnqueueTask {
                    task: Box::new(task),
                })
                .await?;
                created += 1;
            }
        }
        if created > 0 {
            info!(created, "queued replica repair work");
        }
        Ok(())
    }

    async fn plan_repair(
        &self,
        placement: &PayloadPlacement,
        topology: &ClusterTopology,
    ) -> Result<Option<ReplicaTask>, String> {
        let durability = placement.durability(topology);
        let draining: BTreeSet<NodeId> = topology
            .nodes
            .iter()
            .filter(|node| node.state == record_store_cluster::NodeState::Draining)
            .map(|node| node.node_id)
            .collect();
        let drains_pending = placement
            .replicas
            .iter()
            .any(|replica| draining.contains(&replica.node_id));
        if !durability.under_replicated() && !drains_pending && durability.damaged == 0 {
            return Ok(None);
        }
        if durability.healthy == 0 {
            // Nothing can be copied from, so repair is impossible until a holder
            // returns. This is reported rather than retried in a tight loop.
            debug!(
                object = %placement.object_id,
                "payload has no healthy replica; repair is not currently possible"
            );
            return Ok(None);
        }

        let existing: BTreeSet<(NodeId, record_store_core::DeviceId)> = placement
            .replicas
            .iter()
            .filter(|replica| replica.state == ReplicaState::Healthy)
            .map(|replica| (replica.node_id, replica.device_id))
            .collect();
        let damaged: Vec<(NodeId, record_store_core::DeviceId)> = placement
            .replicas
            .iter()
            .filter(|replica| replica.state.needs_repair())
            .map(|replica| (replica.node_id, replica.device_id))
            .collect();
        let leaving: Vec<(NodeId, record_store_core::DeviceId)> = placement
            .replicas
            .iter()
            .filter(|replica| {
                draining.contains(&replica.node_id)
                    || topology
                        .device(replica.device_id)
                        .is_some_and(|(_, device)| {
                            matches!(
                                device.state,
                                record_store_cluster::DeviceState::Draining
                                    | record_store_cluster::DeviceState::Failed
                            )
                        })
            })
            .map(|replica| (replica.node_id, replica.device_id))
            .collect();

        // A failed device has already taken its copies; a draining one still
        // has readable replicas that must be moved and then released. Calling
        // both "drain" makes an incident indistinguishable from an evacuation in
        // the repair queue, and makes the executor try to delete from a device
        // that is gone.
        let failed: Vec<(NodeId, record_store_core::DeviceId)> = placement
            .replicas
            .iter()
            .filter(|replica| {
                topology
                    .device(replica.device_id)
                    .is_some_and(|(_, device)| {
                        device.state == record_store_cluster::DeviceState::Failed
                    })
            })
            .map(|replica| (replica.node_id, replica.device_id))
            .collect();

        let (kind, source) = if !failed.is_empty() {
            (ReplicaTaskKind::DeviceFailed, None)
        } else if !leaving.is_empty() {
            (ReplicaTaskKind::Drain, leaving.first().copied())
        } else if damaged.iter().any(|(node, device)| {
            placement
                .replica_on(*node, *device)
                .is_some_and(|replica| replica.state == ReplicaState::Corrupt)
        }) {
            (ReplicaTaskKind::RepairCorrupt, None)
        } else {
            (ReplicaTaskKind::Repair, None)
        };

        // A damaged replica is rebuilt in place where possible, so the node that
        // holds it is preferred as the destination.
        let target = if let Some(target) = damaged.first().copied() {
            Some(target)
        } else {
            let request = ObjectPlacementRequest::new(
                placement.object_id,
                placement.desired_replicas.max(1),
                1,
                placement.storage_class.clone(),
            )
            .with_size_hint(Some(placement.size))
            .with_existing_targets(existing.iter().copied())
            .with_excluded_nodes(draining.iter().copied());
            match self.context.placement.place(&request, topology) {
                Ok(plan) => plan
                    .targets
                    .first()
                    .map(|target| (target.node_id, target.device_id)),
                Err(error) => {
                    debug!(
                        object = %placement.object_id,
                        %error,
                        "no eligible destination for repair right now"
                    );
                    None
                }
            }
        };
        let Some((target_node, target_device)) = target else {
            return Ok(None);
        };

        let priority = ReplicaTaskPriority::classify(kind, durability.healthy, durability.desired);
        Ok(Some(
            ReplicaTask::queued(
                placement.object_id,
                kind,
                priority,
                placement.size,
                Utc::now(),
            )
            .with_target(Some(target_node))
            .with_target_device(Some(target_device))
            .with_source(source.map(|(node, _)| node))
            .with_source_device(source.map(|(_, device)| device)),
        ))
    }

    /// Queues replica deletions for tombstones and purges completed ones.
    pub async fn collect_tombstones(&self) -> Result<(), String> {
        let config = self.context.config().await.map_err(display)?;
        let pending = self
            .context
            .cluster
            .pending_tombstones(self.settings.tombstone_batch)
            .await
            .map_err(display)?;
        for tombstone in pending {
            for node_id in &tombstone.pending_nodes {
                let task = ReplicaTask::queued(
                    tombstone.object_id,
                    ReplicaTaskKind::Delete,
                    ReplicaTaskPriority::Normal,
                    0,
                    Utc::now(),
                )
                .with_source(Some(*node_id));
                self.apply(ClusterCommand::EnqueueTask {
                    task: Box::new(task),
                })
                .await?;
            }
        }
        let purgeable = self
            .context
            .cluster
            .purgeable_tombstones(
                config.tombstone_retention_hours,
                Utc::now(),
                self.settings.tombstone_batch,
            )
            .await
            .map_err(display)?;
        for object_id in purgeable {
            self.apply(ClusterCommand::PurgeTombstone { object_id })
                .await?;
        }
        Ok(())
    }

    /// Advances drain, rebalance, and decommission operations.
    pub async fn progress_operations(&self) -> Result<(), String> {
        let operations = self.context.cluster.operations(64).await.map_err(display)?;
        for operation in operations {
            // A paused operation stays outstanding but creates no new work.
            if !operation.state.progressing() {
                continue;
            }
            let Some(node_id) = operation.node_id else {
                self.progress_rebalance(&operation).await?;
                continue;
            };
            let remaining = self
                .context
                .cluster
                .node_replica_count(node_id)
                .await
                .map_err(display)?;
            let bytes = self.remaining_bytes(node_id).await?;
            let moving = self.moving_count(node_id).await?;
            let progress = OperationProgress {
                objects_remaining: remaining,
                bytes_remaining: bytes,
                objects_moved: operation.progress.objects_moved,
                bytes_moved: operation.progress.bytes_moved,
                replicas_moving: moving,
                tasks_parked: operation.progress.tasks_parked,
            };
            let state = if remaining == 0 {
                match operation.kind {
                    ClusterOperationKind::Drain | ClusterOperationKind::Decommission => {
                        ClusterOperationState::Completed
                    }
                    ClusterOperationKind::Rebalance => ClusterOperationState::Completed,
                }
            } else {
                ClusterOperationState::Moving
            };
            let message = if remaining == 0 {
                Some(format!("node {node_id} no longer holds any replica"))
            } else {
                Some(format!("{remaining} replica(s) still to move"))
            };
            // Evacuating the data and retiring the consensus membership are two
            // different jobs, and only the first was being done. A node marked
            // decommissioned but still in the voter set counts toward every
            // quorum for ever: decommission three of five and the cluster
            // silently needs a majority of a group that no longer exists.
            //
            // The membership change is therefore a precondition of calling the
            // operation complete, not a step after it. Doing it the other way
            // round means a transient failure here leaves an operation that is
            // already Completed — which later passes skip — and the node keeps
            // its vote with nothing left to notice.
            let mut state = state;
            if state == ClusterOperationState::Completed
                && operation.kind == ClusterOperationKind::Decommission
            {
                self.apply(ClusterCommand::SetNodeState {
                    node_id,
                    state: record_store_cluster::NodeState::Decommissioned,
                    reason: Some("decommission completed".into()),
                    at: Utc::now(),
                })
                .await?;
                if let Err(error) = self.retire_membership(node_id).await {
                    warn!(
                        node = %node_id,
                        %error,
                        "the decommissioned node could not be retired from the metadata group; \
                         the operation stays outstanding so the next pass retries it"
                    );
                    state = ClusterOperationState::Moving;
                }
            }
            self.apply(ClusterCommand::UpdateOperation {
                operation_id: operation.id,
                state,
                progress,
                message,
                at: Utc::now(),
            })
            .await?;
        }
        Ok(())
    }

    /// Removes a decommissioned node from the metadata consensus group.
    ///
    /// Ordering matters and is the reason this is a separate step: the node is
    /// demoted to a learner before it is removed, so the voter set shrinks
    /// through the consensus library's own joint-consensus protocol rather than
    /// by editing membership. Removing the last voter is refused outright — a
    /// cluster with no metadata authority cannot be recovered by consensus
    /// alone.
    ///
    /// This is idempotent. A leader change partway through leaves the node
    /// either still a voter or already gone, and the next coordination pass on
    /// the surviving leader repeats the step from whichever state it finds.
    async fn retire_membership(&self, node_id: NodeId) -> Result<(), String> {
        let Some(node) = self.context.cluster.node(node_id).await.map_err(display)? else {
            return Ok(());
        };
        match self.consensus.remove_member(node.raft_id).await {
            Ok(()) => {
                info!(
                    node = %node_id,
                    member = node.raft_id,
                    "retired a decommissioned node from the metadata consensus group"
                );
                Ok(())
            }
            Err(record_store_consensus::ConsensusError::Configuration(reason)) => {
                // The only configuration refusal here is "that was the last
                // voter". Reporting it is right: silently leaving the node in
                // the group would be the original defect, and silently removing
                // it would destroy the cluster's metadata authority.
                warn!(
                    node = %node_id,
                    member = node.raft_id,
                    %reason,
                    "refusing to retire the node from consensus"
                );
                Err(reason)
            }
            Err(error) => Err(error.to_string()),
        }
    }

    async fn progress_rebalance(
        &self,
        operation: &record_store_cluster::ClusterOperation,
    ) -> Result<(), String> {
        let page = self
            .context
            .cluster
            .queued_tasks(self.settings.scan_batch)
            .await
            .map_err(display)?;
        let outstanding = page
            .tasks
            .iter()
            .filter(|task| task.operation_id == Some(operation.id))
            .count();
        let state = if outstanding == 0 {
            ClusterOperationState::Completed
        } else {
            ClusterOperationState::Moving
        };
        self.apply(ClusterCommand::UpdateOperation {
            operation_id: operation.id,
            state,
            progress: OperationProgress {
                replicas_moving: u32::try_from(outstanding).unwrap_or(u32::MAX),
                ..operation.progress
            },
            message: Some(format!("{outstanding} movement(s) outstanding")),
            at: Utc::now(),
        })
        .await
    }

    /// Plans capacity rebalancing and queues the resulting movements.
    pub async fn rebalance(
        &self,
        operation_id: Option<record_store_core::ClusterOperationId>,
    ) -> Result<usize, String> {
        // Planning reads the whole placement table and then queues movement for
        // it. That is scheduling, so it is fenced to one term for the same
        // reason a pass is: half a plan from a deposed leader interleaved with a
        // fresh plan from the new one would move the same replicas twice.
        let term = self.consensus.leadership_term().await.ok_or_else(|| {
            "rebalancing requires metadata leadership; this node does not hold it".to_owned()
        })?;
        *self.term.lock().await = Some(term);
        let topology = self.context.topology().await.map_err(display)?;
        let page = self
            .context
            .cluster
            .list_placements(None, self.settings.scan_batch)
            .await
            .map_err(display)?;
        let candidates: Vec<RebalanceCandidate> = page
            .placements
            .into_iter()
            .map(|placement| RebalanceCandidate { placement })
            .collect();
        let moves = plan_rebalance(
            &topology,
            &candidates,
            self.context.placement.as_ref(),
            usize::try_from(topology.config.rebalance.movement.batch_size).unwrap_or(64),
        );
        let planned = moves.len();
        for movement in moves {
            let task = ReplicaTask::queued(
                movement.object_id,
                ReplicaTaskKind::Rebalance,
                ReplicaTaskPriority::Low,
                movement.size,
                Utc::now(),
            )
            .with_source(Some(movement.source_node))
            .with_source_device(Some(movement.source_device))
            .with_target(Some(movement.target_node))
            .with_target_device(Some(movement.target_device))
            .with_operation(operation_id);
            self.apply(ClusterCommand::EnqueueTask {
                task: Box::new(task),
            })
            .await?;
        }
        if planned > 0 {
            info!(planned, "queued rebalance movements");
        }
        Ok(planned)
    }

    /// Evaluates whether a node can be removed without losing durability.
    pub async fn decommission_safety(&self, node_id: NodeId) -> Result<DecommissionSafety, String> {
        let topology = self.context.topology().await.map_err(display)?;
        let mut cursor = None;
        let mut at_risk = DecommissionSafety::evaluate(&topology, node_id, &[]);
        loop {
            let page = self
                .context
                .cluster
                .list_placements(cursor, self.settings.scan_batch)
                .await
                .map_err(display)?;
            if page.placements.is_empty() {
                break;
            }
            let partial = DecommissionSafety::evaluate(&topology, node_id, &page.placements);
            at_risk = DecommissionSafety {
                node_id,
                safe: at_risk.safe && partial.safe,
                at_risk_payloads: at_risk.at_risk_payloads + partial.at_risk_payloads,
                unavailable_payloads: at_risk.unavailable_payloads + partial.unavailable_payloads,
                replicas_remaining: at_risk.replicas_remaining + partial.replicas_remaining,
                bytes_remaining: at_risk.bytes_remaining + partial.bytes_remaining,
                reason: partial.reason,
            };
            cursor = page.next_object_id;
            if cursor.is_none() {
                break;
            }
        }
        // The aggregated reason is rebuilt so it reflects the whole cluster
        // rather than only the last page examined.
        let required = topology.config.required_acknowledgements();
        at_risk.reason = if at_risk.safe {
            format!(
                "removing node {node_id} keeps every object version at or above the required \
                 durability of {required} replica(s); {} replica(s) would be released",
                at_risk.replicas_remaining
            )
        } else {
            format!(
                "{} object version(s) would become unreadable and {} would fall below the \
                 required durability of {required} replica(s)",
                at_risk.unavailable_payloads, at_risk.at_risk_payloads
            )
        };
        Ok(at_risk)
    }

    async fn remaining_bytes(&self, node_id: NodeId) -> Result<u64, String> {
        let payloads = self
            .context
            .cluster
            .node_replicas(node_id, None, self.settings.scan_batch)
            .await
            .map_err(display)?;
        let mut total = 0_u64;
        for object_id in payloads {
            if let Some(placement) = self
                .context
                .placement_for(object_id)
                .await
                .map_err(display)?
            {
                total = total.saturating_add(placement.size);
            }
        }
        Ok(total)
    }

    async fn moving_count(&self, node_id: NodeId) -> Result<u32, String> {
        let page = self
            .context
            .cluster
            .queued_tasks(self.settings.scan_batch)
            .await
            .map_err(display)?;
        Ok(u32::try_from(
            page.tasks
                .iter()
                .filter(|task| {
                    matches!(task.state, ReplicaTaskState::Running { .. })
                        && (task.source_node == Some(node_id) || task.target_node == Some(node_id))
                })
                .count(),
        )
        .unwrap_or(u32::MAX))
    }

    /// Commits a coordination decision, but only while this node still leads.
    ///
    /// Coordination writes are deliberately not forwarded. A client write only
    /// needs to reach whichever node can commit it; a scheduler's write is an
    /// exercise of authority this node may no longer hold, and forwarding it
    /// would launder a deposed leader's decision through the current one.
    async fn apply(&self, command: ClusterCommand) -> Result<(), String> {
        let pinned = *self.term.lock().await;
        let term = match pinned {
            Some(term) => term,
            // Called outside a pass, by an operator-initiated action. Planning
            // still belongs to the leader — a follower that planned and then
            // forwarded would be a second scheduler — so the term is acquired
            // here instead, and a follower is refused rather than forwarded.
            None => self.consensus.leadership_term().await.ok_or_else(|| {
                "cluster coordination requires metadata leadership; this node does not hold it"
                    .to_owned()
            })?,
        };
        let name = command.name();
        match self
            .consensus
            .write_as_leader(term, ClusterWrite::cluster(command))
            .await
        {
            Ok(record_store_consensus::ClusterWriteResponse::Rejected(rejection)) => {
                Err(rejection.message)
            }
            Ok(_) => Ok(()),
            Err(record_store_consensus::ConsensusError::NoLeader) => {
                // Not an error worth alarming about: leadership moved, and the
                // node that holds it now will schedule the same work.
                debug!(
                    command = name,
                    "leadership changed during the coordination pass"
                );
                Err("leadership changed during the coordination pass".to_owned())
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

fn display<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

/// Returns the interval a coordination pass should use.
#[must_use]
pub fn coordination_interval(topology: &ClusterTopology) -> Duration {
    Duration::from_secs(
        topology
            .config
            .failure_detection
            .heartbeat_interval_seconds
            .max(1),
    )
}

/// Returns the last time a node was seen, for operator-facing output.
#[must_use]
pub fn last_seen(node: &record_store_cluster::NodeRecord) -> DateTime<Utc> {
    node.last_heartbeat_at.unwrap_or(node.joined_at)
}
