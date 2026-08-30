//! Administrative cluster operations.
//!
//! Drain, maintenance, resume, and decommission are all idempotent and all
//! validated before they change anything. Decommission in particular refuses to
//! proceed when it would knowingly drop object versions below their required
//! durability, unless an operator explicitly overrides that.

use std::sync::Arc;

use chrono::Utc;
use record_store_cluster::{
    ClusterCommand, ClusterOperation, ClusterOperationKind, ClusterOperationState,
    DecommissionSafety, DeviceRecord, DeviceState, IssuedJoinToken, JoinToken, NodeState,
    StorageClass, StoragePolicy,
};
use record_store_consensus::{ClusterWrite, MetadataConsensus};
use record_store_core::{ClusterOperationId, DeviceId, NodeId};
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
    /// The device is unknown on the named node.
    #[error("device {device} is not registered on node {node}")]
    DeviceNotFound {
        /// Node the device was looked for on.
        node: NodeId,
        /// Device that is not registered.
        device: DeviceId,
    },
    /// The requested device lifecycle transition is not allowed.
    #[error("device {device} cannot move from {from} to {to}")]
    InvalidDeviceTransition {
        /// Device being changed.
        device: DeviceId,
        /// Current state.
        from: DeviceState,
        /// Requested state.
        to: DeviceState,
    },
    /// The storage policy is unknown.
    #[error("storage class '{0}' is not defined")]
    StoragePolicyNotFound(StorageClass),
    /// The policy cannot be removed while devices still carry its class.
    #[error(
        "storage class '{class}' is still assigned to {devices} device(s); \
         reassign them before removing the policy"
    )]
    StoragePolicyInUse {
        /// Class that was asked to be removed.
        class: StorageClass,
        /// Devices still carrying it.
        devices: usize,
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

    /// Explains where an object would be placed, and why.
    ///
    /// A read-only dry run against committed state. Placement is a pure
    /// function of the object identity, the policy, and the cluster map, so
    /// explaining a placement never touches data or changes anything.
    pub async fn explain_placement(
        &self,
        bucket: &record_store_core::BucketName,
        key: &record_store_core::ObjectKey,
    ) -> Result<record_store_cluster::PlacementExplanation, OperationError> {
        let bucket_record = self
            .context
            .metadata
            .get_bucket_by_name(bucket)
            .await
            .map_err(|error| OperationError::Cluster(error.to_string()))?
            .ok_or_else(|| OperationError::Cluster(format!("bucket '{bucket}' does not exist")))?;
        let class = bucket_record.storage_class.clone().unwrap_or_default();
        let policy = self.storage_policy(&class).await?;

        // An existing object is explained where it actually lives; one that does
        // not exist yet is explained as the write that would create it.
        let object = self
            .context
            .metadata
            .get_object(bucket_record.id, key)
            .await
            .map_err(|error| OperationError::Cluster(error.to_string()))?;
        let (object_id, size_hint) = match &object {
            Some(metadata) => (metadata.id, Some(metadata.size)),
            None => (record_store_core::ObjectId::new(), None),
        };

        let topology = self.context.cluster.topology().await.map_err(cluster)?;
        let config = self
            .context
            .cluster
            .config()
            .await
            .map_err(cluster)?
            .ok_or_else(|| OperationError::Cluster("cluster is not initialized".into()))?;
        let replicas = policy
            .durability
            .replicas()
            .unwrap_or(config.replication_factor);
        let request = record_store_cluster::ObjectPlacementRequest::new(
            object_id,
            replicas,
            config.required_acknowledgements().min(replicas).max(1),
            class,
        )
        .with_policy(Some(policy))
        .with_size_hint(size_hint);

        record_store_cluster::CapacityAwarePlacement::new(Some(self.context.node_id))
            .explain(&request, &topology)
            .map_err(|error| OperationError::Cluster(error.to_string()))
    }

    /// Returns every defined storage policy.
    pub async fn storage_policies(&self) -> Result<Vec<StoragePolicy>, OperationError> {
        self.context
            .cluster
            .storage_policies()
            .await
            .map_err(cluster)
    }

    /// Returns one storage policy.
    pub async fn storage_policy(
        &self,
        class: &StorageClass,
    ) -> Result<StoragePolicy, OperationError> {
        self.storage_policies()
            .await?
            .into_iter()
            .find(|policy| &policy.class == class)
            .ok_or_else(|| OperationError::StoragePolicyNotFound(class.clone()))
    }

    /// Defines or replaces a storage policy.
    ///
    /// Validation happens here and again in the catalog. Doing it twice is
    /// deliberate: the caller gets a useful error, and consensus stays the
    /// authority regardless of which node accepted the request.
    pub async fn put_storage_policy(
        &self,
        policy: StoragePolicy,
    ) -> Result<StoragePolicy, OperationError> {
        policy
            .validate()
            .map_err(|error| OperationError::Cluster(error.to_string()))?;
        self.apply(ClusterCommand::PutStoragePolicy {
            policy: Box::new(policy.clone()),
            at: Utc::now(),
        })
        .await?;
        info!(class = %policy.class, "storage policy committed");
        Ok(policy)
    }

    /// Removes a storage policy.
    ///
    /// Refused while any device still carries the class. Those devices would
    /// otherwise resolve to no policy at all and quietly stop being placement
    /// candidates, which looks like capacity vanishing.
    pub async fn delete_storage_policy(&self, class: &StorageClass) -> Result<(), OperationError> {
        self.storage_policy(class).await?;
        let assigned = self
            .devices()
            .await?
            .into_iter()
            .filter(|(_, device)| &device.storage_class == class)
            .count();
        if assigned > 0 {
            return Err(OperationError::StoragePolicyInUse {
                class: class.clone(),
                devices: assigned,
            });
        }
        self.apply(ClusterCommand::DeleteStoragePolicy {
            class: class.clone(),
            at: Utc::now(),
        })
        .await?;
        info!(%class, "storage policy removed");
        Ok(())
    }

    /// Returns every registered device in the cluster, in stable order.
    pub async fn devices(&self) -> Result<Vec<(NodeId, DeviceRecord)>, OperationError> {
        let mut devices = Vec::new();
        for node in self.context.cluster.nodes().await.map_err(cluster)? {
            for device in &node.devices {
                devices.push((node.node_id, device.clone()));
            }
        }
        devices.sort_by_key(|(node_id, device)| (*node_id, device.id));
        Ok(devices)
    }

    /// Returns one registered device.
    pub async fn device(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
    ) -> Result<DeviceRecord, OperationError> {
        self.node(node_id)
            .await?
            .devices
            .into_iter()
            .find(|device| device.id == device_id)
            .ok_or(OperationError::DeviceNotFound {
                node: node_id,
                device: device_id,
            })
    }

    /// Brings a registered device into service.
    pub async fn activate_device(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
    ) -> Result<DeviceRecord, OperationError> {
        self.transition_device(node_id, device_id, DeviceState::Active)
            .await
    }

    /// Starts draining a device.
    ///
    /// Draining stops new placement on the device and lets the coordinator move
    /// its replicas elsewhere. It does not itself declare the device safe to
    /// remove; only completed evacuation does that.
    pub async fn drain_device(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
    ) -> Result<DeviceRecord, OperationError> {
        self.transition_device(node_id, device_id, DeviceState::Draining)
            .await
    }

    /// Pauses a device without evacuating it.
    pub async fn maintain_device(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
    ) -> Result<DeviceRecord, OperationError> {
        self.transition_device(node_id, device_id, DeviceState::Maintenance)
            .await
    }

    /// Returns a drained or maintained device to service.
    pub async fn resume_device(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
    ) -> Result<DeviceRecord, OperationError> {
        self.transition_device(node_id, device_id, DeviceState::Active)
            .await
    }

    /// Marks an evacuated device safe to remove.
    ///
    /// The catalog refuses this while the device still owns replica records, so
    /// "safe to remove" always means evacuation actually finished rather than
    /// that an operator asked for it.
    pub async fn release_device(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
    ) -> Result<DeviceRecord, OperationError> {
        self.transition_device(node_id, device_id, DeviceState::SafeToRemove)
            .await
    }

    /// Permanently retires a device.
    pub async fn retire_device(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
    ) -> Result<DeviceRecord, OperationError> {
        self.transition_device(node_id, device_id, DeviceState::Retired)
            .await
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
            progress: record_store_cluster::OperationProgress {
                objects_remaining: u64::try_from(planned).unwrap_or(0),
                ..record_store_cluster::OperationProgress::default()
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
        token_id: record_store_core::JoinTokenId,
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
        config: record_store_cluster::ClusterConfig,
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

    /// Validates a device lifecycle change before committing it.
    ///
    /// The transition table lives on `DeviceState`, so an unsafe move is
    /// rejected here rather than reaching consensus and being rejected there
    /// with a less useful error.
    async fn transition_device(
        &self,
        node_id: NodeId,
        device_id: DeviceId,
        state: DeviceState,
    ) -> Result<DeviceRecord, OperationError> {
        let device = self.device(node_id, device_id).await?;
        if device.state == state {
            return Ok(device);
        }
        if !device.state.can_transition_to(state) {
            return Err(OperationError::InvalidDeviceTransition {
                device: device_id,
                from: device.state,
                to: state,
            });
        }
        self.apply(ClusterCommand::SetDeviceState {
            node_id,
            device_id,
            state,
            at: Utc::now(),
        })
        .await?;
        info!(%node_id, %device_id, %state, "device lifecycle changed");
        self.device(node_id, device_id).await
    }

    async fn node(
        &self,
        node_id: NodeId,
    ) -> Result<record_store_cluster::NodeRecord, OperationError> {
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
