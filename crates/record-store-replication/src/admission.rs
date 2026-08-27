//! Cluster admission.
//!
//! Joining is a deliberate, auditable act: a node presents a single-use token,
//! the cluster records it as a member through consensus, issues it a credential
//! of its own, and only then adds it to the metadata group. A node that already
//! belongs to a different cluster is refused rather than silently rebound.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use record_store_cluster::{
    ClusterCommand, ClusterOutcome, NodeCredential, NodeIdentityStore, NodeVersions,
    check_compatibility, parse_join_token,
};
use record_store_consensus::{ClusterWrite, ConsensusError, MetadataConsensus, rejection_error};
use record_store_core::NodeId;
use record_store_protocol::system_v1::NodeDescriptor;
use record_store_rpc::{ClusterAdmission, JoinOutcome, NodeJoinRequest};
use tracing::{info, warn};

use crate::context::ClusterContext;

/// Grants or refuses cluster membership.
pub struct JoinCoordinator {
    context: Arc<ClusterContext>,
    consensus: Arc<MetadataConsensus>,
    local: NodeVersions,
    storage_node: bool,
    advertise_address: String,
}

impl JoinCoordinator {
    /// Creates a coordinator for one node.
    #[must_use]
    pub fn new(
        context: Arc<ClusterContext>,
        consensus: Arc<MetadataConsensus>,
        local: NodeVersions,
        storage_node: bool,
        advertise_address: String,
    ) -> Self {
        Self {
            context,
            consensus,
            local,
            storage_node,
            advertise_address,
        }
    }
}

#[async_trait]
impl ClusterAdmission for JoinCoordinator {
    async fn join(&self, request: NodeJoinRequest) -> Result<JoinOutcome, ConsensusError> {
        check_compatibility(&self.local, &request.versions).map_err(|error| {
            rejection_error(
                record_store_consensus::RejectionKind::InvalidConfiguration,
                error.to_string(),
            )
        })?;
        let identity = self
            .context
            .cluster
            .identity()
            .await
            .map_err(|error| ConsensusError::Internal(error.to_string()))?
            .ok_or_else(|| {
                rejection_error(
                    record_store_consensus::RejectionKind::ClusterNotInitialized,
                    "this cluster has not been initialized",
                )
            })?;

        let token_id = parse_join_token(&request.join_token).map_err(|error| {
            rejection_error(
                record_store_consensus::RejectionKind::JoinTokenNotFound,
                error.to_string(),
            )
        })?;
        let token = self
            .context
            .cluster
            .join_token(token_id)
            .await
            .map_err(|error| ConsensusError::Internal(error.to_string()))?
            .ok_or_else(|| {
                rejection_error(
                    record_store_consensus::RejectionKind::JoinTokenNotFound,
                    "the join token is not recognized",
                )
            })?;
        let now = Utc::now();
        token.verify(&request.join_token, now).map_err(|error| {
            rejection_error(
                record_store_consensus::RejectionKind::JoinTokenNotFound,
                error.to_string(),
            )
        })?;

        let node_id = request.registration.node_id;
        let issued = NodeCredential::issue(node_id, now);
        // The token consumption, the membership record, and the node credential
        // are committed together, so a partially joined node cannot exist.
        let response = self
            .consensus
            .write(ClusterWrite::batch([
                ClusterWrite::cluster(ClusterCommand::ConsumeJoinToken { token_id, at: now }),
                ClusterWrite::cluster(ClusterCommand::RegisterNode {
                    registration: Box::new(request.registration),
                    at: now,
                }),
                ClusterWrite::cluster(ClusterCommand::PutNodeCredential {
                    credential: Box::new(issued.record.clone()),
                }),
            ]))
            .await?;
        let responses = response.into_batch().map_err(ConsensusError::Rejected)?;
        let registration = responses
            .into_iter()
            .nth(1)
            .ok_or_else(|| ConsensusError::Internal("registration response missing".into()))?
            .into_cluster()
            .map_err(ConsensusError::Rejected)?;
        let ClusterOutcome::Registration {
            record, raft_id, ..
        } = registration
        else {
            return Err(ConsensusError::Internal(
                "registration did not return a member identifier".into(),
            ));
        };
        let config = self
            .context
            .cluster
            .config()
            .await
            .map_err(|error| ConsensusError::Internal(error.to_string()))?
            .ok_or_else(|| {
                rejection_error(
                    record_store_consensus::RejectionKind::ClusterNotInitialized,
                    "cluster configuration is missing",
                )
            })?;
        info!(
            node = %node_id,
            member = raft_id,
            voter = record.metadata_voter,
            "granted cluster membership"
        );
        Ok(JoinOutcome {
            cluster_id: identity.cluster_id,
            member_id: raft_id,
            node_credential: issued.secret.expose().to_owned(),
            metadata_voter: record.metadata_voter,
            cluster_config: config,
        })
    }

    async fn activate(
        &self,
        node_id: NodeId,
        member_id: u64,
        address: String,
        _voter: bool,
    ) -> Result<bool, ConsensusError> {
        let record = self
            .context
            .cluster
            .node(node_id)
            .await
            .map_err(|error| ConsensusError::Internal(error.to_string()))?
            .ok_or_else(|| {
                rejection_error(
                    record_store_consensus::RejectionKind::NodeNotFound,
                    format!("node {node_id} is not a cluster member"),
                )
            })?;
        if record.raft_id != member_id {
            return Err(rejection_error(
                record_store_consensus::RejectionKind::NodeNotFound,
                format!(
                    "node {node_id} is consensus member {} and cannot activate as member {member_id}",
                    record.raft_id
                ),
            ));
        }
        // Adding a member is idempotent: a node that retries activation after a
        // restart is already present and the call succeeds unchanged.
        self.consensus
            .add_member(member_id, address, record.metadata_voter)
            .await?;
        info!(node = %node_id, member = member_id, "activated cluster membership");
        Ok(true)
    }

    async fn descriptor(&self) -> NodeDescriptor {
        let cluster_id = self
            .context
            .cluster
            .identity()
            .await
            .ok()
            .flatten()
            .map(|identity| identity.cluster_id.to_string())
            .unwrap_or_default();
        NodeDescriptor {
            node_id: self.context.node_id.to_string(),
            member_id: self.consensus.member_id(),
            protocol_major_version: self.local.protocol.major,
            protocol_minor_version: self.local.protocol.minor,
            software_version: self.local.software.clone(),
            storage_format_version: self.local.storage_format,
            cluster_format_version: self.local.cluster_format,
            cluster_id,
            rpc_address: self.advertise_address.clone(),
            storage_node: self.storage_node,
        }
    }

    async fn metadata_leader(&self) -> Option<(u64, String)> {
        self.consensus.current_leader().await
    }
}

/// Persists a granted membership into this node's durable identity.
///
/// Binding fails if the node already belongs to another cluster, which is what
/// stops stale local data from being presented as a new member's state.
pub fn bind_identity(
    store: &NodeIdentityStore,
    cluster_id: record_store_core::ClusterId,
    member_id: u64,
) -> Result<(), String> {
    store
        .bind(cluster_id, member_id, Utc::now())
        .map(|_| ())
        .map_err(|error| {
            warn!(%error, "cluster binding refused");
            error.to_string()
        })
}

/// The admission service handed to the internal RPC listener.
pub type ClusterAdmissionService = Arc<dyn ClusterAdmission>;
