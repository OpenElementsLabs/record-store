//! The internal RPC listener.
//!
//! The listener binds its own address, separate from both public listeners, and
//! is never published by default. It is supervised like any other critical task:
//! if it stops unexpectedly, the node reports that rather than silently losing
//! its ability to participate in the cluster.

use std::{future::Future, net::SocketAddr, time::Duration};

use record_store_protocol::{
    consensus_v1::consensus_service_server::ConsensusServiceServer,
    replica_v1::replica_service_server::ReplicaServiceServer,
    system_v1::system_service_server::SystemServiceServer,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tracing::info;

use crate::{
    services::{ConsensusRpcService, ReplicaRpcService, SystemRpcService},
    tls::{TlsError, TlsSettings},
};

/// Failures raised by the internal RPC listener.
#[derive(Debug, Error)]
pub enum RpcServerError {
    /// The internal address could not be bound.
    #[error("failed to bind the internal RPC listener on {address}: {source}")]
    Bind {
        /// Address that could not be bound.
        address: SocketAddr,
        /// Socket failure.
        #[source]
        source: std::io::Error,
    },
    /// Transport security could not be prepared.
    #[error(transparent)]
    Tls(#[from] TlsError),
    /// Serving failed.
    #[error("internal RPC listener failed: {0}")]
    Serve(String),
}

/// Node-local listener settings.
#[derive(Debug, Clone)]
pub struct RpcServerSettings {
    /// Address the listener binds.
    pub bind: SocketAddr,
    /// Transport security.
    pub tls: TlsSettings,
    /// Maximum concurrent internal streams accepted from all peers.
    pub concurrency_limit: usize,
    /// Maximum time a graceful shutdown may take.
    pub shutdown_grace_period: Duration,
}

/// The assembled internal RPC listener.
pub struct InternalRpcServer {
    settings: RpcServerSettings,
    consensus: Option<ConsensusRpcService>,
    replica: Option<ReplicaRpcService>,
    system: Option<SystemRpcService>,
}

impl InternalRpcServer {
    /// Creates a listener with no services registered yet.
    #[must_use]
    pub const fn new(settings: RpcServerSettings) -> Self {
        Self {
            settings,
            consensus: None,
            replica: None,
            system: None,
        }
    }

    /// Registers the metadata consensus transport.
    #[must_use]
    pub fn with_consensus(mut self, service: ConsensusRpcService) -> Self {
        self.consensus = Some(service);
        self
    }

    /// Registers replica transfer and integrity operations.
    #[must_use]
    pub fn with_replica(mut self, service: ReplicaRpcService) -> Self {
        self.replica = Some(service);
        self
    }

    /// Registers node lifecycle and diagnostics.
    #[must_use]
    pub fn with_system(mut self, service: SystemRpcService) -> Self {
        self.system = Some(service);
        self
    }

    /// Binds the configured address.
    ///
    /// Binding is separated from serving so that start-up fails fast and
    /// deterministically when the internal address is unusable.
    pub async fn bind(&self) -> Result<TcpListener, RpcServerError> {
        TcpListener::bind(self.settings.bind)
            .await
            .map_err(|source| RpcServerError::Bind {
                address: self.settings.bind,
                source,
            })
    }

    /// Serves until the shutdown future resolves.
    pub async fn serve<F>(self, listener: TcpListener, shutdown: F) -> Result<(), RpcServerError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let local = listener.local_addr().unwrap_or(self.settings.bind);
        let mut builder = Server::builder()
            .concurrency_limit_per_connection(self.settings.concurrency_limit)
            .timeout(Duration::from_secs(300))
            .http2_keepalive_interval(Some(Duration::from_secs(15)));
        if let Some(tls) = self.settings.tls.server_config()? {
            builder = builder
                .tls_config(tls)
                .map_err(|error| RpcServerError::Serve(error.to_string()))?;
        }
        info!(
            address = %local,
            tls = self.settings.tls.enabled(),
            mutual_tls = self.settings.tls.mutual(),
            "internal RPC listening"
        );
        let router = builder
            .add_optional_service(self.consensus.map(ConsensusServiceServer::new))
            .add_optional_service(self.replica.map(|service| {
                ReplicaServiceServer::new(service)
                    .max_decoding_message_size(8 * 1024 * 1024)
                    .max_encoding_message_size(8 * 1024 * 1024)
            }))
            .add_optional_service(self.system.map(SystemServiceServer::new));
        router
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
            .await
            .map_err(|error| RpcServerError::Serve(error.to_string()))
    }
}
