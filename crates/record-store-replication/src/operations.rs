//! Administrative cluster operations.
//!
//! Drain, maintenance, resume, and decommission are all idempotent and all
//! validated before they change anything. Decommission in particular refuses to
//! proceed when it would knowingly drop object versions below their required
//! durability, unless an operator explicitly overrides that.

use std::{collections::BTreeSet, sync::Arc};

use chrono::Utc;
use record_store_cluster::{
    ClusterCommand, ClusterOperation, ClusterOperationKind, ClusterOperationState, ClusterTopology,
    DecommissionSafety, DeviceRecord, DeviceState, IssuedJoinToken, JoinToken, NodeState,
    PlacementPolicy, StorageClass, StoragePolicy,
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

    /// Holds every active rebalance without discarding its progress.
    ///
    /// Pausing stops both the planning of new movement and the transfers
    /// already queued. A pause that only stopped planning would keep moving
    /// data for a while, which is not what an operator pressing pause meant.
    pub async fn pause_rebalance(&self) -> Result<usize, OperationError> {
        self.set_rebalance_state(ClusterOperationState::Paused, "paused by an operator")
            .await
    }

    /// Returns paused rebalances to service.
    pub async fn resume_rebalance(&self) -> Result<usize, OperationError> {
        self.set_rebalance_state(ClusterOperationState::Moving, "resumed by an operator")
            .await
    }

    async fn set_rebalance_state(
        &self,
        state: ClusterOperationState,
        message: &str,
    ) -> Result<usize, OperationError> {
        let wanted = if state == ClusterOperationState::Paused {
            ClusterOperationState::Paused
        } else {
            ClusterOperationState::Moving
        };
        let mut changed = 0;
        for operation in self.context.cluster.operations(64).await.map_err(cluster)? {
            if operation.kind != ClusterOperationKind::Rebalance || !operation.state.active() {
                continue;
            }
            // Pausing what is already paused, or resuming what is running, is a
            // no-op rather than an error: an operator repeating a command should
            // not have to care whether it already took effect.
            let already_paused = operation.state == ClusterOperationState::Paused;
            if (wanted == ClusterOperationState::Paused) == already_paused {
                continue;
            }
            self.apply(ClusterCommand::UpdateOperation {
                operation_id: operation.id,
                state: wanted,
                progress: operation.progress,
                message: Some(message.to_owned()),
                at: Utc::now(),
            })
            .await?;
            changed += 1;
        }
        info!(%state, changed, "rebalance state changed");
        Ok(changed)
    }

    /// Sets the byte-per-second ceiling for one rebalance transfer.
    ///
    /// Zero disables throttling. This is cluster configuration rather than a
    /// property of a running operation, so it applies to rebalancing generally
    /// and survives the current one finishing.
    pub async fn throttle_rebalance(&self, bytes_per_second: u64) -> Result<u64, OperationError> {
        let mut config = self
            .context
            .cluster
            .config()
            .await
            .map_err(cluster)?
            .ok_or_else(|| OperationError::Cluster("cluster is not initialized".into()))?;
        config.rebalance.movement.maximum_bytes_per_second = bytes_per_second;
        config
            .validate()
            .map_err(|error| OperationError::Cluster(error.to_string()))?;
        self.apply(ClusterCommand::UpdateConfig {
            config: Box::new(config),
            at: Utc::now(),
        })
        .await?;
        info!(bytes_per_second, "rebalance throttle changed");
        Ok(bytes_per_second)
    }

    /// Simulates a topology change without altering anything.
    ///
    /// The real placement engine is run against a hypothetical cluster map, so
    /// the answer is what would actually happen rather than a model of it. A
    /// bounded sample of committed placements is replayed through both the
    /// current and proposed maps, and the movement is measured rather than
    /// predicted.
    pub async fn simulate(
        &self,
        change: TopologyChange,
        sample_size: usize,
    ) -> Result<SimulationReport, OperationError> {
        let topology = self.context.cluster.topology().await.map_err(cluster)?;
        let proposed = change.apply(&topology)?;

        let sample_size = sample_size.clamp(1, 5_000);
        let page = self
            .context
            .cluster
            .list_placements(None, sample_size)
            .await
            .map_err(cluster)?;

        let engine = record_store_cluster::CapacityAwarePlacement::new(None);
        let policies = self.storage_policies().await?;

        let mut sampled = 0_u64;
        let mut moved = 0_u64;
        let mut moved_bytes = 0_u64;
        let mut unsatisfiable = 0_u64;
        for placement in &page.placements {
            let policy = policies
                .iter()
                .find(|policy| policy.class == placement.storage_class)
                .cloned();
            let request = record_store_cluster::ObjectPlacementRequest::new(
                placement.object_id,
                placement.desired_replicas.max(1),
                1,
                placement.storage_class.clone(),
            )
            .with_size_hint(Some(placement.size))
            .with_policy(policy);

            let before = engine.place(&request, &topology);
            let after = engine.place(&request, &proposed);
            sampled += 1;
            match (before, after) {
                (Ok(before), Ok(after)) => {
                    let was: BTreeSet<_> = before
                        .targets
                        .iter()
                        .map(|target| (target.node_id, target.device_id))
                        .collect();
                    let now: BTreeSet<_> = after
                        .targets
                        .iter()
                        .map(|target| (target.node_id, target.device_id))
                        .collect();
                    if was != now {
                        moved += 1;
                        moved_bytes = moved_bytes.saturating_add(placement.size);
                    }
                }
                // A payload the proposed map cannot place is the important
                // result, not a rounding error in the movement estimate.
                (_, Err(_)) => unsatisfiable += 1,
                (Err(_), Ok(_)) => {}
            }
        }

        let usage = self.context.cluster.usage().await.map_err(cluster)?;
        Ok(SimulationReport {
            change: change.describe(),
            raw_capacity_before: capacity_of(&topology),
            raw_capacity_after: capacity_of(&proposed),
            devices_before: device_count(&topology),
            devices_after: device_count(&proposed),
            placements_total: usage.payloads,
            placements_sampled: sampled,
            placements_moved: moved,
            sampled_bytes_moved: moved_bytes,
            placements_unsatisfiable: unsatisfiable,
            truncated: page.next_object_id.is_some(),
        })
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

/// A hypothetical change to the cluster map.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum TopologyChange {
    /// A new node joining with the given devices.
    AddNode {
        /// Failure-domain labels the node would carry, for example `rack=b`.
        #[serde(default)]
        failure_domain: String,
        /// Storage class its devices would belong to.
        #[serde(default)]
        storage_class: Option<String>,
        /// Usable capacity of each device the node would bring.
        devices: Vec<u64>,
    },
    /// A device added to a node already in the cluster.
    AddDevice {
        /// Node that would gain the device.
        node_id: NodeId,
        /// Usable capacity it would contribute.
        usable_bytes: u64,
        /// Storage class it would belong to. Defaults to the node's.
        #[serde(default)]
        storage_class: Option<String>,
    },
    /// A device leaving the cluster, as a drain or a failure would remove it.
    RemoveDevice {
        /// Node holding the device.
        node_id: NodeId,
        /// Device that would go away.
        device_id: DeviceId,
    },
}

impl TopologyChange {
    /// Returns the proposed cluster map.
    fn apply(&self, topology: &ClusterTopology) -> Result<ClusterTopology, OperationError> {
        let mut nodes = topology.nodes.clone();
        match self {
            Self::AddNode {
                failure_domain,
                storage_class,
                devices,
            } => {
                if devices.is_empty() {
                    return Err(OperationError::Cluster(
                        "a simulated node must bring at least one device".into(),
                    ));
                }
                let template = nodes.first().cloned().ok_or_else(|| {
                    OperationError::Cluster("the cluster has no node to model against".into())
                })?;
                let node_id = NodeId::new();
                let class = storage_class
                    .as_deref()
                    .and_then(|value| StorageClass::new(value).ok())
                    .unwrap_or_else(|| template.storage_class.clone());
                let mut node = template;
                node.node_id = node_id;
                node.state = record_store_cluster::NodeState::Healthy;
                node.storage_class = class.clone();
                node.failure_domain =
                    record_store_cluster::FailureDomain::parse(failure_domain).unwrap_or_default();
                node.devices = devices
                    .iter()
                    .map(|capacity| simulated_device(node_id, &class, *capacity))
                    .collect();
                node.capacity = record_store_cluster::NodeCapacity {
                    total_bytes: devices.iter().copied().sum(),
                    available_bytes: devices.iter().copied().sum(),
                    replica_bytes: 0,
                    temporary_bytes: 0,
                };
                nodes.push(node);
            }
            Self::AddDevice {
                node_id,
                usable_bytes,
                storage_class,
            } => {
                let node = nodes
                    .iter_mut()
                    .find(|node| node.node_id == *node_id)
                    .ok_or(OperationError::NodeNotFound(*node_id))?;
                let class = storage_class
                    .as_deref()
                    .and_then(|value| StorageClass::new(value).ok())
                    .unwrap_or_else(|| node.storage_class.clone());
                node.devices
                    .push(simulated_device(*node_id, &class, *usable_bytes));
            }
            Self::RemoveDevice { node_id, device_id } => {
                let node = nodes
                    .iter_mut()
                    .find(|node| node.node_id == *node_id)
                    .ok_or(OperationError::NodeNotFound(*node_id))?;
                let before = node.devices.len();
                node.devices.retain(|device| device.id != *device_id);
                if node.devices.len() == before {
                    return Err(OperationError::DeviceNotFound {
                        node: *node_id,
                        device: *device_id,
                    });
                }
            }
        }
        Ok(ClusterTopology::at_epoch(
            topology.cluster_id,
            topology.config.clone(),
            nodes,
            topology.epoch.next(),
        ))
    }

    /// Returns an operator-facing description of the change.
    fn describe(&self) -> String {
        match self {
            Self::AddNode { devices, .. } => {
                format!("add a node with {} device(s)", devices.len())
            }
            Self::AddDevice { node_id, .. } => format!("add a device to node {node_id}"),
            Self::RemoveDevice { device_id, .. } => format!("remove device {device_id}"),
        }
    }
}

/// A device that exists only inside a simulation.
fn simulated_device(node_id: NodeId, class: &StorageClass, usable_bytes: u64) -> DeviceRecord {
    let mut device = DeviceRecord::legacy_directory(
        node_id,
        None,
        class.clone(),
        record_store_cluster::DeviceCapacity {
            raw_bytes: usable_bytes,
            usable_bytes,
            allocated_bytes: 0,
            reserved_bytes: 0,
            available_bytes: usable_bytes,
        },
    );
    device.id = DeviceId::new();
    device
}

fn capacity_of(topology: &ClusterTopology) -> u64 {
    topology
        .nodes
        .iter()
        .flat_map(|node| &node.devices)
        .map(|device| device.capacity.usable_bytes)
        .sum()
}

fn device_count(topology: &ClusterTopology) -> u64 {
    topology
        .nodes
        .iter()
        .map(|node| node.devices.len() as u64)
        .sum()
}

/// What a simulated topology change would do.
///
/// Movement is measured over a bounded sample of real placements, never
/// extrapolated into a duration: how long a migration takes depends on
/// bandwidth nobody has told us about.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SimulationReport {
    /// The change that was simulated.
    pub change: String,
    /// Usable capacity across every device today.
    pub raw_capacity_before: u64,
    /// Usable capacity after the change.
    pub raw_capacity_after: u64,
    /// Devices in the cluster today.
    pub devices_before: u64,
    /// Devices after the change.
    pub devices_after: u64,
    /// Payloads the cluster tracks in total.
    pub placements_total: u64,
    /// Payloads actually replayed through both maps.
    pub placements_sampled: u64,
    /// Sampled payloads whose targets would change.
    pub placements_moved: u64,
    /// Bytes belonging to the moved payloads in the sample.
    pub sampled_bytes_moved: u64,
    /// Sampled payloads the proposed map could not place at all.
    pub placements_unsatisfiable: u64,
    /// Whether more placements exist than were sampled.
    pub truncated: bool,
}
