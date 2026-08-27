//! Administrative cluster operations.
//!
//! Drain, maintenance, resume, and decommission are all idempotent and all
//! validated before they change anything. Decommission in particular refuses to
//! proceed when it would knowingly drop object versions below their required
//! durability, unless an operator explicitly overrides that.

use std::sync::Arc;

use chrono::Utc;
use oes_cluster::{
    ClusterCommand, ClusterOperation, ClusterOperationKind, ClusterOperationState,
    DecommissionSafety, IssuedJoinToken, JoinToken, NodeState,
};
use oes_consensus::{ClusterWrite, MetadataConsensus};
use oes_core::{ClusterOperationId, NodeId};
use thiserror::Error;
use tracing::info;

use crate::{context::ClusterContext, coordinator::Coordinator};

/// Failures raised by administrative cluster operations.
#[derive(Debug, Error)]
pub enum OperationError {
    /// The node is unknown.
    #[error("node {0} is not a member of this cluster")]
    NodeNotFound(NodeId),
    /// The requested lifecycle transition is not allowed.
    #[error("node {node} cannot move from {from} to {to}")]
    InvalidTransition {
        /// Node being changed.
        node: NodeId,
        /// Current state.
        from: NodeState,
        /// Requested state.
        to: NodeState,
    },
    /// The operation would violate durability.
    #[error("{0}")]
    DurabilityAtRisk(String),
    /// Cluster state could not be read or written.
    #[error("cluster operation failed: {0}")]
    Cluster(String),
}

/// Administrative cluster operations for one node's management API.
pub struct ClusterOperations {
    context: Arc<ClusterContext>,
    coordinator: Arc<Coordinator>,
    consensus: Arc<MetadataConsensus>,
}

impl ClusterOperations {
    /// Creates the operation surface.
    #[must_use]
    pub const fn new(
        context: Arc<ClusterContext>,
        coordinator: Arc<Coordinator>,
        consensus: Arc<MetadataConsensus>,
    ) -> Self {
        Self {
            context,
            coordinator,
            consensus,
        }
    }

    /// Starts or re-reports a drain.
    ///
    /// Draining stops new placement on the node and moves its replicas
    /// elsewhere. Repeating the request returns the existing operation instead of
    /// creating a second one.
    pub async fn drain(&self, node_id: NodeId) -> Result<ClusterOperation, OperationError> {
        self.transition(node_id, NodeState::Draining, "administratively draining")
            .await?;
        self.ensure_operation(node_id, ClusterOperationKind::Drain)
            .await
    }

    /// Places a node into maintenance.
    ///
    /// Maintenance keeps the node's data and stops new placement. Whether it
    /// keeps serving reads is a cluster-wide setting, so the behaviour is
    /// explicit rather than implied.
    pub async fn maintenance(&self, node_id: NodeId) -> Result<(), OperationError> {
        self.transition(
            node_id,
            NodeState::Maintenance,
            "administratively in maintenance",
        )
        .await
    }

    /// Returns a drained or maintained node to service.
    pub async fn resume(&self, node_id: NodeId) -> Result<(), OperationError> {
        let node = self.node(node_id).await?;
        if node.state == NodeState::Decommissioned {
            return Err(OperationError::InvalidTransition {
                node: node_id,
                from: node.state,
                to: NodeState::Healthy,
            });
        }
        self.transition(node_id, NodeState::Healthy, "administratively resumed")
            .await?;
        // An active drain for the node is no longer wanted.
        for operation in self.context.cluster.operations(64).await.map_err(cluster)? {
            if operation.node_id == Some(node_id)
                && operation.kind == ClusterOperationKind::Drain
                && operation.state.active()
            {
                self.apply(ClusterCommand::UpdateOperation {
                    operation_id: operation.id,
                    state: ClusterOperationState::Cancelled,
                    progress: operation.progress,
                    message: Some("node resumed".into()),
                    at: Utc::now(),
                })
                .await?;
            }
        }
        Ok(())
    }

    /// Checks whether a node can be removed without losing durability.
    pub async fn decommission_safety(
        &self,
        node_id: NodeId,
    ) -> Result<DecommissionSafety, OperationError> {
        self.node(node_id).await?;
        self.coordinator
            .decommission_safety(node_id)
            .await
            .map_err(OperationError::Cluster)
    }

    /// Permanently removes a node.
    ///
    /// The safety check is mandatory unless the operator explicitly forces the
    /// operation, because a mistyped node name must not be able to destroy
    /// durability.
    pub async fn decommission(
        &self,
        node_id: NodeId,
        force: bool,
    ) -> Result<ClusterOperation, OperationError> {
        let safety = self.decommission_safety(node_id).await?;
        if !safety.safe && !force {
            return Err(OperationError::DurabilityAtRisk(format!(
                "cannot decommission node {node_id}. Reason: {}. Drain the node first, or pass \
                 an explicit force acknowledgement to accept the loss.",
                safety.reason
            )));
        }
        if safety.replicas_remaining > 0 {
            // Removal still moves the data first; forcing only bypasses the
            // durability objection, never the movement itself.
            self.transition(node_id, NodeState::Draining, "decommission in progress")
                .await?;
        } else {
            self.transition(
                node_id,
                NodeState::Decommissioned,
                "decommissioned with no replicas remaining",
            )
            .await?;
        }
        let operation = self
            .ensure_operation(node_id, ClusterOperationKind::Decommission)
            .await?;
        info!(
            node = %node_id,
            forced = force,
            replicas = safety.replicas_remaining,
            "decommission started"
        );
        Ok(operation)
    }

    /// Starts a rebalance and returns its operation record.
    pub async fn rebalance(&self) -> Result<ClusterOperation, OperationError> {
        let operation =
            ClusterOperation::planning(ClusterOperationKind::Rebalance, None, Utc::now());
        self.apply(ClusterCommand::StartOperation {
            operation: Box::new(operation.clone()),
        })
        .await?;
        let planned = self
            .coordinator
            .rebalance(Some(operation.id))
            .await
            .map_err(OperationError::Cluster)?;
        self.apply(ClusterCommand::UpdateOperation {
            operation_id: operation.id,
            state: if planned == 0 {
                ClusterOperationState::Completed
            } else {
                ClusterOperationState::Moving
            },
            progress: oes_cluster::OperationProgress {
                objects_remaining: u64::try_from(planned).unwrap_or(0),
                ..oes_cluster::OperationProgress::default()
            },
            message: Some(format!("{planned} movement(s) planned")),
            at: Utc::now(),
        })
        .await?;
        self.context
            .cluster
            .operation(operation.id)
            .await
            .map_err(cluster)?
            .ok_or_else(|| OperationError::Cluster("operation record disappeared".into()))
    }

    /// Issues a single-use join token.
    pub async fn issue_join_token(
        &self,
        lifetime_seconds: u64,
        description: String,
    ) -> Result<IssuedJoinToken, OperationError> {
        let issued = JoinToken::issue(lifetime_seconds, 1, description, Utc::now());
        self.apply(ClusterCommand::IssueJoinToken {
            token: Box::new(issued.record.clone()),
        })
        .await?;
        Ok(issued)
    }

    /// Revokes a join token.
    pub async fn revoke_join_token(
        &self,
        token_id: oes_core::JoinTokenId,
    ) -> Result<(), OperationError> {
        self.apply(ClusterCommand::RevokeJoinToken {
            token_id,
            at: Utc::now(),
        })
        .await
    }

    /// Replaces the cluster-wide configuration after strict validation.
    pub async fn set_config(
        &self,
        config: oes_cluster::ClusterConfig,
    ) -> Result<(), OperationError> {
        config
            .validate()
            .map_err(|error| OperationError::Cluster(error.to_string()))?;
        self.apply(ClusterCommand::UpdateConfig {
            config: Box::new(config),
            at: Utc::now(),
        })
        .await
    }

    /// Requests an immediate metadata snapshot.
    pub async fn snapshot_metadata(&self) -> Result<(), OperationError> {
        self.consensus
            .trigger_snapshot()
            .await
            .map_err(|error| OperationError::Cluster(error.to_string()))
    }

    async fn ensure_operation(
        &self,
        node_id: NodeId,
        kind: ClusterOperationKind,
    ) -> Result<ClusterOperation, OperationError> {
        for operation in self.context.cluster.operations(64).await.map_err(cluster)? {
            if operation.node_id == Some(node_id)
                && operation.kind == kind
                && operation.state.active()
            {
                return Ok(operation);
            }
        }
        let operation = ClusterOperation::planning(kind, Some(node_id), Utc::now());
        self.apply(ClusterCommand::StartOperation {
            operation: Box::new(operation.clone()),
        })
        .await?;
        Ok(operation)
    }

    async fn transition(
        &self,
        node_id: NodeId,
        state: NodeState,
        reason: &str,
    ) -> Result<(), OperationError> {
        let node = self.node(node_id).await?;
        if node.state == state {
            return Ok(());
        }
        if !node.state.can_transition_to(state) {
            return Err(OperationError::InvalidTransition {
                node: node_id,
                from: node.state,
                to: state,
            });
        }
        self.apply(ClusterCommand::SetNodeState {
            node_id,
            state,
            reason: Some(reason.to_owned()),
            at: Utc::now(),
        })
        .await
    }

    async fn node(&self, node_id: NodeId) -> Result<oes_cluster::NodeRecord, OperationError> {
        self.context
            .cluster
            .node(node_id)
            .await
            .map_err(cluster)?
            .ok_or(OperationError::NodeNotFound(node_id))
    }

    async fn apply(&self, command: ClusterCommand) -> Result<(), OperationError> {
        self.context
            .commit(ClusterWrite::cluster(command))
            .await
            .map_err(|error| OperationError::Cluster(error.to_string()))
    }
}

fn cluster<E: std::fmt::Display>(error: E) -> OperationError {
    OperationError::Cluster(error.to_string())
}

/// Returns the operation identifier callers should quote in follow-up requests.
#[must_use]
pub const fn operation_id(operation: &ClusterOperation) -> ClusterOperationId {
    operation.id
}
