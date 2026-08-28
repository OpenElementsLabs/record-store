//! Consensus-backed cluster metadata.
//!
//! Record Store keeps exactly one authoritative copy of cluster metadata, replicated by a
//! Raft group formed from the storage nodes themselves. That placement is
//! deliberate: it means object requests keep working while the management plane
//! is restarted, upgraded, or temporarily unavailable.
//!
//! Two rules bound what consensus is used for:
//!
//! * only metadata is replicated through the log — bucket records, object
//!   versions, replica placement, tombstones, membership, and configuration;
//! * object bytes never enter the log. They stream directly between storage
//!   nodes and are referenced by the committed metadata.
//!
//! The consensus algorithm itself is provided by a mature implementation rather
//! than written here, and it is fully hidden behind [`MetadataConsensus`] so the
//! choice can change without touching the rest of Record Store.

mod command;
mod consensus;
mod log_store;
mod repository;
mod state_machine;
mod types;

#[cfg(test)]
mod test_support;

pub use command::{
    ClusterWrite, ClusterWriteResponse, CommandRejection, RejectionKind, classify_cluster_error,
    classify_metadata_error, is_durable_cluster_failure, is_durable_metadata_failure,
};
pub use consensus::{
    ConsensusError, ConsensusMemberStatus, ConsensusSettings, LeaderForwarder, MetadataConsensus,
    MetadataQuorum, rejection_error,
};
pub use log_store::{LogStoreError, RedbLogStore};
pub use repository::{
    ClusterStore, LocalClusterStore, ReplicatedClusterStore, ReplicatedMetadataRepository,
};
pub use state_machine::{ReplicatedState, StateMachineError, StateMachineStore};
pub use types::{
    ConsensusEntry, ConsensusLogId, ConsensusMembership, MemberId, MemberNode,
    RecordStoreTypeConfig,
};
