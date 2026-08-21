//! Internal node-to-node RPC.
//!
//! The internal listener is separate from both public listeners and is not
//! exposed publicly by default. Every call carries the caller's protocol
//! version, cluster identity, node identity, node credential, and trace
//! context, and all of them are validated before any cluster state is touched.

pub mod client;
pub mod consensus_network;
pub mod peer;
pub mod server;
pub mod services;
pub mod tls;
pub mod trace;

pub use client::{
    PeerPool, RemoteReadStream, RemoteReplicaVerification, RemoteReplicaWrite, ReplicaTarget,
    ReplicaTransport, RpcClientError, RpcClientSettings, RpcReplicaTransport, TransferExpectation,
    TransferStream,
};
pub use consensus_network::{ConsensusNetwork, RpcLeaderForwarder};
pub use peer::{
    AuthenticationRequirement, CatalogPeerAuthenticator, PeerAuthenticator, PeerError, PeerHeaders,
    PeerIdentity, PeerVerifier,
};
pub use server::{InternalRpcServer, RpcServerError, RpcServerSettings};
pub use services::{
    ClusterAdmission, ConsensusRpcService, JoinOutcome, NodeJoinRequest, ReplicaRpcService,
    SystemRpcService,
};
pub use tls::{TlsError, TlsSettings};
pub use trace::TraceContext;
