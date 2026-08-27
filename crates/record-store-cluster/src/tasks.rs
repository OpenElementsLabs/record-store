//! Replica movement tasks and long-running cluster operations.
//!
//! Repair, rebalance, and drain all move replica bytes with identical safety
//! requirements: copy, verify, commit the new replica, and only then release the
//! old one. They therefore share one durable queue instead of three, which keeps
//! prioritisation, throttling, and restart recovery in a single place.

use std::fmt::{self, Display, Formatter};

use chrono::{DateTime, TimeDelta, Utc};
use oes_core::{ClusterOperationId, NodeId, ObjectId, ReplicaTaskId};
use serde::{Deserialize, Serialize};

/// Why a replica needs to move or be rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaTaskKind {
    /// Restore a payload to its desired replica count.
    Repair,
    /// Replace a replica whose stored bytes failed verification.
    RepairCorrupt,
    /// Move replicas off a node being drained.
    Drain,
    /// Even out capacity utilization across nodes.
    Rebalance,
    /// Improve failure-domain spread without changing the replica count.
    RebalanceDomain,
    /// Remove a replica that a tombstone says must not exist.
    Delete,
}

impl ReplicaTaskKind {
    /// Returns whether the source replica must be removed after commit.
    #[must_use]
    pub const fn removes_source(self) -> bool {
        matches!(self, Self::Drain | Self::Rebalance | Self::RebalanceDomain)
    }

    /// Returns whether the task only removes bytes.
    #[must_use]
    pub const fn is_deletion(self) -> bool {
        matches!(self, Self::Delete)
    }

    /// Returns the movement budget this task draws from.
    #[must_use]
    pub const fn budget(self) -> MovementBudget {
        match self {
            Self::Repair | Self::RepairCorrupt | Self::Drain | Self::Delete => {
                MovementBudget::Repair
            }
            Self::Rebalance | Self::RebalanceDomain => MovementBudget::Rebalance,
        }
    }
}

impl Display for ReplicaTaskKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Repair => "repair",
            Self::RepairCorrupt => "repair-corrupt",
            Self::Drain => "drain",
            Self::Rebalance => "rebalance",
            Self::RebalanceDomain => "rebalance-topology",
            Self::Delete => "delete",
        })
    }
}

/// Which configured throttle a task is limited by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovementBudget {
    /// Durability-restoring work.
    Repair,
    /// Capacity-balancing work.
    Rebalance,
}

/// Risk-ordered task priority. Lower values run first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplicaTaskPriority {
    /// No healthy replica remains: the payload is currently unreadable.
    Unavailable,
    /// One healthy replica remains: another failure loses the data.
    Critical,
    /// Below the desired replica count with margin remaining.
    High,
    /// Administrative movement that must finish for an operation to complete.
    Normal,
    /// Fully replicated but topologically or capacity-wise suboptimal.
    Low,
}

impl ReplicaTaskPriority {
    /// Returns the durable sort rank.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Unavailable => 0,
            Self::Critical => 1,
            Self::High => 2,
            Self::Normal => 3,
            Self::Low => 4,
        }
    }

    /// Classifies repair urgency from replica accounting.
    ///
    /// Repair work is not uniform: losing the last healthy replica is
    /// categorically worse than dropping from three replicas to two.
    #[must_use]
    pub const fn classify(kind: ReplicaTaskKind, healthy: u32, desired: u32) -> Self {
        match kind {
            ReplicaTaskKind::Delete => Self::Normal,
            ReplicaTaskKind::Rebalance | ReplicaTaskKind::RebalanceDomain => Self::Low,
            ReplicaTaskKind::Drain => {
                if healthy <= 1 {
                    Self::Critical
                } else {
                    Self::Normal
                }
            }
            ReplicaTaskKind::Repair | ReplicaTaskKind::RepairCorrupt => {
                if healthy == 0 {
                    Self::Unavailable
                } else if healthy == 1 && desired > 1 {
                    Self::Critical
                } else if healthy < desired {
                    Self::High
                } else {
                    Self::Low
                }
            }
        }
    }
}

/// Execution state of one replica movement task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReplicaTaskState {
    /// Waiting to be claimed.
    Queued,
    /// Claimed by a node under a time-bounded lease.
    Running {
        /// Node executing the task.
        node_id: NodeId,
        /// When execution started.
        started_at: DateTime<Utc>,
        /// When the claim expires and the task returns to the queue.
        lease_expires_at: DateTime<Utc>,
    },
    /// Finished successfully.
    Completed {
        /// Completion time.
        completed_at: DateTime<Utc>,
    },
    /// Exhausted its retry budget and parked for operator attention.
    Parked {
        /// Time the task was parked.
        parked_at: DateTime<Utc>,
        /// Last failure message.
        reason: String,
    },
    /// Cancelled administratively or because it became unnecessary.
    Cancelled {
        /// Cancellation time.
        cancelled_at: DateTime<Utc>,
        /// Operator-facing reason.
        reason: String,
    },
}

impl ReplicaTaskState {
    /// Returns whether the task still needs work.
    #[must_use]
    pub const fn active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running { .. })
    }
}

/// One durable, restart-safe replica movement task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaTask {
    /// Stable task identifier exposed through the API and CLI.
    pub id: ReplicaTaskId,
    /// Payload the task acts on.
    pub object_id: ObjectId,
    /// Why the task exists.
    pub kind: ReplicaTaskKind,
    /// Risk ordering.
    pub priority: ReplicaTaskPriority,
    /// Node whose replica must be removed, for movement and deletion tasks.
    pub source_node: Option<NodeId>,
    /// Node the replica must exist on when the task completes.
    pub target_node: Option<NodeId>,
    /// Long-running operation this task belongs to.
    pub operation_id: Option<ClusterOperationId>,
    /// Payload size, used for progress accounting and throttling.
    pub size: u64,
    /// Current execution state.
    pub state: ReplicaTaskState,
    /// Failed attempts so far.
    pub attempts: u32,
    /// Most recent failure message.
    pub last_error: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last modification time.
    pub updated_at: DateTime<Utc>,
}

impl ReplicaTask {
    /// Creates a queued task.
    #[must_use]
    pub fn queued(
        object_id: ObjectId,
        kind: ReplicaTaskKind,
        priority: ReplicaTaskPriority,
        size: u64,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id: ReplicaTaskId::new(),
            object_id,
            kind,
            priority,
            source_node: None,
            target_node: None,
            operation_id: None,
            size,
            state: ReplicaTaskState::Queued,
            attempts: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Sets the node whose replica must be released or removed.
    #[must_use]
    pub const fn with_source(mut self, node_id: Option<NodeId>) -> Self {
        self.source_node = node_id;
        self
    }

    /// Pins the destination node.
    #[must_use]
    pub const fn with_target(mut self, node_id: Option<NodeId>) -> Self {
        self.target_node = node_id;
        self
    }

    /// Associates the task with a long-running operation.
    #[must_use]
    pub const fn with_operation(mut self, operation_id: Option<ClusterOperationId>) -> Self {
        self.operation_id = operation_id;
        self
    }

    /// Claims the task for a node under a lease.
    pub fn claim(&mut self, node_id: NodeId, lease_seconds: u64, now: DateTime<Utc>) {
        let lease = TimeDelta::try_seconds(i64::try_from(lease_seconds).unwrap_or(600))
            .unwrap_or_else(TimeDelta::zero);
        self.state = ReplicaTaskState::Running {
            node_id,
            started_at: now,
            lease_expires_at: now + lease,
        };
        self.updated_at = now;
    }

    /// Returns whether a running lease has expired and must be reclaimed.
    #[must_use]
    pub fn lease_expired(&self, now: DateTime<Utc>) -> bool {
        matches!(
            &self.state,
            ReplicaTaskState::Running {
                lease_expires_at, ..
            } if *lease_expires_at <= now
        )
    }

    /// Returns the task to the queue, for example after a lease expired.
    pub fn requeue(&mut self, reason: Option<String>, now: DateTime<Utc>) {
        self.state = ReplicaTaskState::Queued;
        if reason.is_some() {
            self.last_error = reason;
        }
        self.updated_at = now;
    }

    /// Records a failure, parking the task once the attempt budget is spent.
    pub fn fail(&mut self, reason: String, maximum_attempts: u32, now: DateTime<Utc>) {
        self.attempts = self.attempts.saturating_add(1);
        self.last_error = Some(reason.clone());
        self.updated_at = now;
        self.state = if self.attempts >= maximum_attempts {
            ReplicaTaskState::Parked {
                parked_at: now,
                reason,
            }
        } else {
            ReplicaTaskState::Queued
        };
    }

    /// Marks the task completed.
    pub fn complete(&mut self, now: DateTime<Utc>) {
        self.state = ReplicaTaskState::Completed { completed_at: now };
        self.updated_at = now;
    }

    /// Cancels the task.
    pub fn cancel(&mut self, reason: String, now: DateTime<Utc>) {
        self.state = ReplicaTaskState::Cancelled {
            cancelled_at: now,
            reason,
        };
        self.updated_at = now;
    }
}

/// The kind of long-running cluster operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterOperationKind {
    /// Move every replica off a node so it can be stopped safely.
    Drain,
    /// Even out utilization across the cluster.
    Rebalance,
    /// Permanently remove a node.
    Decommission,
}

impl Display for ClusterOperationKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Drain => "drain",
            Self::Rebalance => "rebalance",
            Self::Decommission => "decommission",
        })
    }
}

/// Lifecycle of a long-running cluster operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterOperationState {
    /// Enumerating the work to be done.
    Planning,
    /// Replica movement is in progress.
    Moving,
    /// All movement finished; verifying durability before completing.
    Verifying,
    /// Finished successfully.
    Completed,
    /// Stopped by an operator.
    Cancelled,
    /// Stopped because it could not proceed safely.
    Failed,
}

impl ClusterOperationState {
    /// Returns whether the operation still needs coordination.
    #[must_use]
    pub const fn active(self) -> bool {
        matches!(self, Self::Planning | Self::Moving | Self::Verifying)
    }
}

impl Display for ClusterOperationState {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Planning => "planning",
            Self::Moving => "moving",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        })
    }
}

/// Durable record of one long-running cluster operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterOperation {
    /// Stable identifier exposed through the API and CLI.
    pub id: ClusterOperationId,
    /// What kind of operation this is.
    pub kind: ClusterOperationKind,
    /// Node the operation targets, when it is node-scoped.
    pub node_id: Option<NodeId>,
    /// Current lifecycle state.
    pub state: ClusterOperationState,
    /// Progress counters.
    pub progress: OperationProgress,
    /// Time the operation started.
    pub started_at: DateTime<Utc>,
    /// Last progress update.
    pub updated_at: DateTime<Utc>,
    /// Completion time.
    pub completed_at: Option<DateTime<Utc>>,
    /// Operator-facing message.
    pub message: Option<String>,
}

impl ClusterOperation {
    /// Creates a planning operation.
    #[must_use]
    pub fn planning(
        kind: ClusterOperationKind,
        node_id: Option<NodeId>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id: ClusterOperationId::new(),
            kind,
            node_id,
            state: ClusterOperationState::Planning,
            progress: OperationProgress::default(),
            started_at: now,
            updated_at: now,
            completed_at: None,
            message: None,
        }
    }
}

/// Queryable progress for a long-running operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationProgress {
    /// Payloads that still need movement.
    pub objects_remaining: u64,
    /// Bytes that still need movement.
    pub bytes_remaining: u64,
    /// Payloads already moved.
    pub objects_moved: u64,
    /// Bytes already moved.
    pub bytes_moved: u64,
    /// Movement tasks currently running.
    pub replicas_moving: u32,
    /// Tasks parked after exhausting their retries.
    pub tasks_parked: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_priority_follows_remaining_durability() {
        assert_eq!(
            ReplicaTaskPriority::classify(ReplicaTaskKind::Repair, 0, 3),
            ReplicaTaskPriority::Unavailable
        );
        assert_eq!(
            ReplicaTaskPriority::classify(ReplicaTaskKind::Repair, 1, 3),
            ReplicaTaskPriority::Critical
        );
        assert_eq!(
            ReplicaTaskPriority::classify(ReplicaTaskKind::Repair, 2, 3),
            ReplicaTaskPriority::High
        );
        assert_eq!(
            ReplicaTaskPriority::classify(ReplicaTaskKind::Repair, 3, 3),
            ReplicaTaskPriority::Low
        );
        assert_eq!(
            ReplicaTaskPriority::classify(ReplicaTaskKind::Rebalance, 3, 3),
            ReplicaTaskPriority::Low
        );
        assert!(
            ReplicaTaskPriority::Critical.rank() < ReplicaTaskPriority::Low.rank(),
            "critical repair must outrank rebalancing"
        );
    }

    #[test]
    fn leases_expire_and_return_work_to_the_queue() {
        let now = Utc::now();
        let mut task = ReplicaTask::queued(
            ObjectId::new(),
            ReplicaTaskKind::Repair,
            ReplicaTaskPriority::High,
            1_024,
            now,
        );
        let node = NodeId::new();
        task.claim(node, 60, now);
        assert!(!task.lease_expired(now));
        assert!(task.lease_expired(now + TimeDelta::try_seconds(61).expect("delta")));
        task.requeue(Some("lease expired".into()), now);
        assert_eq!(task.state, ReplicaTaskState::Queued);
    }

    #[test]
    fn tasks_park_after_exhausting_attempts() {
        let now = Utc::now();
        let mut task = ReplicaTask::queued(
            ObjectId::new(),
            ReplicaTaskKind::Repair,
            ReplicaTaskPriority::High,
            1,
            now,
        );
        task.fail("target unreachable".into(), 2, now);
        assert_eq!(task.state, ReplicaTaskState::Queued);
        task.fail("target unreachable".into(), 2, now);
        assert!(matches!(task.state, ReplicaTaskState::Parked { .. }));
        assert!(!task.state.active());
    }

    #[test]
    fn movement_budgets_separate_durability_from_balancing() {
        assert_eq!(ReplicaTaskKind::Repair.budget(), MovementBudget::Repair);
        assert_eq!(
            ReplicaTaskKind::Rebalance.budget(),
            MovementBudget::Rebalance
        );
        assert!(ReplicaTaskKind::Drain.removes_source());
        assert!(!ReplicaTaskKind::Repair.removes_source());
    }
}
