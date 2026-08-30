//! The distributed data plane.
//!
//! This crate turns the single-node object store into a replicated one without
//! changing the contract the S3 layer sees. It owns:
//!
//! * the write path that streams one payload to several replicas in one pass and
//!   commits object metadata and replica placement together;
//! * the read path that prefers a local replica, falls back before any byte
//!   reaches the client, and verifies integrity independently;
//! * the supervised background services that detect failures, repair
//!   under-replicated payloads, rebalance capacity, drain nodes, and collect
//!   garbage;
//! * cluster admission and administrative cluster operations.

pub mod admission;
pub mod context;
pub mod coordinator;
pub mod operations;
pub mod read;
pub mod runtime;
pub mod status;
pub mod store;
pub mod tasks;
pub mod write;

#[cfg(test)]
mod test_support;

pub use admission::{ClusterAdmissionService, JoinCoordinator};
pub use context::ClusterContext;
pub use coordinator::{Coordinator, CoordinatorSettings};
pub use operations::{ClusterOperations, OperationError, SimulationReport, TopologyChange};
pub use read::{ReadCandidate, ReplicaRead, open_replica, read_candidates};
pub use runtime::{ClusterRuntime, RuntimeSettings, SupervisedTasks, TaskHealth, TaskStatus};
pub use status::{ClusterStatus, DeviceStatus, NodeStatus, RepairStatus, ReplicationStatus};
pub use store::{DistributedObjectStore, DistributedSettings};
pub use tasks::TaskExecutor;
pub use write::{ReplicationOutcome, WriteSettings, replicate};
