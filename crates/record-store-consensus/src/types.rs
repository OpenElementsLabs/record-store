//! Consensus type configuration.
//!
//! The consensus implementation is deliberately hidden behind OES-owned types.
//! Nothing outside this crate names an `openraft` type, so the dependency can be
//! replaced without touching the rest of the system.

use std::io::Cursor;

use openraft::{TokioRuntime, impls::OneshotResponder};

use crate::command::{ClusterWrite, ClusterWriteResponse};

/// Dense consensus member identifier assigned when a node joins.
pub type MemberId = u64;

/// Consensus member addressing information.
pub type MemberNode = openraft::BasicNode;

openraft::declare_raft_types!(
    /// The OES consensus type configuration.
    pub OesTypeConfig:
        D = ClusterWrite,
        R = ClusterWriteResponse,
        NodeId = MemberId,
        Node = MemberNode,
        Entry = openraft::Entry<OesTypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        AsyncRuntime = TokioRuntime,
        Responder = OneshotResponder<OesTypeConfig>,
);

/// A committed log position.
pub type ConsensusLogId = openraft::LogId<MemberId>;

/// A consensus log entry.
pub type ConsensusEntry = openraft::Entry<OesTypeConfig>;

/// The membership configuration stored with the state machine.
pub type ConsensusMembership = openraft::StoredMembership<MemberId, MemberNode>;
