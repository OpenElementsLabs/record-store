//! Internal RPC clients.
//!
//! Connections are created lazily, cached per peer address, and always carry
//! this node's identity, protocol version, and credential. Callers work with
//! OES-owned request and response types; no other crate sees a transport type.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use oes_core::{Checksum, ObjectId};
use oes_protocol::{
    consensus_v1::consensus_service_client::ConsensusServiceClient,
    replica_v1::replica_service_client::ReplicaServiceClient,
    system_v1::{
        ActivateRequest, ActivateResponse, JoinRequest, NodeDescriptor, NodeProfile, PingRequest,
        system_service_client::SystemServiceClient,
    },
};
use thiserror::Error;
use tokio::sync::RwLock;
use tonic::{
    Request, Status,
    transport::{Channel, Endpoint},
};
use tracing::debug;

use crate::{
    peer::{JOIN_TOKEN_HEADER, PeerHeaders},
    services::JoinOutcome,
    tls::{TlsError, TlsSettings},
    trace::TraceContext,
};

/// Failures raised by internal RPC clients.
#[derive(Debug, Error)]
pub enum RpcClientError {
    /// The peer address could not be turned into an endpoint.
    #[error("invalid internal peer address '{address}': {reason}")]
    Address {
        /// Address that could not be used.
        address: String,
        /// Why it was rejected.
        reason: String,
    },
    /// Transport security could not be prepared.
    #[error(transparent)]
    Tls(#[from] TlsError),
    /// The peer could not be reached.
    #[error("internal peer {address} is unreachable: {reason}")]
    Unreachable {
        /// Peer that could not be reached.
        address: String,
        /// Why it could not be reached.
        reason: String,
    },
    /// The peer refused or failed the call.
    #[error("internal call to {address} failed: {status}")]
    Call {
        /// Peer that returned the failure.
        address: String,
        /// Failure detail.
        status: String,
    },
    /// The peer returned a response OES could not interpret.
    #[error("internal peer {address} returned an unusable response: {reason}")]
    Response {
        /// Peer that returned the response.
        address: String,
        /// Why it was unusable.
        reason: String,
    },
}

impl RpcClientError {
    /// Returns whether retrying against another peer is sensible.
    #[must_use]
    pub const fn transient(&self) -> bool {
        matches!(self, Self::Unreachable { .. } | Self::Call { .. })
    }
}

fn call_error(address: &str, status: Status) -> RpcClientError {
    match status.code() {
        tonic::Code::Unavailable | tonic::Code::DeadlineExceeded => RpcClientError::Unreachable {
            address: address.to_owned(),
            reason: status.message().to_owned(),
        },
        _ => RpcClientError::Call {
            address: address.to_owned(),
            status: format!("{}: {}", status.code(), status.message()),
        },
    }
}

/// Node-local client settings.
#[derive(Debug, Clone)]
pub struct RpcClientSettings {
    /// Identity headers attached to every call.
    pub headers: PeerHeaders,
    /// Transport security.
    pub tls: TlsSettings,
    /// Time allowed to establish a connection.
    pub connect_timeout: Duration,
    /// Time allowed for a unary call.
    pub request_timeout: Duration,
    /// Time allowed for one chunk of a streaming transfer to be accepted.
    pub stream_chunk_timeout: Duration,
    /// Chunk size used for replica transfers.
    pub transfer_chunk_bytes: usize,
    /// Chunks buffered in flight per replica transfer.
    ///
    /// This bound is what makes a slow destination slow the source instead of
    /// growing memory without limit.
    pub transfer_queue_depth: usize,
}

impl RpcClientSettings {
    /// Creates settings with conservative transfer bounds.
    #[must_use]
    pub fn new(headers: PeerHeaders, tls: TlsSettings) -> Self {
        Self {
            headers,
            tls,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            stream_chunk_timeout: Duration::from_secs(30),
            transfer_chunk_bytes: 256 * 1024,
            transfer_queue_depth: 8,
        }
    }
}

/// A lazily connected, cached pool of internal peer connections.
pub struct PeerPool {
    settings: RwLock<RpcClientSettings>,
    channels: Mutex<HashMap<String, Channel>>,
}

impl PeerPool {
    /// Creates a pool for this node.
    #[must_use]
    pub fn new(settings: RpcClientSettings) -> Arc<Self> {
        Arc::new(Self {
            settings: RwLock::new(settings),
            channels: Mutex::new(HashMap::new()),
        })
    }

    /// Replaces the identity headers, for example after a node joins a cluster
    /// or rotates its credential.
    pub async fn update_headers(&self, headers: PeerHeaders) {
        self.settings.write().await.headers = headers;
        if let Ok(mut channels) = self.channels.lock() {
            // Existing connections keep working, but new calls must carry the
            // new identity, so the cache is cleared deliberately.
            channels.clear();
        }
    }

    /// Returns the current settings.
    pub async fn settings(&self) -> RpcClientSettings {
        self.settings.read().await.clone()
    }

    async fn channel(&self, address: &str) -> Result<Channel, RpcClientError> {
        if let Ok(channels) = self.channels.lock()
            && let Some(channel) = channels.get(address)
        {
            return Ok(channel.clone());
        }
        let settings = self.settings.read().await.clone();
        let uri = if address.contains("://") {
            address.to_owned()
        } else if settings.tls.enabled() {
            format!("https://{address}")
        } else {
            format!("http://{address}")
        };
        let mut endpoint = Endpoint::from_shared(uri).map_err(|error| RpcClientError::Address {
            address: address.to_owned(),
            reason: error.to_string(),
        })?;
        endpoint = endpoint
            .connect_timeout(settings.connect_timeout)
            .timeout(settings.request_timeout)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(15));
        if let Some(tls) = settings.tls.client_config()? {
            endpoint = endpoint
                .tls_config(tls)
                .map_err(|error| RpcClientError::Address {
                    address: address.to_owned(),
                    reason: error.to_string(),
                })?;
        }
        // A lazy channel connects on first use, so an unreachable peer does not
        // block start-up or hold a task while a node is restarting.
        let channel = endpoint.connect_lazy();
        if let Ok(mut channels) = self.channels.lock() {
            channels.insert(address.to_owned(), channel.clone());
        }
        Ok(channel)
    }

    async fn request<T>(&self, message: T, trace: &TraceContext) -> Request<T> {
        let settings = self.settings.read().await;
        let mut request = Request::new(message);
        settings.headers.write(request.metadata_mut(), trace);
        request
    }

    /// Returns a consensus client for a peer.
    pub async fn consensus(
        &self,
        address: &str,
    ) -> Result<ConsensusServiceClient<Channel>, RpcClientError> {
        Ok(ConsensusServiceClient::new(self.channel(address).await?))
    }

    /// Returns a replica client for a peer.
    pub async fn replica(
        &self,
        address: &str,
    ) -> Result<ReplicaServiceClient<Channel>, RpcClientError> {
        Ok(ReplicaServiceClient::new(self.channel(address).await?)
            .max_decoding_message_size(4 * 1024 * 1024)
            .max_encoding_message_size(4 * 1024 * 1024))
    }

    /// Returns a system client for a peer.
    pub async fn system(
        &self,
        address: &str,
    ) -> Result<SystemServiceClient<Channel>, RpcClientError> {
        Ok(SystemServiceClient::new(self.channel(address).await?))
    }

    /// Builds a request carrying this node's identity and a trace context.
    pub async fn envelope<T>(&self, message: T, trace: &TraceContext) -> Request<T> {
        self.request(message, trace).await
    }

    /// Builds a request that also carries a single-use join token.
    pub async fn join_request<T>(
        &self,
        message: T,
        token: &str,
        trace: &TraceContext,
    ) -> Request<T> {
        let mut request = self.request(message, trace).await;
        if let Ok(value) = tonic::metadata::MetadataValue::try_from(token) {
            request.metadata_mut().insert(JOIN_TOKEN_HEADER, value);
        }
        request
    }

    /// Exchanges a single-use token for durable cluster membership.
    pub async fn join_cluster(
        &self,
        address: &str,
        token: &str,
        peer: NodeDescriptor,
        profile: NodeProfile,
    ) -> Result<JoinOutcome, RpcClientError> {
        let mut client = self.system(address).await?;
        let request = self
            .join_request(
                JoinRequest {
                    join_token: token.to_owned(),
                    peer: Some(peer),
                    profile: Some(profile),
                },
                token,
                &TraceContext::root(),
            )
            .await;
        let response = client
            .join(request)
            .await
            .map_err(|status| call_error(address, status))?
            .into_inner();
        let cluster_id = response
            .cluster_id
            .parse()
            .map_err(|_| RpcClientError::Response {
                address: address.to_owned(),
                reason: "join response contained an invalid cluster identity".to_owned(),
            })?;
        let cluster_config = serde_json::from_str(&response.cluster_config).map_err(|error| {
            RpcClientError::Response {
                address: address.to_owned(),
                reason: format!("join response contained invalid cluster configuration: {error}"),
            }
        })?;
        if response.member_id == 0 || response.node_credential.is_empty() {
            return Err(RpcClientError::Response {
                address: address.to_owned(),
                reason: "join response omitted the member identity or node credential".to_owned(),
            });
        }
        Ok(JoinOutcome {
            cluster_id,
            member_id: response.member_id,
            node_credential: response.node_credential,
            metadata_voter: response.metadata_voter,
            cluster_config,
        })
    }

    /// Retrieves a seed's versioned identity before a join attempt.
    pub async fn probe_cluster(
        &self,
        address: &str,
        peer: NodeDescriptor,
    ) -> Result<NodeDescriptor, RpcClientError> {
        let mut client = self.system(address).await?;
        let request = self
            .envelope(PingRequest { peer: Some(peer) }, &TraceContext::root())
            .await;
        client
            .ping(request)
            .await
            .map_err(|status| call_error(address, status))?
            .into_inner()
            .peer
            .ok_or_else(|| RpcClientError::Response {
                address: address.to_owned(),
                reason: "seed probe omitted its node descriptor".to_owned(),
            })
    }

    /// Asks an existing member to add this node to the consensus group.
    pub async fn activate_cluster(
        &self,
        address: &str,
        peer: NodeDescriptor,
        profile: NodeProfile,
    ) -> Result<ActivateResponse, RpcClientError> {
        let mut client = self.system(address).await?;
        let request = self
            .envelope(
                ActivateRequest {
                    peer: Some(peer),
                    profile: Some(profile),
                },
                &TraceContext::root(),
            )
            .await;
        client
            .activate(request)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| call_error(address, status))
    }
}

/// A replica transfer target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaTarget {
    /// Node holding or receiving the replica.
    pub node_id: oes_core::NodeId,
    /// Internal RPC address of that node.
    pub address: String,
}

/// What a remote replica write reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteReplicaWrite {
    /// Payload identifier that was stored.
    pub object_id: ObjectId,
    /// Logical bytes the peer recorded.
    pub size: u64,
    /// Checksum the peer calculated locally.
    pub checksum: Checksum,
    /// Whether the peer already held a verified replica.
    pub already_present: bool,
}

/// What a remote verification reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteReplicaVerification {
    /// Whether the peer holds the payload.
    pub present: bool,
    /// Whether the peer's bytes matched the expected checksum.
    pub matches: bool,
    /// Logical bytes the peer read.
    pub size: u64,
    /// Checksum the peer calculated.
    pub checksum: Option<Checksum>,
}

/// A bounded stream of payload chunks handed to a replica transfer.
pub type TransferStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// How a transfer's expected content is established.
pub enum TransferExpectation {
    /// The expectation is known from committed metadata before the transfer.
    Known {
        /// Logical payload length.
        size: u64,
        /// Logical payload checksum.
        checksum: Checksum,
    },
    /// The expectation arrives after the last byte, as for a client upload.
    Trailing(tokio::sync::oneshot::Receiver<Result<(u64, Checksum), String>>),
}

/// A bounded stream of payload chunks produced by a replica read.
pub type RemoteReadStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<Bytes, RpcClientError>> + Send + 'static>,
>;

/// Replica transfer operations against remote nodes.
///
/// The trait exists so replication logic can be tested without a live cluster,
/// and so nothing above this layer depends on a transport.
#[async_trait::async_trait]
pub trait ReplicaTransport: Send + Sync {
    /// Streams a payload to a peer, which verifies it before publishing.
    async fn write_replica(
        &self,
        target: &ReplicaTarget,
        operation_id: &str,
        object_id: ObjectId,
        expectation: TransferExpectation,
        body: TransferStream,
    ) -> Result<RemoteReplicaWrite, RpcClientError>;

    /// Opens a payload read from a peer.
    async fn read_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
        size: u64,
        checksum: &Checksum,
    ) -> Result<RemoteReadStream, RpcClientError>;

    /// Removes a payload from a peer.
    async fn delete_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
    ) -> Result<bool, RpcClientError>;

    /// Asks a peer to verify a payload it holds.
    async fn verify_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
        size: u64,
        checksum: &Checksum,
    ) -> Result<RemoteReplicaVerification, RpcClientError>;

    /// Lists the payload identifiers a peer physically stores.
    async fn list_local_payloads(
        &self,
        target: &ReplicaTarget,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, RpcClientError>;
}

/// The gRPC implementation of [`ReplicaTransport`].
pub struct RpcReplicaTransport {
    pool: Arc<PeerPool>,
}

impl RpcReplicaTransport {
    /// Creates a transport over a peer pool.
    #[must_use]
    pub const fn new(pool: Arc<PeerPool>) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ReplicaTransport for RpcReplicaTransport {
    async fn write_replica(
        &self,
        target: &ReplicaTarget,
        operation_id: &str,
        object_id: ObjectId,
        expectation: TransferExpectation,
        mut body: TransferStream,
    ) -> Result<RemoteReplicaWrite, RpcClientError> {
        use futures_util::StreamExt;
        use oes_protocol::replica_v1::{
            ReplicaDescriptor, WriteReplicaChunk, WriteReplicaCommit, WriteReplicaHeader,
            write_replica_chunk,
        };

        let settings = self.pool.settings().await;
        let mut client = self.pool.replica(&target.address).await?;
        let (sender, receiver) = tokio::sync::mpsc::channel(settings.transfer_queue_depth);
        let (declared_size, declared_checksum) = match &expectation {
            TransferExpectation::Known { size, checksum } => (*size, checksum.to_string()),
            TransferExpectation::Trailing(_) => (0, String::new()),
        };
        let header = WriteReplicaChunk {
            body: Some(write_replica_chunk::Body::Header(WriteReplicaHeader {
                operation_id: operation_id.to_owned(),
                descriptor: Some(ReplicaDescriptor {
                    object_id: object_id.to_string(),
                    size: declared_size,
                    checksum: declared_checksum,
                }),
            })),
        };
        if sender.send(header).await.is_err() {
            return Err(RpcClientError::Unreachable {
                address: target.address.clone(),
                reason: "replica transfer was closed before it started".into(),
            });
        }
        let address = target.address.clone();
        let chunk_timeout = settings.stream_chunk_timeout;
        let pump = tokio::spawn(async move {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|error| error.to_string())?;
                if chunk.is_empty() {
                    continue;
                }
                let message = WriteReplicaChunk {
                    body: Some(write_replica_chunk::Body::Data(chunk.to_vec())),
                };
                // A bounded channel with a timeout turns a stalled destination
                // into a failed target rather than unbounded buffering.
                match tokio::time::timeout(chunk_timeout, sender.send(message)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return Err("replica transfer was closed by the peer".to_owned()),
                    Err(_) => return Err("replica transfer stalled".to_owned()),
                }
            }
            // The commitment closes the transfer. The peer refuses to publish
            // anything until it has recomputed the checksum and matched it.
            let (size, checksum) = match expectation {
                TransferExpectation::Known { size, checksum } => (size, checksum),
                TransferExpectation::Trailing(receiver) => match receiver.await {
                    Ok(Ok(commitment)) => commitment,
                    Ok(Err(reason)) => return Err(reason),
                    Err(_) => {
                        return Err("upload ended without a commitment".to_owned());
                    }
                },
            };
            let commit = WriteReplicaChunk {
                body: Some(write_replica_chunk::Body::Commit(WriteReplicaCommit {
                    size,
                    checksum: checksum.to_string(),
                })),
            };
            match tokio::time::timeout(chunk_timeout, sender.send(commit)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(_)) => Err("replica transfer was closed by the peer".to_owned()),
                Err(_) => Err("replica transfer stalled".to_owned()),
            }
        });

        let request = self
            .pool
            .envelope(
                tokio_stream::wrappers::ReceiverStream::new(receiver),
                &TraceContext::root(),
            )
            .await;
        let response = client.write_replica(request).await;
        let pump_result = pump.await;
        let response = response.map_err(|status| call_error(&address, status))?;
        match pump_result {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => {
                return Err(RpcClientError::Unreachable { address, reason });
            }
            Err(error) => {
                return Err(RpcClientError::Unreachable {
                    address,
                    reason: error.to_string(),
                });
            }
        }
        let response = response.into_inner();
        let checksum =
            response
                .checksum
                .parse::<Checksum>()
                .map_err(|error| RpcClientError::Response {
                    address: address.clone(),
                    reason: error.to_string(),
                })?;
        let object_id =
            response
                .object_id
                .parse::<ObjectId>()
                .map_err(|error| RpcClientError::Response {
                    address,
                    reason: error.to_string(),
                })?;
        Ok(RemoteReplicaWrite {
            object_id,
            size: response.size,
            checksum,
            already_present: response.already_present,
        })
    }

    async fn read_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
        size: u64,
        checksum: &Checksum,
    ) -> Result<RemoteReadStream, RpcClientError> {
        use futures_util::StreamExt;
        use oes_protocol::replica_v1::ReadReplicaRequest;

        let mut client = self.pool.replica(&target.address).await?;
        let request = self
            .pool
            .envelope(
                ReadReplicaRequest {
                    object_id: object_id.to_string(),
                    size,
                    checksum: checksum.to_string(),
                    offset: 0,
                    length: 0,
                    whole_payload: true,
                },
                &TraceContext::root(),
            )
            .await;
        let address = target.address.clone();
        let stream = client
            .read_replica(request)
            .await
            .map_err(|status| call_error(&address, status))?
            .into_inner();
        let mapped = stream.map(move |chunk| match chunk {
            Ok(chunk) => Ok(Bytes::from(chunk.data)),
            Err(status) => Err(call_error(&address, status)),
        });
        Ok(Box::pin(mapped))
    }

    async fn delete_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
    ) -> Result<bool, RpcClientError> {
        use oes_protocol::replica_v1::DeleteReplicaRequest;

        let mut client = self.pool.replica(&target.address).await?;
        let request = self
            .pool
            .envelope(
                DeleteReplicaRequest {
                    object_id: object_id.to_string(),
                },
                &TraceContext::root(),
            )
            .await;
        let response = client
            .delete_replica(request)
            .await
            .map_err(|status| call_error(&target.address, status))?;
        Ok(response.into_inner().removed)
    }

    async fn verify_replica(
        &self,
        target: &ReplicaTarget,
        object_id: ObjectId,
        size: u64,
        checksum: &Checksum,
    ) -> Result<RemoteReplicaVerification, RpcClientError> {
        use oes_protocol::replica_v1::VerifyReplicaRequest;

        let mut client = self.pool.replica(&target.address).await?;
        let request = self
            .pool
            .envelope(
                VerifyReplicaRequest {
                    object_id: object_id.to_string(),
                    size,
                    checksum: checksum.to_string(),
                },
                &TraceContext::root(),
            )
            .await;
        let response = client
            .verify_replica(request)
            .await
            .map_err(|status| call_error(&target.address, status))?
            .into_inner();
        Ok(RemoteReplicaVerification {
            present: response.present,
            matches: response.matches,
            size: response.size,
            checksum: response.checksum.parse().ok(),
        })
    }

    async fn list_local_payloads(
        &self,
        target: &ReplicaTarget,
        after: Option<ObjectId>,
        limit: usize,
    ) -> Result<Vec<ObjectId>, RpcClientError> {
        use oes_protocol::replica_v1::ListLocalPayloadsRequest;

        let mut client = self.pool.replica(&target.address).await?;
        let request = self
            .pool
            .envelope(
                ListLocalPayloadsRequest {
                    after_object_id: after.map(|id| id.to_string()).unwrap_or_default(),
                    limit: u32::try_from(limit).unwrap_or(u32::MAX),
                },
                &TraceContext::root(),
            )
            .await;
        let response = client
            .list_local_payloads(request)
            .await
            .map_err(|status| call_error(&target.address, status))?
            .into_inner();
        let mut out = Vec::with_capacity(response.object_ids.len());
        for encoded in response.object_ids {
            match encoded.parse::<ObjectId>() {
                Ok(object_id) => out.push(object_id),
                Err(error) => {
                    debug!(%error, "peer returned an unusable payload identifier");
                }
            }
        }
        Ok(out)
    }
}
