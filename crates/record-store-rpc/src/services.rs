//! Internal RPC service implementations.
#![expect(
    clippy::result_large_err,
    reason = "the transport's status type sets the error size and is not ours to box"
)]

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use oes_cluster::{
    ClusterConfig, FailureDomain, NodeCapacity, NodeRegistration, NodeVersions, ProtocolVersion,
    StorageClass,
};
use oes_consensus::{ClusterWrite, ConsensusError, MetadataConsensus};
use oes_core::{Checksum, ClusterId, NodeId, ObjectId, PayloadFormat};
use oes_protocol::{
    consensus_v1::{
        ConsensusEnvelope, ConsensusReply, ForwardWriteRequest, ForwardWriteResponse,
        ReadBarrierRequest, ReadBarrierResponse, consensus_service_server::ConsensusService,
    },
    replica_v1::{
        DeleteReplicaRequest, DeleteReplicaResponse, ListLocalPayloadsRequest,
        ListLocalPayloadsResponse, ReadReplicaChunk, ReadReplicaRequest, StatReplicaRequest,
        StatReplicaResponse, VerifyReplicaRequest, VerifyReplicaResponse, WriteReplicaChunk,
        WriteReplicaResponse, replica_service_server::ReplicaService, write_replica_chunk,
    },
    system_v1::{
        ActivateRequest, ActivateResponse, JoinRequest, JoinResponse, NodeDescriptor, NodeProfile,
        PingRequest, PingResponse, system_service_server::SystemService,
    },
};
use oes_storage::{ReplicaCommitment, ReplicaStore, StorageError, WriteReplicaRequest};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming, metadata::MetadataMap};
use tracing::{Instrument, info, info_span, warn};

use crate::{
    peer::{AuthenticationRequirement, JOIN_TOKEN_HEADER, PeerIdentity, PeerVerifier},
    trace::TraceContext,
};

/// Maximum bytes accepted in one replica transfer chunk.
const MAXIMUM_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Chunk size used when streaming a replica out.
const READ_CHUNK_BYTES: usize = 256 * 1024;

/// Depth of the outbound read queue, which bounds memory per reader.
const READ_QUEUE_DEPTH: usize = 8;

/// Depth of the inbound transfer queue, which bounds memory per writer.
const INBOUND_QUEUE_DEPTH: usize = 4;

fn storage_status(error: &StorageError) -> Status {
    match error {
        StorageError::ObjectNotFound | StorageError::InconsistentState => {
            Status::not_found(error.to_string())
        }
        StorageError::ChecksumMismatch { .. } | StorageError::IntegrityMismatch => {
            Status::data_loss(error.to_string())
        }
        StorageError::InvalidRequest(_) => Status::invalid_argument(error.to_string()),
        StorageError::EncryptionKeyRequired | StorageError::EncryptionKeyMismatch => {
            Status::failed_precondition(error.to_string())
        }
        _ => Status::internal(error.to_string()),
    }
}

fn consensus_status(error: &ConsensusError) -> Status {
    if error.retryable() {
        Status::unavailable(error.to_string())
    } else {
        match error {
            ConsensusError::NotLeader { .. } => Status::failed_precondition(error.to_string()),
            ConsensusError::Rejected(_) => Status::invalid_argument(error.to_string()),
            _ => Status::internal(error.to_string()),
        }
    }
}

/// Metadata consensus transport service.
pub struct ConsensusRpcService {
    consensus: Arc<MetadataConsensus>,
    verifier: Arc<PeerVerifier>,
}

impl ConsensusRpcService {
    /// Creates the service for one node.
    #[must_use]
    pub const fn new(consensus: Arc<MetadataConsensus>, verifier: Arc<PeerVerifier>) -> Self {
        Self {
            consensus,
            verifier,
        }
    }

    async fn authorize(&self, metadata: &MetadataMap) -> Result<PeerIdentity, Status> {
        self.verifier
            .verify(metadata, AuthenticationRequirement::Required)
            .await
            .map_err(Status::from)
    }
}

#[tonic::async_trait]
impl ConsensusService for ConsensusRpcService {
    async fn append_entries(
        &self,
        request: Request<ConsensusEnvelope>,
    ) -> Result<Response<ConsensusReply>, Status> {
        let peer = self.authorize(request.metadata()).await?;
        let span = info_span!("consensus.append_entries", trace_id = %peer.trace.trace_id);
        async move {
            let decoded = serde_json::from_slice(&request.into_inner().payload)
                .map_err(|error| Status::invalid_argument(error.to_string()))?;
            let response = self
                .consensus
                .handle_append_entries(decoded)
                .await
                .map_err(|error| consensus_status(&error))?;
            encode_reply(&response)
        }
        .instrument(span)
        .await
    }

    async fn vote(
        &self,
        request: Request<ConsensusEnvelope>,
    ) -> Result<Response<ConsensusReply>, Status> {
        let peer = self.authorize(request.metadata()).await?;
        let span = info_span!("consensus.vote", trace_id = %peer.trace.trace_id);
        async move {
            let decoded = serde_json::from_slice(&request.into_inner().payload)
                .map_err(|error| Status::invalid_argument(error.to_string()))?;
            let response = self
                .consensus
                .handle_vote(decoded)
                .await
                .map_err(|error| consensus_status(&error))?;
            encode_reply(&response)
        }
        .instrument(span)
        .await
    }

    async fn install_snapshot(
        &self,
        request: Request<ConsensusEnvelope>,
    ) -> Result<Response<ConsensusReply>, Status> {
        let peer = self.authorize(request.metadata()).await?;
        let span = info_span!("consensus.install_snapshot", trace_id = %peer.trace.trace_id);
        async move {
            let decoded = serde_json::from_slice(&request.into_inner().payload)
                .map_err(|error| Status::invalid_argument(error.to_string()))?;
            let response = self
                .consensus
                .handle_install_snapshot(decoded)
                .await
                .map_err(|error| consensus_status(&error))?;
            encode_reply(&response)
        }
        .instrument(span)
        .await
    }

    async fn forward_write(
        &self,
        request: Request<ForwardWriteRequest>,
    ) -> Result<Response<ForwardWriteResponse>, Status> {
        let peer = self.authorize(request.metadata()).await?;
        let span = info_span!("consensus.forward_write", trace_id = %peer.trace.trace_id);
        async move {
            let command: ClusterWrite = serde_json::from_slice(&request.into_inner().command)
                .map_err(|error| Status::invalid_argument(error.to_string()))?;
            let response = self
                .consensus
                .write(command)
                .await
                .map_err(|error| consensus_status(&error))?;
            let encoded = serde_json::to_vec(&response)
                .map_err(|error| Status::internal(error.to_string()))?;
            Ok(Response::new(ForwardWriteResponse { response: encoded }))
        }
        .instrument(span)
        .await
    }

    async fn read_barrier(
        &self,
        request: Request<ReadBarrierRequest>,
    ) -> Result<Response<ReadBarrierResponse>, Status> {
        self.authorize(request.metadata()).await?;
        let index = self
            .consensus
            .read_barrier_index()
            .await
            .map_err(|error| consensus_status(&error))?;
        Ok(Response::new(ReadBarrierResponse {
            has_index: index.is_some(),
            index: index.unwrap_or_default(),
        }))
    }
}

fn encode_reply<T: serde::Serialize>(value: &T) -> Result<Response<ConsensusReply>, Status> {
    let payload = serde_json::to_vec(value).map_err(|error| Status::internal(error.to_string()))?;
    Ok(Response::new(ConsensusReply { payload }))
}

/// Replica transfer and integrity service.
pub struct ReplicaRpcService {
    storage: Arc<dyn ReplicaStore>,
    verifier: Arc<PeerVerifier>,
    payload_format: PayloadFormat,
}

impl ReplicaRpcService {
    /// Creates the service for one node.
    #[must_use]
    pub const fn new(
        storage: Arc<dyn ReplicaStore>,
        verifier: Arc<PeerVerifier>,
        payload_format: PayloadFormat,
    ) -> Self {
        Self {
            storage,
            verifier,
            payload_format,
        }
    }

    async fn authorize(&self, metadata: &MetadataMap) -> Result<PeerIdentity, Status> {
        self.verifier
            .verify(metadata, AuthenticationRequirement::Required)
            .await
            .map_err(Status::from)
    }
}

#[tonic::async_trait]
impl ReplicaService for ReplicaRpcService {
    type ReadReplicaStream = ReceiverStream<Result<ReadReplicaChunk, Status>>;

    async fn write_replica(
        &self,
        request: Request<Streaming<WriteReplicaChunk>>,
    ) -> Result<Response<WriteReplicaResponse>, Status> {
        let peer = self.authorize(request.metadata()).await?;
        let span = info_span!(
            "replica.write",
            trace_id = %peer.trace.trace_id,
            source = %peer.node_id
        );
        let storage = Arc::clone(&self.storage);
        async move {
            let mut inbound = request.into_inner();
            let first = inbound
                .next()
                .await
                .transpose()?
                .ok_or_else(|| Status::invalid_argument("replica transfer sent no header"))?;
            let Some(write_replica_chunk::Body::Header(header)) = first.body else {
                return Err(Status::invalid_argument(
                    "the first replica transfer frame must be a header",
                ));
            };
            let descriptor = header
                .descriptor
                .ok_or_else(|| Status::invalid_argument("replica header has no descriptor"))?;
            let object_id: ObjectId = descriptor.object_id.parse().map_err(|_| {
                Status::invalid_argument("replica header has an invalid payload id")
            })?;
            if header.operation_id.is_empty() || header.operation_id.len() > 128 {
                return Err(Status::invalid_argument(
                    "replica header has an invalid operation identity",
                ));
            }
            // An empty header checksum means the source only learns the
            // expectation after its last byte, which is the case for a client
            // upload being replicated as it streams in.
            let declared = if descriptor.checksum.is_empty() {
                None
            } else {
                Some(ReplicaCommitment {
                    size: descriptor.size,
                    checksum: descriptor.checksum.parse::<Checksum>().map_err(|_| {
                        Status::invalid_argument("replica header has an invalid checksum")
                    })?,
                })
            };

            // Frames are forwarded through a bounded channel so a fast sender
            // cannot grow this node's memory beyond the queued chunks.
            let (data_sender, data_receiver) =
                mpsc::channel::<Result<Bytes, std::io::Error>>(INBOUND_QUEUE_DEPTH);
            let (commit_sender, commit_receiver) = tokio::sync::oneshot::channel();
            let operation_id = header.operation_id.clone();
            tokio::spawn(async move {
                let mut commit_sender = Some(commit_sender);
                let mut failure: Option<String> = None;
                while let Some(frame) = inbound.next().await {
                    match frame {
                        Ok(WriteReplicaChunk {
                            body: Some(write_replica_chunk::Body::Data(data)),
                        }) => {
                            if data.len() > MAXIMUM_CHUNK_BYTES {
                                failure =
                                    Some("replica chunk exceeds the accepted size".to_owned());
                                break;
                            }
                            if data_sender.send(Ok(Bytes::from(data))).await.is_err() {
                                return;
                            }
                        }
                        Ok(WriteReplicaChunk {
                            body: Some(write_replica_chunk::Body::Commit(commit)),
                        }) => {
                            let parsed = commit
                                .checksum
                                .parse::<Checksum>()
                                .map(|checksum| ReplicaCommitment {
                                    size: commit.size,
                                    checksum,
                                })
                                .map_err(|error| error.to_string());
                            drop(data_sender);
                            if let Some(sender) = commit_sender.take() {
                                let _ = sender.send(parsed);
                            }
                            return;
                        }
                        Ok(WriteReplicaChunk {
                            body: Some(write_replica_chunk::Body::Header(_)),
                        }) => {
                            failure = Some("replica transfer sent a second header".to_owned());
                            break;
                        }
                        Ok(WriteReplicaChunk { body: None }) => {}
                        Err(status) => {
                            failure = Some(status.message().to_owned());
                            break;
                        }
                    }
                }
                let reason = failure
                    .unwrap_or_else(|| "replica transfer ended without a commitment".to_owned());
                let _ = data_sender
                    .send(Err(std::io::Error::other(reason.clone())))
                    .await;
                drop(data_sender);
                if let Some(sender) = commit_sender.take() {
                    let _ = sender.send(Err(reason));
                }
            });

            let body = oes_storage::upload_stream(ReceiverStream::new(data_receiver));
            let request = match declared {
                Some(commitment) => WriteReplicaRequest::known(
                    operation_id.clone(),
                    object_id,
                    commitment.size,
                    commitment.checksum,
                    body,
                ),
                None => WriteReplicaRequest::trailing(
                    operation_id.clone(),
                    object_id,
                    commit_receiver,
                    body,
                ),
            };
            let result = storage.write_replica(request).await.map_err(|error| {
                warn!(%error, %object_id, "replica transfer refused");
                storage_status(&error)
            })?;
            Ok(Response::new(WriteReplicaResponse {
                operation_id,
                object_id: result.object_id.to_string(),
                size: result.size,
                checksum: result.checksum.to_string(),
                already_present: result.already_present,
            }))
        }
        .instrument(span)
        .await
    }

    async fn read_replica(
        &self,
        request: Request<ReadReplicaRequest>,
    ) -> Result<Response<Self::ReadReplicaStream>, Status> {
        let peer = self.authorize(request.metadata()).await?;
        let input = request.into_inner();
        let object_id: ObjectId = input
            .object_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid payload identifier"))?;
        let whole_payload = input.whole_payload || (input.offset == 0 && input.length == 0);
        let expected = if whole_payload {
            Some(
                input
                    .checksum
                    .parse::<Checksum>()
                    .map_err(|_| Status::invalid_argument("invalid checksum"))?,
            )
        } else {
            None
        };
        let range = if whole_payload {
            None
        } else {
            Some(
                oes_core::ByteRange::new(input.offset, input.length)
                    .map_err(|error| Status::invalid_argument(error.to_string()))?,
            )
        };
        let opened = self
            .storage
            .read_replica(oes_storage::ReadReplicaRequest {
                object_id,
                size: input.size,
                payload_format: self.payload_format,
                range,
                expected_checksum: expected,
            })
            .await
            .map_err(|error| storage_status(&error))?;

        let (sender, receiver) = mpsc::channel(READ_QUEUE_DEPTH);
        let trace = peer.trace.clone();
        tokio::spawn(
            async move {
                let mut body = opened.body;
                let mut pending = Vec::with_capacity(READ_CHUNK_BYTES);
                while let Some(chunk) = body.next().await {
                    match chunk {
                        Ok(chunk) => {
                            pending.extend_from_slice(&chunk);
                            while pending.len() >= READ_CHUNK_BYTES {
                                let rest = pending.split_off(READ_CHUNK_BYTES);
                                let frame = std::mem::replace(&mut pending, rest);
                                if sender
                                    .send(Ok(ReadReplicaChunk { data: frame }))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(storage_status(&error))).await;
                            return;
                        }
                    }
                }
                if !pending.is_empty() {
                    let _ = sender.send(Ok(ReadReplicaChunk { data: pending })).await;
                }
            }
            .instrument(info_span!("replica.read", trace_id = %trace.trace_id)),
        );
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn delete_replica(
        &self,
        request: Request<DeleteReplicaRequest>,
    ) -> Result<Response<DeleteReplicaResponse>, Status> {
        self.authorize(request.metadata()).await?;
        let object_id: ObjectId = request
            .into_inner()
            .object_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid payload identifier"))?;
        let removed = self
            .storage
            .delete_replica(object_id)
            .await
            .map_err(|error| storage_status(&error))?;
        Ok(Response::new(DeleteReplicaResponse { removed }))
    }

    async fn verify_replica(
        &self,
        request: Request<VerifyReplicaRequest>,
    ) -> Result<Response<VerifyReplicaResponse>, Status> {
        self.authorize(request.metadata()).await?;
        let input = request.into_inner();
        let object_id: ObjectId = input
            .object_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid payload identifier"))?;
        let checksum: Checksum = input
            .checksum
            .parse()
            .map_err(|_| Status::invalid_argument("invalid checksum"))?;
        let verification = self
            .storage
            .verify_replica(object_id, input.size, self.payload_format, checksum)
            .await
            .map_err(|error| storage_status(&error))?;
        Ok(Response::new(VerifyReplicaResponse {
            present: verification.present,
            matches: verification.matches,
            size: verification.size,
            checksum: verification
                .checksum
                .map(|checksum| checksum.to_string())
                .unwrap_or_default(),
        }))
    }

    async fn stat_replica(
        &self,
        request: Request<StatReplicaRequest>,
    ) -> Result<Response<StatReplicaResponse>, Status> {
        self.authorize(request.metadata()).await?;
        let object_id: ObjectId = request
            .into_inner()
            .object_id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid payload identifier"))?;
        let stat = self
            .storage
            .stat_replica(object_id)
            .await
            .map_err(|error| storage_status(&error))?;
        Ok(Response::new(StatReplicaResponse {
            present: stat.is_some(),
            physical_bytes: stat.map(|stat| stat.physical_bytes).unwrap_or_default(),
        }))
    }

    async fn list_local_payloads(
        &self,
        request: Request<ListLocalPayloadsRequest>,
    ) -> Result<Response<ListLocalPayloadsResponse>, Status> {
        self.authorize(request.metadata()).await?;
        let input = request.into_inner();
        let after = if input.after_object_id.is_empty() {
            None
        } else {
            Some(
                input
                    .after_object_id
                    .parse::<ObjectId>()
                    .map_err(|_| Status::invalid_argument("invalid cursor"))?,
            )
        };
        let limit = usize::try_from(input.limit)
            .unwrap_or(1_000)
            .clamp(1, 10_000);
        let payloads = self
            .storage
            .list_local_payloads(after, limit)
            .await
            .map_err(|error| storage_status(&error))?;
        Ok(Response::new(ListLocalPayloadsResponse {
            object_ids: payloads.iter().map(|id| id.to_string()).collect(),
        }))
    }
}

/// A node asking to join the cluster.
#[derive(Debug, Clone)]
pub struct NodeJoinRequest {
    /// Single-use join token presented by the operator.
    pub join_token: String,
    /// What the joining node reports about itself.
    pub registration: NodeRegistration,
    /// Whether the node stores object replicas.
    pub storage_node: bool,
    /// Advertised consensus versions.
    pub versions: NodeVersions,
}

/// What the cluster grants a joining node.
#[derive(Debug, Clone)]
pub struct JoinOutcome {
    /// Cluster the node is now bound to.
    pub cluster_id: ClusterId,
    /// Consensus member identifier assigned to the node.
    pub member_id: u64,
    /// Node credential shown exactly once.
    pub node_credential: String,
    /// Whether the node should vote in metadata consensus.
    pub metadata_voter: bool,
    /// Cluster-wide configuration in effect.
    pub cluster_config: ClusterConfig,
}

/// Cluster admission decisions, implemented by the cluster runtime.
#[async_trait]
pub trait ClusterAdmission: Send + Sync {
    /// Validates a join token and registers the node.
    async fn join(&self, request: NodeJoinRequest) -> Result<JoinOutcome, ConsensusError>;

    /// Adds an already-bound node to the consensus group.
    async fn activate(
        &self,
        node_id: NodeId,
        member_id: u64,
        address: String,
        voter: bool,
    ) -> Result<bool, ConsensusError>;

    /// Returns this node's descriptor for diagnostics.
    async fn descriptor(&self) -> NodeDescriptor;

    /// Returns the current metadata leader's address, if one is known.
    async fn metadata_leader(&self) -> Option<(u64, String)>;
}

/// Node lifecycle and diagnostics service.
pub struct SystemRpcService {
    admission: Arc<dyn ClusterAdmission>,
    verifier: Arc<PeerVerifier>,
}

impl SystemRpcService {
    /// Creates the service for one node.
    #[must_use]
    pub const fn new(admission: Arc<dyn ClusterAdmission>, verifier: Arc<PeerVerifier>) -> Self {
        Self {
            admission,
            verifier,
        }
    }
}

#[tonic::async_trait]
impl SystemService for SystemRpcService {
    async fn ping(&self, request: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        // A node that has not joined yet has no credential, so compatibility
        // probing is allowed unauthenticated. It therefore reveals nothing
        // beyond this node's own descriptor.
        let peer = self
            .verifier
            .verify(request.metadata(), AuthenticationRequirement::Optional)
            .await
            .map_err(Status::from)?;
        let descriptor = self.admission.descriptor().await;
        let leader = if peer.authenticated {
            self.admission.metadata_leader().await
        } else {
            None
        };
        Ok(Response::new(PingResponse {
            peer: Some(descriptor),
            metadata_leader: leader.is_some(),
            metadata_leader_address: leader.map(|(_, address)| address).unwrap_or_default(),
        }))
    }

    async fn join(&self, request: Request<JoinRequest>) -> Result<Response<JoinResponse>, Status> {
        // Joining is authenticated by the single-use token rather than a node
        // credential, because the node does not have one yet.
        let trace = TraceContext::from_metadata(request.metadata());
        let token = request
            .metadata()
            .get(JOIN_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let input = request.into_inner();
        let token = token
            .or_else(|| Some(input.join_token.clone()))
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Status::unauthenticated("a join token is required"))?;
        let descriptor = input
            .peer
            .ok_or_else(|| Status::invalid_argument("join request has no node descriptor"))?;
        let profile = input
            .profile
            .ok_or_else(|| Status::invalid_argument("join request has no node profile"))?;
        let registration = registration_from(&descriptor, &profile)?;
        let versions = versions_from(&descriptor);
        let span =
            info_span!("cluster.join", trace_id = %trace.trace_id, node = %registration.node_id);
        let outcome = self
            .admission
            .join(NodeJoinRequest {
                join_token: token,
                registration,
                storage_node: descriptor.storage_node,
                versions,
            })
            .instrument(span)
            .await
            .map_err(|error| consensus_status(&error))?;
        let cluster_config = serde_json::to_string(&outcome.cluster_config)
            .map_err(|error| Status::internal(error.to_string()))?;
        info!(member = outcome.member_id, "node joined the cluster");
        Ok(Response::new(JoinResponse {
            cluster_id: outcome.cluster_id.to_string(),
            member_id: outcome.member_id,
            node_credential: outcome.node_credential,
            metadata_voter: outcome.metadata_voter,
            cluster_config,
        }))
    }

    async fn activate(
        &self,
        request: Request<ActivateRequest>,
    ) -> Result<Response<ActivateResponse>, Status> {
        let peer = self
            .verifier
            .verify(request.metadata(), AuthenticationRequirement::Required)
            .await
            .map_err(Status::from)?;
        let input = request.into_inner();
        let descriptor = input
            .peer
            .ok_or_else(|| Status::invalid_argument("activation has no node descriptor"))?;
        let node_id: NodeId = descriptor
            .node_id
            .parse()
            .map_err(|_| Status::invalid_argument("activation has an invalid node identity"))?;
        if node_id != peer.node_id {
            return Err(Status::permission_denied(
                "a node may only activate its own membership",
            ));
        }
        let activated = self
            .admission
            .activate(node_id, descriptor.member_id, descriptor.rpc_address, false)
            .await
            .map_err(|error| consensus_status(&error))?;
        Ok(Response::new(ActivateResponse {
            activated,
            metadata_voter: false,
        }))
    }
}

fn registration_from(
    descriptor: &NodeDescriptor,
    profile: &NodeProfile,
) -> Result<NodeRegistration, Status> {
    let node_id: NodeId = descriptor
        .node_id
        .parse()
        .map_err(|_| Status::invalid_argument("invalid node identity"))?;
    let storage_class = StorageClass::new(if profile.storage_class.is_empty() {
        StorageClass::DEFAULT.to_owned()
    } else {
        profile.storage_class.clone()
    })
    .map_err(|error| Status::invalid_argument(error.to_string()))?;
    let failure_domain = FailureDomain::new(profile.failure_domain.clone().into_iter().collect())
        .map_err(|error| Status::invalid_argument(error.to_string()))?;
    let started_at: DateTime<Utc> = profile
        .started_at
        .parse::<DateTime<Utc>>()
        .unwrap_or_else(|_| Utc::now());
    Ok(NodeRegistration {
        node_id,
        versions: versions_from(descriptor),
        rpc_address: descriptor.rpc_address.clone(),
        s3_endpoint: (!profile.s3_endpoint.is_empty()).then(|| profile.s3_endpoint.clone()),
        storage_class,
        failure_domain,
        capacity: NodeCapacity {
            total_bytes: profile.total_bytes,
            available_bytes: profile.available_bytes,
            replica_bytes: profile.replica_bytes,
            temporary_bytes: profile.temporary_bytes,
        },
        started_at,
    })
}

fn versions_from(descriptor: &NodeDescriptor) -> NodeVersions {
    NodeVersions {
        protocol: ProtocolVersion::new(
            descriptor.protocol_major_version,
            descriptor.protocol_minor_version,
        ),
        software: descriptor.software_version.clone(),
        storage_format: descriptor.storage_format_version,
        cluster_format: descriptor.cluster_format_version,
    }
}
