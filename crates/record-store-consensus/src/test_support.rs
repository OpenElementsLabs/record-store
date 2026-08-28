//! Shared fixtures for consensus tests.
//!
//! A single-member cluster is a real consensus group: it elects itself leader,
//! commits through the durable log, and applies to the real state machine. Only
//! the peer transport is a stub, because a lone member never contacts anyone —
//! and if it ever did, these tests would rather fail loudly than pretend.

use std::sync::Arc;

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use tempfile::TempDir;

use crate::types::{MemberId, MemberNode, RecordStoreTypeConfig};
use crate::{ConsensusSettings, MetadataConsensus};

/// A transport no single-member cluster should ever reach for.
pub(crate) struct UnreachablePeers;

/// The connection the stub factory hands out.
pub(crate) struct NoPeer;

fn unreachable<E: std::error::Error + 'static>(
    error: E,
) -> RPCError<MemberId, MemberNode, RaftError<MemberId>> {
    RPCError::Unreachable(Unreachable::new(&error))
}

fn refused() -> std::io::Error {
    std::io::Error::other("a single-member cluster has no peers to contact")
}

impl RaftNetwork<RecordStoreTypeConfig> for NoPeer {
    async fn append_entries(
        &mut self,
        _request: AppendEntriesRequest<RecordStoreTypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<MemberId>, RPCError<MemberId, MemberNode, RaftError<MemberId>>>
    {
        Err(unreachable(refused()))
    }

    async fn vote(
        &mut self,
        _request: VoteRequest<MemberId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<MemberId>, RPCError<MemberId, MemberNode, RaftError<MemberId>>> {
        Err(unreachable(refused()))
    }

    async fn install_snapshot(
        &mut self,
        _request: InstallSnapshotRequest<RecordStoreTypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<MemberId>,
        RPCError<MemberId, MemberNode, RaftError<MemberId, InstallSnapshotError>>,
    > {
        Err(RPCError::Network(NetworkError::new(&refused())))
    }
}

impl RaftNetworkFactory<RecordStoreTypeConfig> for UnreachablePeers {
    type Network = NoPeer;

    async fn new_client(&mut self, _target: MemberId, _node: &MemberNode) -> Self::Network {
        NoPeer
    }
}

/// Starts a single-member consensus group and waits until it leads itself.
pub(crate) async fn consensus() -> (TempDir, Arc<MetadataConsensus>) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut settings =
        ConsensusSettings::new(1, "127.0.0.1:17603", directory.path().join("consensus"));
    // A lone member has nobody to wait for, so the timings are pulled in to keep
    // the tests fast without changing any behaviour under test.
    settings.heartbeat_interval_millis = 20;
    settings.election_timeout_min_millis = 60;
    settings.election_timeout_max_millis = 120;

    let consensus = MetadataConsensus::start(settings, UnreachablePeers)
        .await
        .expect("start consensus");
    consensus
        .initialize_single_member()
        .await
        .expect("initialize");
    consensus
        .wait_for_leader(std::time::Duration::from_secs(10))
        .await
        .expect("elect a leader");
    (directory, consensus)
}
