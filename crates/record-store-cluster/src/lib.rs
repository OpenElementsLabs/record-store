//! Cluster domain model for distributed OES.
//!
//! This crate owns everything a cluster needs to reason about itself without any
//! network or consensus dependency:
//!
//! * stable node identity that survives restarts and address changes;
//! * internal protocol, software, and storage-format compatibility rules;
//! * an explicit node lifecycle state machine and failure detection;
//! * failure-domain-aware replica placement;
//! * replica state, durability accounting, and tombstones;
//! * one durable queue for repair, drain, and rebalance movement;
//! * the replicated cluster catalog and its deterministic command set.
//!
//! Keeping these decisions free of I/O is what makes them testable in isolation,
//! which matters more for distributed behaviour than for anything else in OES.

pub mod catalog;
pub mod command;
pub mod config;
pub mod credentials;
pub mod health;
pub mod identity;
pub mod placement;
pub mod rebalance;
pub mod replica;
pub mod tasks;
pub mod topology;
pub mod version;

pub use catalog::{
    CLUSTER_TABLES, CatalogEntry, ClusterCatalog, ClusterCatalogError, ClusterUsage, PlacementPage,
    TaskPage, apply_command_tx, export_tx, import_tx, initialize_tables,
};
pub use command::{ClusterCommand, ClusterIdentity, ClusterOutcome, enqueue_repair};
pub use config::{
    CapacityLevel, CapacityWatermarks, ClusterConfig, ClusterConfigError, FailureDetectionPolicy,
    MAXIMUM_REPLICATION_FACTOR, MovementPolicy, RebalancePolicy, RepairPolicy,
    WriteAcknowledgement,
};
pub use credentials::{
    ClusterSecret, CredentialError, IssuedJoinToken, IssuedNodeCredential, JoinToken,
    NodeCredential, parse_join_token, parse_node_credential,
};
pub use health::{
    ClusterHealth, DataHealth, HealthTransition, QuorumStatus, Readiness, evaluate_node,
};
pub use identity::{IdentityError, NodeIdentity, NodeIdentityStore, RaftNodeId};
pub use placement::{
    CapacityAwarePlacement, ObjectPlacementRequest, PlacementError, PlacementPlan, PlacementPolicy,
    PlacementTarget,
};
pub use rebalance::{DecommissionSafety, RebalanceCandidate, RebalanceMove, plan_rebalance};
pub use replica::{Durability, PayloadPlacement, Replica, ReplicaState, Tombstone};
pub use tasks::{
    ClusterOperation, ClusterOperationKind, ClusterOperationState, MovementBudget,
    OperationProgress, ReplicaTask, ReplicaTaskKind, ReplicaTaskPriority, ReplicaTaskState,
};
pub use topology::{
    ClusterTopology, FailureDomain, FailureDomainScope, NodeActivity, NodeCapacity, NodeRecord,
    NodeRegistration, NodeState, StorageClass, TopologyError,
};
pub use version::{
    CLUSTER_FORMAT_VERSION, CompatibilityError, MINIMUM_COMPATIBLE_PROTOCOL_MINOR_VERSION,
    NodeVersions, PROTOCOL_MAJOR_VERSION, PROTOCOL_MINOR_VERSION, ProtocolVersion,
    STORAGE_FORMAT_VERSION, check_compatibility,
};
