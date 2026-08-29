//! Failure detection and cluster health classification.
//!
//! Detection is deliberately conservative. A single missed heartbeat never marks
//! a node dead, and administrative states are never overridden, because both
//! mistakes cause recovery storms that damage a cluster more than the original
//! fault did.

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    config::{ClusterConfig, FailureDetectionPolicy},
    topology::{ClusterTopology, NodeRecord, NodeState},
};

/// A proposed lifecycle transition produced by failure detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthTransition {
    /// State the node should move to.
    pub state: NodeState,
    /// Operator-facing explanation.
    pub reason: String,
}

/// Evaluates one node against the failure-detection policy.
///
/// Returns `None` when the node should keep its current state.
#[must_use]
pub fn evaluate_node(
    node: &NodeRecord,
    policy: &FailureDetectionPolicy,
    now: DateTime<Utc>,
) -> Option<HealthTransition> {
    if node.state.is_administrative() {
        // Draining, maintenance, and decommissioned are operator decisions.
        return None;
    }
    let last_seen = node.last_heartbeat_at.unwrap_or(node.joined_at);
    let silence = now.signed_duration_since(last_seen);
    let suspect_after = seconds(policy.suspect_timeout_seconds);
    let offline_after = seconds(policy.offline_timeout_seconds);

    if silence >= offline_after {
        return transition(
            node,
            NodeState::Offline,
            format!(
                "no heartbeat for {}s, exceeding the {}s offline timeout",
                silence.num_seconds().max(0),
                policy.offline_timeout_seconds
            ),
        );
    }
    if silence >= suspect_after {
        // A node already known to be unreachable keeps that more specific state
        // until it either recovers or crosses the offline timeout.
        if node.state == NodeState::Unreachable {
            return None;
        }
        return transition(
            node,
            NodeState::Suspect,
            format!(
                "no heartbeat for {}s, exceeding the {}s suspect timeout",
                silence.num_seconds().max(0),
                policy.suspect_timeout_seconds
            ),
        );
    }
    transition(
        node,
        NodeState::Healthy,
        "heartbeats are current".to_owned(),
    )
}

fn transition(node: &NodeRecord, state: NodeState, reason: String) -> Option<HealthTransition> {
    if node.state == state {
        return None;
    }
    if !node.state.can_transition_to(state) {
        return None;
    }
    Some(HealthTransition { state, reason })
}

fn seconds(value: u64) -> TimeDelta {
    TimeDelta::try_seconds(i64::try_from(value).unwrap_or(i64::MAX)).unwrap_or_else(TimeDelta::zero)
}

/// Overall cluster health, reported separately for data and metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealth {
    /// Every dimension is nominal.
    Healthy,
    /// Reduced redundancy, but reads and writes still meet their contracts.
    Degraded,
    /// Durability or availability guarantees are actively violated.
    Critical,
    /// The cluster cannot serve its core contract.
    Unavailable,
}

impl ClusterHealth {
    /// Returns the worse of two health values.
    #[must_use]
    pub fn worst(self, other: Self) -> Self {
        self.max(other)
    }
}

/// Health of the consensus group backing cluster metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuorumStatus {
    /// Voting members configured.
    pub members: u32,
    /// Voting members currently reachable, when this member can observe that.
    ///
    /// Only the leader tracks replication contact with its peers. A follower
    /// reports `None` rather than a count it cannot know.
    pub healthy_members: Option<u32>,
    /// Members required for a quorum.
    pub quorum: u32,
    /// Current leader, when one is known.
    pub leader: Option<String>,
    /// Whether metadata writes can currently be committed.
    pub writable: bool,
    /// Whether this member's applied state is usable for reads.
    pub readable: bool,
    /// Derived health classification.
    pub health: ClusterHealth,
    /// Whether the deployment has enough voters for fault tolerance.
    pub fault_tolerant: bool,
    /// Operator-facing notes explaining the classification.
    pub notes: Vec<String>,
}

impl QuorumStatus {
    /// Classifies a consensus group from observed member counts.
    ///
    /// `healthy_members` is `None` when the caller cannot observe peer
    /// reachability, which is every member except the leader. In that case the
    /// classification rests on leadership instead: Raft cannot hold a leader
    /// without a majority, so a member that can see one knows a quorum exists.
    /// Counting unobserved peers as unreachable would report a healthy cluster
    /// as degraded from any follower.
    #[must_use]
    pub fn evaluate(members: u32, healthy_members: Option<u32>, leader: Option<String>) -> Self {
        let quorum = members / 2 + 1;
        let writable = match healthy_members {
            Some(healthy) => leader.is_some() && healthy >= quorum && members > 0,
            None => leader.is_some() && members > 0,
        };
        let fault_tolerant = members >= 3;
        let mut notes = Vec::new();
        let health = if members == 0 {
            notes.push("no metadata members are registered".into());
            ClusterHealth::Unavailable
        } else if !writable {
            notes.push(match healthy_members {
                Some(healthy) => format!(
                    "metadata quorum lost: {healthy} of {members} members reachable, \
                     {quorum} required; metadata operations are read-only or unavailable"
                ),
                None => "metadata quorum lost: no leader is known to this member; \
                         metadata operations are read-only or unavailable"
                    .to_owned(),
            });
            ClusterHealth::Unavailable
        } else if healthy_members.is_some_and(|healthy| healthy < members) {
            let healthy = healthy_members.unwrap_or_default();
            notes.push(format!(
                "metadata quorum degraded: {healthy} of {members} members reachable"
            ));
            ClusterHealth::Degraded
        } else {
            if healthy_members.is_none() {
                notes.push(
                    "peer reachability is only observable on the leader; this member reports \
                     the quorum implied by the leader it can see"
                        .to_owned(),
                );
            }
            ClusterHealth::Healthy
        };
        if !fault_tolerant && members > 0 {
            notes.push(format!(
                "{members} metadata member(s) configured: at least 3 are required to survive \
                 the loss of one member"
            ));
        }
        Self {
            members,
            healthy_members,
            quorum,
            leader,
            writable,
            readable: members > 0,
            health,
            fault_tolerant,
            notes,
        }
    }
}

/// Data-plane health derived from node states and replica accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataHealth {
    /// Non-terminal members.
    pub nodes: u32,
    /// Members in the `Healthy` state.
    pub healthy_nodes: u32,
    /// Members that stopped counting towards durability.
    pub unavailable_nodes: u32,
    /// Payloads with fewer healthy replicas than desired.
    pub under_replicated_payloads: u64,
    /// Payloads with no healthy replica at all.
    pub unavailable_payloads: u64,
    /// Whether new writes can currently reach their durability requirement.
    pub writable: bool,
    /// Derived health classification.
    pub health: ClusterHealth,
    /// Operator-facing notes explaining the classification.
    pub notes: Vec<String>,
}

impl DataHealth {
    /// Classifies data-plane health.
    #[must_use]
    pub fn evaluate(
        topology: &ClusterTopology,
        under_replicated_payloads: u64,
        unavailable_payloads: u64,
    ) -> Self {
        let config: &ClusterConfig = &topology.config;
        let nodes = u32::try_from(topology.members().count()).unwrap_or(u32::MAX);
        let healthy_nodes = u32::try_from(topology.healthy().count()).unwrap_or(u32::MAX);
        let placeable = u32::try_from(topology.placeable().count()).unwrap_or(u32::MAX);
        let unavailable_nodes = u32::try_from(
            topology
                .members()
                .filter(|node| !node.state.contributes_durability())
                .count(),
        )
        .unwrap_or(u32::MAX);
        let required = u32::from(config.required_acknowledgements());
        let writable = placeable >= required;
        let mut notes = Vec::new();
        let mut health = ClusterHealth::Healthy;
        if !writable {
            notes.push(format!(
                "writes are refused: {placeable} node(s) can accept replicas, \
                 {required} acknowledgement(s) are required"
            ));
            health = health.worst(ClusterHealth::Critical);
        }
        if unavailable_payloads > 0 {
            notes.push(format!(
                "{unavailable_payloads} payload(s) have no healthy replica and cannot be read"
            ));
            health = health.worst(ClusterHealth::Critical);
        }
        if under_replicated_payloads > 0 {
            notes.push(format!(
                "{under_replicated_payloads} payload(s) are under-replicated and queued for repair"
            ));
            health = health.worst(ClusterHealth::Degraded);
        }
        if healthy_nodes < nodes {
            notes.push(format!(
                "{} of {nodes} node(s) are not healthy",
                nodes.saturating_sub(healthy_nodes)
            ));
            health = health.worst(ClusterHealth::Degraded);
        }
        if placeable < u32::from(config.replication_factor) && writable {
            notes.push(format!(
                "only {placeable} node(s) can accept replicas: new writes land \
                 under-replicated at the configured factor of {}",
                config.replication_factor
            ));
            health = health.worst(ClusterHealth::Degraded);
        }
        Self {
            nodes,
            healthy_nodes,
            unavailable_nodes,
            under_replicated_payloads,
            unavailable_payloads,
            writable,
            health,
            notes,
        }
    }
}

/// Readiness of one node, modelled separately from binary liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    /// Fully able to serve every operation it is responsible for.
    Ready,
    /// Able to serve a reduced set of operations.
    Degraded,
    /// Not able to serve traffic.
    Unavailable,
}

#[cfg(test)]
mod tests {
    use record_store_core::{ClusterId, NodeId};

    use super::*;
    use crate::{
        topology::{FailureDomain, NodeActivity, NodeCapacity, StorageClass},
        version::ProtocolVersion,
    };

    fn node(state: NodeState, last_heartbeat: Option<DateTime<Utc>>) -> NodeRecord {
        let now = Utc::now();
        NodeRecord {
            node_id: NodeId::new(),
            raft_id: 1,
            protocol: ProtocolVersion::current(),
            software_version: "test".into(),
            storage_format_version: 1,
            rpc_address: "127.0.0.1:7603".into(),
            s3_endpoint: None,
            storage_class: StorageClass::default(),
            failure_domain: FailureDomain::default(),
            state,
            metadata_voter: true,
            capacity: NodeCapacity {
                total_bytes: 1_000,
                available_bytes: 900,
                replica_bytes: 100,
                temporary_bytes: 0,
            },
            activity: NodeActivity::default(),
            joined_at: now,
            started_at: now,
            last_heartbeat_at: last_heartbeat,
            state_changed_at: now,
            state_reason: None,
        }
    }

    #[test]
    fn a_single_late_heartbeat_does_not_mark_a_node_offline() {
        let policy = FailureDetectionPolicy::default();
        let now = Utc::now();
        let late = now - TimeDelta::try_seconds(25).expect("delta");
        let record = node(NodeState::Healthy, Some(late));
        let transition = evaluate_node(&record, &policy, now).expect("suspect transition");
        assert_eq!(transition.state, NodeState::Suspect);
    }

    #[test]
    fn prolonged_silence_moves_a_node_offline() {
        let policy = FailureDetectionPolicy::default();
        let now = Utc::now();
        let silent = now - TimeDelta::try_seconds(600).expect("delta");
        let record = node(NodeState::Suspect, Some(silent));
        let transition = evaluate_node(&record, &policy, now).expect("offline transition");
        assert_eq!(transition.state, NodeState::Offline);
    }

    #[test]
    fn recovering_heartbeats_restore_health() {
        let policy = FailureDetectionPolicy::default();
        let now = Utc::now();
        let record = node(NodeState::Suspect, Some(now));
        let transition = evaluate_node(&record, &policy, now).expect("healthy transition");
        assert_eq!(transition.state, NodeState::Healthy);
    }

    #[test]
    fn administrative_states_are_never_overridden() {
        let policy = FailureDetectionPolicy::default();
        let now = Utc::now();
        let silent = now - TimeDelta::try_seconds(10_000).expect("delta");
        for state in [
            NodeState::Draining,
            NodeState::Maintenance,
            NodeState::Decommissioned,
        ] {
            let record = node(state, Some(silent));
            assert!(evaluate_node(&record, &policy, now).is_none(), "{state}");
        }
    }

    #[test]
    fn quorum_status_reports_loss_honestly() {
        let healthy = QuorumStatus::evaluate(3, Some(3), Some("node-02".into()));
        assert_eq!(healthy.health, ClusterHealth::Healthy);
        assert!(healthy.writable);
        assert!(healthy.fault_tolerant);

        let degraded = QuorumStatus::evaluate(3, Some(2), Some("node-02".into()));
        assert_eq!(degraded.health, ClusterHealth::Degraded);
        assert!(degraded.writable);

        let lost = QuorumStatus::evaluate(3, Some(1), None);
        assert_eq!(lost.health, ClusterHealth::Unavailable);
        assert!(!lost.writable);
        assert!(!lost.notes.is_empty());

        let single = QuorumStatus::evaluate(1, Some(1), Some("node-01".into()));
        assert!(single.writable);
        assert!(!single.fault_tolerant);
        assert!(
            single.notes.iter().any(|note| note.contains("at least 3")),
            "a single-member group must not be presented as fault tolerant"
        );
    }

    /// A follower cannot observe peer reachability, and saying so is not the
    /// same as observing a failure.
    ///
    /// Raft cannot keep a leader without a majority, so a member that can see one
    /// knows a quorum exists. Counting unobserved peers as unreachable reported
    /// every healthy cluster as degraded from any follower, which is exactly the
    /// node an operator reaches through a control-plane console.
    #[test]
    fn a_member_that_cannot_observe_peers_reports_the_quorum_its_leader_implies() {
        let follower = QuorumStatus::evaluate(3, None, Some("node-01".into()));
        assert_eq!(follower.health, ClusterHealth::Healthy);
        assert!(follower.writable);
        assert_eq!(follower.healthy_members, None);
        assert!(
            follower
                .notes
                .iter()
                .any(|note| note.contains("only observable on the leader")),
            "the reader must be told why no count is given: {:?}",
            follower.notes
        );
    }

    /// Without a leader there is no quorum to infer, and the member says so.
    #[test]
    fn a_member_with_no_leader_reports_the_quorum_lost_even_without_a_count() {
        let isolated = QuorumStatus::evaluate(3, None, None);
        assert_eq!(isolated.health, ClusterHealth::Unavailable);
        assert!(!isolated.writable);
        assert!(
            isolated.notes.iter().any(|note| note.contains("no leader")),
            "{:?}",
            isolated.notes
        );
    }

    #[test]
    fn data_health_reflects_under_replication_and_write_capability() {
        let config = ClusterConfig::default();
        let topology = ClusterTopology::new(
            ClusterId::new(),
            config,
            vec![
                node(NodeState::Healthy, Some(Utc::now())),
                node(NodeState::Healthy, Some(Utc::now())),
                node(NodeState::Offline, Some(Utc::now())),
            ],
        );
        let health = DataHealth::evaluate(&topology, 12, 0);
        assert!(health.writable);
        assert_eq!(health.health, ClusterHealth::Degraded);

        let unavailable = DataHealth::evaluate(&topology, 12, 3);
        assert_eq!(unavailable.health, ClusterHealth::Critical);
    }
}
