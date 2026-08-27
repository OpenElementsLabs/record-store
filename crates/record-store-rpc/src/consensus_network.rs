//! Consensus transport over internal RPC.
//!
//! Consensus messages are carried as opaque payloads. Mirroring an external
//! library's internal message shapes in a protobuf contract would create a
//! second source of truth for a protocol OES does not own; the protocol version
//! carried on every connection is what guards compatibility instead.

use std::sync::Arc;

use async_trait::async_trait;
use oes_consensus::{
    ClusterWrite, ClusterWriteResponse, ConsensusError, LeaderForwarder, MemberId, MemberNode,
    OesTypeConfig,
};
use oes_protocol::consensus_v1::{ConsensusEnvelope, ForwardWriteRequest, ReadBarrierRequest};
use openraft::{
    RaftNetwork, RaftNetworkFactory,
    error::{InstallSnapshotError, NetworkError, RPCError, RaftError, Unreachable},
    network::RPCOption,
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
};
use tracing::debug;

use crate::{client::PeerPool, trace::TraceContext};

/// Message kinds carried over the consensus transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsensusCall {
    AppendEntries,
    Vote,
    InstallSnapshot,
}

/// Creates consensus connections to peers.
#[derive(Clone)]
pub struct ConsensusNetwork {
    pool: Arc<PeerPool>,
}

impl ConsensusNetwork {
    /// Creates a factory over a peer pool.
    #[must_use]
    pub const fn new(pool: Arc<PeerPool>) -> Self {
        Self { pool }
    }
}

/// One consensus connection to a single peer.
pub struct ConsensusConnection {
    pool: Arc<PeerPool>,
    address: String,
}

impl ConsensusConnection {
    async fn call<Request, Response>(
        &self,
        kind: ConsensusCall,
        request: &Request,
    ) -> Result<Response, String>
    where
        Request: serde::Serialize,
        Response: serde::de::DeserializeOwned,
    {
        let payload = serde_json::to_vec(request).map_err(|error| error.to_string())?;
        let mut client = self
            .pool
            .consensus(&self.address)
            .await
            .map_err(|error| error.to_string())?;
        let envelope = self
            .pool
            .envelope(ConsensusEnvelope { payload }, &TraceContext::root())
            .await;
        let reply = match kind {
            ConsensusCall::AppendEntries => client.append_entries(envelope).await,
            ConsensusCall::Vote => client.vote(envelope).await,
            ConsensusCall::InstallSnapshot => client.install_snapshot(envelope).await,
        }
        .map_err(|status| format!("{}: {}", status.code(), status.message()))?;
        serde_json::from_slice(&reply.into_inner().payload).map_err(|error| error.to_string())
    }
}

fn unreachable<E: std::error::Error + 'static>(
    error: E,
) -> RPCError<MemberId, MemberNode, RaftError<MemberId>> {
    RPCError::Unreachable(Unreachable::new(&error))
}

impl RaftNetwork<OesTypeConfig> for ConsensusConnection {
    async fn append_entries(
        &mut self,
        request: AppendEntriesRequest<OesTypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<MemberId>, RPCError<MemberId, MemberNode, RaftError<MemberId>>>
    {
        self.call(ConsensusCall::AppendEntries, &request)
            .await
            .map_err(|reason| unreachable(std::io::Error::other(reason)))
    }

    async fn vote(
        &mut self,
        request: VoteRequest<MemberId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<MemberId>, RPCError<MemberId, MemberNode, RaftError<MemberId>>> {
        self.call(ConsensusCall::Vote, &request)
            .await
            .map_err(|reason| unreachable(std::io::Error::other(reason)))
    }

    async fn install_snapshot(
        &mut self,
        request: InstallSnapshotRequest<OesTypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<MemberId>,
        RPCError<MemberId, MemberNode, RaftError<MemberId, InstallSnapshotError>>,
    > {
        self.call(ConsensusCall::InstallSnapshot, &request)
            .await
            .map_err(|reason| RPCError::Network(NetworkError::new(&std::io::Error::other(reason))))
    }
}

impl RaftNetworkFactory<OesTypeConfig> for ConsensusNetwork {
    type Network = ConsensusConnection;

    async fn new_client(&mut self, target: MemberId, node: &MemberNode) -> Self::Network {
        debug!(target, address = %node.addr, "creating consensus connection");
        ConsensusConnection {
            pool: Arc::clone(&self.pool),
            address: node.addr.clone(),
        }
    }
}

/// Forwards metadata writes and read barriers to the current leader.
pub struct RpcLeaderForwarder {
    pool: Arc<PeerPool>,
}

impl RpcLeaderForwarder {
    /// Creates a forwarder over a peer pool.
    #[must_use]
    pub const fn new(pool: Arc<PeerPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl LeaderForwarder for RpcLeaderForwarder {
    async fn forward_write(
        &self,
        leader: MemberId,
        address: &str,
        command: &ClusterWrite,
    ) -> Result<ClusterWriteResponse, ConsensusError> {
        let encoded = serde_json::to_vec(command)
            .map_err(|error| ConsensusError::Forward(error.to_string()))?;
        let mut client = self
            .pool
            .consensus(address)
            .await
            .map_err(|error| ConsensusError::Forward(error.to_string()))?;
        let request = self
            .pool
            .envelope(
                ForwardWriteRequest { command: encoded },
                &TraceContext::root(),
            )
            .await;
        let response = client
            .forward_write(request)
            .await
            .map_err(|status| {
                ConsensusError::Forward(format!(
                    "leader {leader} at {address} returned {}: {}",
                    status.code(),
                    status.message()
                ))
            })?
            .into_inner();
        serde_json::from_slice(&response.response)
            .map_err(|error| ConsensusError::Forward(error.to_string()))
    }

    async fn forward_read_barrier(
        &self,
        leader: MemberId,
        address: &str,
    ) -> Result<Option<u64>, ConsensusError> {
        let mut client = self
            .pool
            .consensus(address)
            .await
            .map_err(|error| ConsensusError::Forward(error.to_string()))?;
        let request = self
            .pool
            .envelope(ReadBarrierRequest {}, &TraceContext::root())
            .await;
        let response = client
            .read_barrier(request)
            .await
            .map_err(|status| {
                ConsensusError::Forward(format!(
                    "leader {leader} at {address} returned {}: {}",
                    status.code(),
                    status.message()
                ))
            })?
            .into_inner();
        Ok(response.has_index.then_some(response.index))
    }
}
