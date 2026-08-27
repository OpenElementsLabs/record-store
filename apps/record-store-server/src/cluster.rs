//! Cluster-mode bootstrap and lifecycle wiring.

use std::{
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use record_store_cluster::{
    CapacityAwarePlacement, ClusterCommand, ClusterIdentity, FailureDomain, NodeCapacity,
    NodeCredential, NodeIdentity, NodeIdentityStore, NodeRegistration, NodeState, NodeVersions,
    StorageClass,
};
use record_store_config::{Config, DeploymentMode};
use record_store_consensus::{
    ClusterStore, ClusterWrite, ConsensusSettings, MetadataConsensus, ReplicatedClusterStore,
    ReplicatedMetadataRepository,
};
use record_store_core::{ClusterId, PayloadFormat};
use record_store_metadata::MetadataRepository;
use record_store_protocol::system_v1::{NodeDescriptor, NodeProfile};
use record_store_replication::{
    ClusterContext, ClusterOperations, ClusterRuntime, Coordinator, CoordinatorSettings,
    DistributedObjectStore, DistributedSettings, RuntimeSettings, TaskHealth,
};
use record_store_rpc::{
    CatalogPeerAuthenticator, ConsensusNetwork, ConsensusRpcService, InternalRpcServer,
    PeerHeaders, PeerPool, PeerVerifier, ReplicaRpcService, RpcClientSettings, RpcLeaderForwarder,
    RpcReplicaTransport, RpcServerError, RpcServerSettings, SystemRpcService, TlsSettings,
};
use record_store_storage::{LocalFilesystemStore, ObjectStore, ReplicaStore, StorageError};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Dependencies supplied to the HTTP/S3 layers in cluster mode.
pub struct ClusterDependencies {
    pub storage: Arc<dyn ObjectStore>,
    pub metadata: Arc<dyn MetadataRepository>,
    pub context: Arc<ClusterContext>,
    pub consensus: Arc<MetadataConsensus>,
    pub operations: Arc<ClusterOperations>,
    pub task_health: Arc<TaskHealth>,
    pub process: ClusterProcess,
}

/// Running cluster-only services owned by the server process.
pub struct ClusterProcess {
    runtime: ClusterRuntime,
    consensus: Arc<MetadataConsensus>,
    rpc_cancellation: CancellationToken,
    rpc: JoinHandle<Result<(), RpcServerError>>,
}

impl ClusterProcess {
    /// Supervises the internal listener and shuts every cluster task down.
    pub async fn supervise<F>(self, shutdown: F) -> Result<(), ClusterStartupError>
    where
        F: Future<Output = ()>,
    {
        let Self {
            runtime,
            consensus,
            rpc_cancellation,
            mut rpc,
        } = self;
        tokio::select! {
            result = &mut rpc => {
                runtime.shutdown().await;
                consensus.shutdown().await;
                match result {
                    Ok(result) => result.map_err(ClusterStartupError::Rpc),
                    Err(error) => Err(ClusterStartupError::RpcTask(error.to_string())),
                }
            }
            () = shutdown => {
                rpc_cancellation.cancel();
                runtime.shutdown().await;
                consensus.shutdown().await;
                match rpc.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(ClusterStartupError::Rpc(error)),
                    Err(error) if error.is_cancelled() => Ok(()),
                    Err(error) => Err(ClusterStartupError::RpcTask(error.to_string())),
                }
            }
        }
    }
}

/// Starts the durable identity, consensus, internal RPC, and replicated store.
pub async fn initialize(config: &Config) -> Result<ClusterDependencies, ClusterStartupError> {
    let data_directory = &config.storage.data_directory;
    let identity_store = NodeIdentityStore::new(data_directory);
    let mut identity = identity_store.load_or_create(Utc::now())?;
    let versions = NodeVersions::current(env!("CARGO_PKG_VERSION"));
    let tls = tls_settings(config);
    tls.validate()?;
    let credential_store = LocalCredentialStore::new(data_directory);
    let mut credential = credential_store.load()?;
    let advertise_address = config.server.effective_rpc_advertise();
    let profile = node_profile(config, measure_capacity(data_directory));
    let mut bootstrap_peer = None;
    if !identity.is_bound() {
        if config.cluster.seeds.is_empty() {
            let cluster_id = ClusterId::new();
            identity = identity_store.bind(cluster_id, 1, Utc::now())?;
            let issued = NodeCredential::issue(identity.node_id, Utc::now());
            credential_store.save(issued.secret.expose(), Some(&issued.record))?;
            credential = Some(LocalCredential {
                secret: issued.secret.expose().to_owned(),
                record: Some(issued.record),
            });
        } else {
            let join_token = config
                .cluster
                .join_token
                .as_ref()
                .ok_or(ClusterStartupError::JoinTokenRequired)?;
            let bootstrap_pool = PeerPool::new(RpcClientSettings::new(
                peer_headers(&identity, &versions, None),
                tls.clone(),
            ));
            let descriptor = node_descriptor(
                &identity,
                &versions,
                &advertise_address,
                config.server.mode.stores_replicas(),
            );
            let (outcome, seed_node_id) = join_from_seed(
                &bootstrap_pool,
                &config.cluster.seeds,
                join_token.expose(),
                descriptor,
                profile.clone(),
            )
            .await?;
            bootstrap_peer = Some(seed_node_id);
            identity = identity_store.bind(outcome.cluster_id, outcome.member_id, Utc::now())?;
            credential_store.save(&outcome.node_credential, None)?;
            credential = Some(LocalCredential {
                secret: outcome.node_credential,
                record: None,
            });
        }
    }

    let member_id = identity
        .raft_id
        .ok_or(ClusterStartupError::MissingMemberId)?;
    let credential = credential.ok_or(ClusterStartupError::MissingNodeCredential)?;
    let pool = PeerPool::new(RpcClientSettings::new(
        peer_headers(&identity, &versions, Some(credential.secret.clone())),
        tls.clone(),
    ));
    let mut consensus_settings = ConsensusSettings::new(
        member_id,
        &advertise_address,
        data_directory.join("metadata").join("consensus"),
    );
    consensus_settings.heartbeat_interval_millis = config.cluster.consensus_heartbeat_millis;
    consensus_settings.election_timeout_min_millis = config.cluster.election_timeout_min_millis;
    consensus_settings.election_timeout_max_millis = config.cluster.election_timeout_max_millis;
    consensus_settings.snapshot_logs_threshold = config.cluster.snapshot_logs_threshold;
    consensus_settings.retained_logs = config.cluster.retained_logs;
    let consensus =
        MetadataConsensus::start(consensus_settings, ConsensusNetwork::new(Arc::clone(&pool)))
            .await?;
    let needs_activation = !consensus.is_initialized().await?;
    consensus
        .set_leader_forwarder(Arc::new(RpcLeaderForwarder::new(Arc::clone(&pool))))
        .await;
    if bootstrap_peer.is_none() && needs_activation && !config.cluster.seeds.is_empty() {
        bootstrap_peer = Some(
            probe_seed(
                &pool,
                &config.cluster.seeds,
                node_descriptor(
                    &identity,
                    &versions,
                    &advertise_address,
                    config.server.mode.stores_replicas(),
                ),
            )
            .await?,
        );
    }

    let metadata: Arc<dyn MetadataRepository> =
        Arc::new(ReplicatedMetadataRepository::new(Arc::clone(&consensus)));
    let cluster: Arc<dyn ClusterStore> =
        Arc::new(ReplicatedClusterStore::new(Arc::clone(&consensus)));
    if config.cluster.seeds.is_empty() {
        bootstrap_cluster(
            config,
            &identity,
            &versions,
            &profile,
            credential.record.as_ref(),
            &consensus,
        )
        .await?;
    }
    // The physical replica store performs only node-local crash recovery. It
    // reads the locally applied state machine directly so a joining learner can
    // start its RPC listener before the leader installs the first snapshot.
    // Public metadata reads and every mutation still use the replicated adapter.
    let local_metadata: Arc<dyn MetadataRepository> =
        Arc::new(consensus.state().metadata().clone());
    let local = Arc::new(open_local_store(config, local_metadata).await?);
    let local_replica: Arc<dyn ReplicaStore> = local.clone();
    let transport = Arc::new(RpcReplicaTransport::new(Arc::clone(&pool)));
    let context = Arc::new(ClusterContext {
        node_id: identity.node_id,
        cluster: Arc::clone(&cluster),
        metadata: Arc::clone(&metadata),
        local: Arc::clone(&local_replica),
        transport,
        placement: Arc::new(CapacityAwarePlacement::new(
            config
                .server
                .mode
                .stores_replicas()
                .then_some(identity.node_id),
        )),
        consensus: Some(Arc::clone(&consensus)),
    });

    let authenticator = Arc::new(CatalogPeerAuthenticator::new(Arc::clone(&cluster)));
    let mut verifier = PeerVerifier::new(versions.clone(), authenticator);
    if let (Some(cluster_id), Some(seed_node_id)) = (identity.cluster_id, bootstrap_peer) {
        verifier = verifier.with_bootstrap_peer(cluster_id, seed_node_id);
    }
    let verifier = Arc::new(verifier);
    let admission = Arc::new(record_store_replication::JoinCoordinator::new(
        Arc::clone(&context),
        Arc::clone(&consensus),
        versions.clone(),
        config.server.mode.stores_replicas(),
        advertise_address.clone(),
    ));
    let rpc_server = InternalRpcServer::new(RpcServerSettings {
        bind: config.server.rpc_bind,
        tls,
        concurrency_limit: config.limits.maximum_concurrent_operations,
        shutdown_grace_period: Duration::from_secs(config.server.shutdown_grace_period_seconds),
    })
    .with_consensus(ConsensusRpcService::new(
        Arc::clone(&consensus),
        Arc::clone(&verifier),
    ))
    .with_system(SystemRpcService::new(admission, Arc::clone(&verifier)));
    let rpc_server = if config.server.mode.stores_replicas() {
        rpc_server.with_replica(ReplicaRpcService::new(
            Arc::clone(&local_replica),
            verifier,
            payload_format(config),
        ))
    } else {
        rpc_server
    };
    let rpc_listener = rpc_server.bind().await?;
    let rpc_cancellation = CancellationToken::new();
    let rpc_shutdown = rpc_cancellation.clone().cancelled_owned();
    let rpc = tokio::spawn(rpc_server.serve(rpc_listener, rpc_shutdown));

    let membership_result: Result<(), ClusterStartupError> = async {
        if needs_activation && !config.cluster.seeds.is_empty() {
            activate_with_seed(
                &pool,
                &config.cluster.seeds,
                node_descriptor(
                    &identity,
                    &versions,
                    &advertise_address,
                    config.server.mode.stores_replicas(),
                ),
                profile.clone(),
            )
            .await?;
        }
        update_local_membership(&context, &identity, &versions, config, &profile).await
    }
    .await;
    if let Err(error) = membership_result {
        rpc_cancellation.cancel();
        let _ = rpc.await;
        consensus.shutdown().await;
        return Err(error);
    }
    let storage: Arc<dyn ObjectStore> = Arc::new(DistributedObjectStore::new(
        Arc::clone(&context),
        DistributedSettings::new(payload_format(config)),
    ));
    let coordinator = Arc::new(Coordinator::new(
        Arc::clone(&context),
        Arc::clone(&consensus),
        CoordinatorSettings::default(),
    ));
    let operations = Arc::new(ClusterOperations::new(
        Arc::clone(&context),
        coordinator,
        Arc::clone(&consensus),
    ));
    let mut runtime = ClusterRuntime::new(
        Arc::clone(&context),
        Arc::clone(&consensus),
        runtime_settings(config),
    );
    if config.server.mode.stores_replicas() {
        runtime = runtime.with_storage(Arc::clone(&storage));
    }
    let task_health = runtime.health();
    let cluster_config = context
        .config()
        .await
        .map_err(ClusterStartupError::Storage)?;
    runtime.start(
        Duration::from_secs(cluster_config.failure_detection.heartbeat_interval_seconds),
        record_store_replication::tasks::MovementLimits {
            concurrency: config.cluster.movement_concurrency,
            bytes_per_second: config.cluster.movement_bytes_per_second,
            lease: Duration::from_secs(cluster_config.repair.lease_seconds),
            maximum_attempts: cluster_config.repair.maximum_attempts,
        },
    );
    info!(
        node = %identity.node_id,
        cluster = %identity.cluster_id.map(|value| value.to_string()).unwrap_or_default(),
        member = member_id,
        mode = %config.server.mode,
        "cluster runtime initialized"
    );
    Ok(ClusterDependencies {
        storage,
        metadata,
        context,
        consensus: Arc::clone(&consensus),
        operations,
        task_health,
        process: ClusterProcess {
            runtime,
            consensus,
            rpc_cancellation,
            rpc,
        },
    })
}

async fn bootstrap_cluster(
    config: &Config,
    identity: &NodeIdentity,
    versions: &NodeVersions,
    profile: &NodeProfile,
    credential: Option<&NodeCredential>,
    consensus: &Arc<MetadataConsensus>,
) -> Result<(), ClusterStartupError> {
    if !consensus.is_initialized().await? {
        consensus.initialize_single_member().await?;
    }
    consensus.wait_for_leader(Duration::from_secs(10)).await?;
    if consensus.state().cluster().identity().await?.is_some() {
        return Ok(());
    }
    let cluster_id = identity
        .cluster_id
        .ok_or(ClusterStartupError::MissingClusterId)?;
    let registration = registration(identity, versions, config, profile)?;
    let credential = credential.ok_or(ClusterStartupError::CredentialRecordMissing)?;
    let response = consensus
        .write(ClusterWrite::batch([
            ClusterWrite::cluster(ClusterCommand::InitializeCluster {
                identity: ClusterIdentity {
                    cluster_id,
                    cluster_format_version: versions.cluster_format,
                    created_at: identity.created_at,
                },
                config: Box::new(cluster_config(config)),
            }),
            ClusterWrite::cluster(ClusterCommand::RegisterNode {
                registration: Box::new(registration),
                at: Utc::now(),
            }),
            ClusterWrite::cluster(ClusterCommand::PutNodeCredential {
                credential: Box::new(credential.clone()),
            }),
            ClusterWrite::cluster(ClusterCommand::SetNodeState {
                node_id: identity.node_id,
                state: NodeState::Healthy,
                reason: Some("initial cluster member activated".to_owned()),
                at: Utc::now(),
            }),
        ]))
        .await?;
    if let record_store_consensus::ClusterWriteResponse::Rejected(rejection) = response {
        return Err(ClusterStartupError::Consensus(
            record_store_consensus::ConsensusError::Rejected(rejection),
        ));
    }
    info!(cluster = %cluster_id, node = %identity.node_id, "initialized cluster metadata");
    Ok(())
}

fn cluster_config(config: &Config) -> record_store_cluster::ClusterConfig {
    let mut cluster = record_store_cluster::ClusterConfig::default();
    cluster.replication_factor = config.cluster.replication_factor;
    cluster.watermarks.low_percent = config.cluster.capacity_low_watermark_percent;
    cluster.watermarks.high_percent = config.cluster.capacity_high_watermark_percent;
    cluster.watermarks.critical_percent = config.cluster.capacity_critical_watermark_percent;
    cluster.repair.movement.maximum_concurrent_tasks =
        u32::try_from(config.cluster.movement_concurrency).unwrap_or(u32::MAX);
    cluster.repair.movement.maximum_bytes_per_second = config.cluster.movement_bytes_per_second;
    cluster.rebalance.movement.maximum_concurrent_tasks =
        u32::try_from(config.cluster.movement_concurrency).unwrap_or(u32::MAX);
    cluster.rebalance.movement.maximum_bytes_per_second = config.cluster.movement_bytes_per_second;
    cluster
}

/// How long startup waits for its own registration to become locally visible.
///
/// This bounds a wait on real conditions — leadership knowledge, then the
/// leader's read index — so a genuinely unreachable quorum fails startup
/// instead of hanging. It matches the consensus operation timeout.
const MEMBERSHIP_BARRIER_TIMEOUT: Duration = Duration::from_secs(15);

/// How often the membership barrier re-checks a momentarily unavailable quorum.
const MEMBERSHIP_BARRIER_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Blocks until this node's applied state includes every committed write.
///
/// A joining node's registration is committed by the leader, remotely. Reading
/// the local catalog straight afterwards races the replication and application
/// of that commit, which surfaced as a spurious `NodeNotRegistered` during
/// three-node startup. Waiting for the leader's read index establishes a real
/// consistency boundary: it is a condition wait, not a delay or a retry.
async fn establish_membership_read_barrier(
    context: &ClusterContext,
) -> Result<(), ClusterStartupError> {
    let Some(consensus) = context.consensus.as_deref() else {
        // Standalone has no replicated state, so a local read is already current.
        return Ok(());
    };
    let deadline = tokio::time::Instant::now() + MEMBERSHIP_BARRIER_TIMEOUT;
    // The barrier asks the leader for its read index, so the leader has to be
    // known first; a node that has only just been added does not know it yet.
    consensus
        .wait_for_leader(MEMBERSHIP_BARRIER_TIMEOUT)
        .await?;
    // Knowing the leader is not yet enough. A leader that has just admitted
    // this node still has to establish replication to it before the enlarged
    // voter set can answer a read index, and until it does it reports the
    // quorum as momentarily unavailable. That is a startup condition like the
    // leadership wait above, not a failure, so wait it out under the same
    // bound; a genuinely unreachable quorum still fails startup.
    loop {
        match consensus.ensure_read_consistency().await {
            Ok(()) => return Ok(()),
            Err(error) if error.retryable() && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(MEMBERSHIP_BARRIER_POLL_INTERVAL).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn update_local_membership(
    context: &ClusterContext,
    identity: &NodeIdentity,
    versions: &NodeVersions,
    config: &Config,
    profile: &NodeProfile,
) -> Result<(), ClusterStartupError> {
    establish_membership_read_barrier(context).await?;
    let node = context.cluster.node(identity.node_id).await?;
    let Some(node) = node else {
        return Err(ClusterStartupError::NodeNotRegistered(
            identity.node_id.to_string(),
        ));
    };
    let failure_domain = FailureDomain::new(profile.failure_domain.clone().into_iter().collect())?;
    context
        .commit(ClusterWrite::cluster(
            ClusterCommand::UpdateNodeDescriptor {
                node_id: identity.node_id,
                rpc_address: config.server.effective_rpc_advertise(),
                s3_endpoint: config.cluster.s3_endpoint.clone(),
                versions: Box::new(versions.clone()),
                storage_class: StorageClass::new(&config.cluster.storage_class)?,
                failure_domain,
                started_at: Utc::now(),
                at: Utc::now(),
            },
        ))
        .await?;
    if node.state == NodeState::Joining {
        context
            .commit(ClusterWrite::cluster(ClusterCommand::SetNodeState {
                node_id: identity.node_id,
                state: NodeState::Healthy,
                reason: Some("node activated and local storage recovered".to_owned()),
                at: Utc::now(),
            }))
            .await?;
    }
    Ok(())
}

fn registration(
    identity: &NodeIdentity,
    versions: &NodeVersions,
    config: &Config,
    profile: &NodeProfile,
) -> Result<NodeRegistration, ClusterStartupError> {
    Ok(NodeRegistration {
        node_id: identity.node_id,
        versions: versions.clone(),
        rpc_address: config.server.effective_rpc_advertise(),
        s3_endpoint: config.cluster.s3_endpoint.clone(),
        storage_class: StorageClass::new(&config.cluster.storage_class)?,
        failure_domain: FailureDomain::new(profile.failure_domain.clone().into_iter().collect())?,
        capacity: NodeCapacity {
            total_bytes: profile.total_bytes,
            available_bytes: profile.available_bytes,
            replica_bytes: profile.replica_bytes,
            temporary_bytes: profile.temporary_bytes,
        },
        started_at: Utc::now(),
    })
}

async fn join_from_seed(
    pool: &Arc<PeerPool>,
    seeds: &[String],
    token: &str,
    descriptor: NodeDescriptor,
    profile: NodeProfile,
) -> Result<(record_store_rpc::JoinOutcome, record_store_core::NodeId), ClusterStartupError> {
    let mut failures = Vec::new();
    for seed in seeds {
        let remote = match pool.probe_cluster(seed, descriptor.clone()).await {
            Ok(remote) => remote,
            Err(error) => {
                failures.push(format!("{seed}: {error}"));
                continue;
            }
        };
        let seed_node_id = match remote.node_id.parse() {
            Ok(node_id) => node_id,
            Err(_) => {
                failures.push(format!("{seed}: seed returned an invalid node identity"));
                continue;
            }
        };
        match pool
            .join_cluster(seed, token, descriptor.clone(), profile.clone())
            .await
        {
            Ok(outcome) => return Ok((outcome, seed_node_id)),
            Err(error) => failures.push(format!("{seed}: {error}")),
        }
    }
    Err(ClusterStartupError::Seeds(failures.join("; ")))
}

async fn probe_seed(
    pool: &Arc<PeerPool>,
    seeds: &[String],
    descriptor: NodeDescriptor,
) -> Result<record_store_core::NodeId, ClusterStartupError> {
    let mut failures = Vec::new();
    for seed in seeds {
        match pool.probe_cluster(seed, descriptor.clone()).await {
            Ok(remote) => match remote.node_id.parse() {
                Ok(node_id) => return Ok(node_id),
                Err(_) => failures.push(format!("{seed}: seed returned an invalid node identity")),
            },
            Err(error) => failures.push(format!("{seed}: {error}")),
        }
    }
    Err(ClusterStartupError::Seeds(failures.join("; ")))
}

async fn activate_with_seed(
    pool: &Arc<PeerPool>,
    seeds: &[String],
    descriptor: NodeDescriptor,
    profile: NodeProfile,
) -> Result<(), ClusterStartupError> {
    let mut failures = Vec::new();
    for seed in seeds {
        match pool
            .activate_cluster(seed, descriptor.clone(), profile.clone())
            .await
        {
            Ok(response) if response.activated => return Ok(()),
            Ok(_) => failures.push(format!("{seed}: activation was not accepted")),
            Err(error) => failures.push(format!("{seed}: {error}")),
        }
    }
    Err(ClusterStartupError::Seeds(failures.join("; ")))
}

fn node_descriptor(
    identity: &NodeIdentity,
    versions: &NodeVersions,
    advertise_address: &str,
    storage_node: bool,
) -> NodeDescriptor {
    NodeDescriptor {
        node_id: identity.node_id.to_string(),
        member_id: identity.raft_id.unwrap_or_default(),
        protocol_major_version: versions.protocol.major,
        protocol_minor_version: versions.protocol.minor,
        software_version: versions.software.clone(),
        storage_format_version: versions.storage_format,
        cluster_format_version: versions.cluster_format,
        cluster_id: identity
            .cluster_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        rpc_address: advertise_address.to_owned(),
        storage_node,
    }
}

fn node_profile(config: &Config, capacity: NodeCapacity) -> NodeProfile {
    let failure_domain = FailureDomain::parse(&config.cluster.failure_domain)
        .map(|domain| domain.labels().clone())
        .unwrap_or_default();
    NodeProfile {
        storage_class: config.cluster.storage_class.clone(),
        failure_domain: failure_domain.into_iter().collect(),
        total_bytes: capacity.total_bytes,
        available_bytes: capacity.available_bytes,
        replica_bytes: capacity.replica_bytes,
        temporary_bytes: capacity.temporary_bytes,
        started_at: Utc::now().to_rfc3339(),
        s3_endpoint: config.cluster.s3_endpoint.clone().unwrap_or_default(),
    }
}

fn measure_capacity(path: &Path) -> NodeCapacity {
    NodeCapacity {
        total_bytes: fs2::total_space(path).unwrap_or_default(),
        available_bytes: fs2::available_space(path).unwrap_or_default(),
        ..NodeCapacity::default()
    }
}

fn peer_headers(
    identity: &NodeIdentity,
    versions: &NodeVersions,
    credential: Option<String>,
) -> PeerHeaders {
    PeerHeaders {
        node_id: identity.node_id,
        cluster_id: identity.cluster_id,
        versions: versions.clone(),
        credential,
    }
}

fn tls_settings(config: &Config) -> TlsSettings {
    TlsSettings {
        certificate_path: config.cluster.tls.certificate_path.clone(),
        private_key_path: config.cluster.tls.private_key_path.clone(),
        peer_ca_path: config.cluster.tls.peer_ca_path.clone(),
        client_ca_path: config.cluster.tls.client_ca_path.clone(),
        server_name: config.cluster.tls.server_name.clone(),
    }
}

fn payload_format(config: &Config) -> PayloadFormat {
    if config.storage.encryption_enabled {
        PayloadFormat::Aes256GcmEnvelopeV1
    } else {
        PayloadFormat::Plaintext
    }
}

fn runtime_settings(config: &Config) -> RuntimeSettings {
    let mut settings = match config.server.mode {
        DeploymentMode::Cluster => RuntimeSettings::storage(payload_format(config)),
        DeploymentMode::Control => RuntimeSettings::control(),
        DeploymentMode::Standalone => RuntimeSettings::storage(payload_format(config)),
    };
    settings.reconcile_interval = Duration::from_secs(config.cluster.reconcile_interval_seconds);
    settings
}

async fn open_local_store(
    config: &Config,
    metadata: Arc<dyn MetadataRepository>,
) -> Result<LocalFilesystemStore, StorageError> {
    if config.storage.encryption_enabled {
        let master_key = config
            .auth
            .credential_master_key
            .as_ref()
            .ok_or(StorageError::EncryptionKeyRequired)?;
        LocalFilesystemStore::open_encrypted(
            &config.storage.data_directory,
            config.storage.effective_temporary_directory(),
            metadata,
            master_key.expose().as_bytes(),
        )
        .await
    } else {
        LocalFilesystemStore::open(
            &config.storage.data_directory,
            config.storage.effective_temporary_directory(),
            metadata,
        )
        .await
    }
}

const CREDENTIAL_FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialDocument {
    format_version: u32,
    credential: String,
    #[serde(default)]
    record: Option<NodeCredential>,
}

struct LocalCredential {
    secret: String,
    record: Option<NodeCredential>,
}

struct LocalCredentialStore {
    path: PathBuf,
}

impl LocalCredentialStore {
    fn new(data_directory: impl AsRef<Path>) -> Self {
        Self {
            path: data_directory.as_ref().join("node-credential.json"),
        }
    }

    fn load(&self) -> Result<Option<LocalCredential>, ClusterStartupError> {
        let encoded = match std::fs::read(&self.path) {
            Ok(encoded) => encoded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ClusterStartupError::CredentialIo(error)),
        };
        let document: CredentialDocument = serde_json::from_slice(&encoded)
            .map_err(|error| ClusterStartupError::CredentialMalformed(error.to_string()))?;
        if document.format_version != CREDENTIAL_FORMAT_VERSION
            || record_store_cluster::parse_node_credential(&document.credential).is_err()
        {
            return Err(ClusterStartupError::CredentialMalformed(
                "unsupported format or malformed credential".to_owned(),
            ));
        }
        Ok(Some(LocalCredential {
            secret: document.credential,
            record: document.record,
        }))
    }

    fn save(
        &self,
        credential: &str,
        record: Option<&NodeCredential>,
    ) -> Result<(), ClusterStartupError> {
        record_store_cluster::parse_node_credential(credential)?;
        let parent = self.path.parent().ok_or_else(|| {
            ClusterStartupError::CredentialMalformed("credential path has no parent".to_owned())
        })?;
        std::fs::create_dir_all(parent).map_err(ClusterStartupError::CredentialIo)?;
        let temporary = self.path.with_extension("json.tmp");
        let encoded = serde_json::to_vec_pretty(&CredentialDocument {
            format_version: CREDENTIAL_FORMAT_VERSION,
            credential: credential.to_owned(),
            record: record.cloned(),
        })
        .map_err(|error| ClusterStartupError::CredentialMalformed(error.to_string()))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(ClusterStartupError::CredentialIo)?;
        file.write_all(&encoded)
            .map_err(ClusterStartupError::CredentialIo)?;
        file.sync_all().map_err(ClusterStartupError::CredentialIo)?;
        drop(file);
        std::fs::rename(temporary, &self.path).map_err(ClusterStartupError::CredentialIo)?;
        Ok(())
    }
}

/// Cluster initialization failures surfaced as actionable startup errors.
#[derive(Debug, Error)]
pub enum ClusterStartupError {
    #[error(transparent)]
    Identity(#[from] record_store_cluster::IdentityError),
    #[error(transparent)]
    Credential(#[from] record_store_cluster::CredentialError),
    #[error(transparent)]
    Topology(#[from] record_store_cluster::TopologyError),
    #[error(transparent)]
    Catalog(#[from] record_store_cluster::ClusterCatalogError),
    #[error(transparent)]
    Consensus(#[from] record_store_consensus::ConsensusError),
    #[error(transparent)]
    StateMachine(#[from] record_store_consensus::StateMachineError),
    #[error(transparent)]
    Tls(#[from] record_store_rpc::TlsError),
    #[error(transparent)]
    Rpc(#[from] RpcServerError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("node credential file operation failed: {0}")]
    CredentialIo(#[source] std::io::Error),
    #[error("node credential file is malformed: {0}")]
    CredentialMalformed(String),
    #[error("cluster seeds are configured but cluster.join_token is missing")]
    JoinTokenRequired,
    #[error("node identity is missing its consensus member identifier")]
    MissingMemberId,
    #[error("node identity is missing its cluster identifier")]
    MissingClusterId,
    #[error("the bound node has no persisted node credential")]
    MissingNodeCredential,
    #[error("the bootstrap node credential record was not retained")]
    CredentialRecordMissing,
    #[error("node {0} is not registered in authoritative cluster metadata")]
    NodeNotRegistered(String),
    #[error("no configured seed accepted the cluster operation: {0}")]
    Seeds(String),
    #[error("the internal RPC supervisor stopped unexpectedly: {0}")]
    RpcTask(String),
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, io, net::SocketAddr};

    use bytes::Bytes;
    use futures_util::{TryStreamExt, stream};
    use record_store_core::{
        Bucket, BucketId, BucketName, BucketQuota, ObjectKey, OrganizationId, VersioningState,
    };
    use record_store_storage::{GetObjectRequest, PutObjectRequest, upload_stream};
    use tempfile::tempdir;
    use tokio::time::timeout;

    use super::*;

    fn reserve_rpc_address() -> SocketAddr {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("reserve an internal RPC address");
        listener.local_addr().expect("reserved RPC address")
    }

    fn node_config(data_directory: PathBuf, rpc: SocketAddr, rack: &str) -> Config {
        let mut config = Config::default();
        config.server.mode = DeploymentMode::Cluster;
        config.server.rpc_bind = rpc;
        config.server.rpc_advertise = Some(rpc.to_string());
        config.server.shutdown_grace_period_seconds = 2;
        config.cluster.replication_factor = 3;
        config.cluster.failure_domain = format!("rack={rack}");
        config.storage.data_directory = data_directory;
        config
    }

    fn supervise(
        process: ClusterProcess,
    ) -> (
        CancellationToken,
        JoinHandle<Result<(), ClusterStartupError>>,
    ) {
        let cancellation = CancellationToken::new();
        let shutdown = cancellation.clone().cancelled_owned();
        let task = tokio::spawn(process.supervise(shutdown));
        (cancellation, task)
    }

    #[tokio::test]
    async fn an_rf3_put_is_committed_on_three_joined_nodes_and_read_remotely() {
        let directory = tempdir().expect("temporary cluster directory");
        let first_rpc = reserve_rpc_address();
        let first_config = node_config(directory.path().join("first"), first_rpc, "rack-a");
        let first = initialize(&first_config)
            .await
            .expect("initialize the first cluster node");
        let first_operations = Arc::clone(&first.operations);
        let first_context = Arc::clone(&first.context);
        let first_metadata = Arc::clone(&first.metadata);
        let first_storage = Arc::clone(&first.storage);
        let first_process = supervise(first.process);

        // The test exercises replica durability, not the host running the test's
        // production disk-pressure policy. CI and developer machines may already
        // be above the default 90% high watermark or have less than the default
        // 1 GiB safety margin, which would make placement fail for an unrelated
        // environmental reason. Keep a real measured capacity while making the
        // tiny test payload's reservation deterministic.
        let mut cluster_config = first_context
            .config()
            .await
            .expect("read the initial cluster configuration");
        cluster_config.watermarks = record_store_cluster::CapacityWatermarks {
            low_percent: 98,
            high_percent: 99,
            critical_percent: 100,
        };
        cluster_config.capacity_safety_margin_bytes = 0;
        cluster_config.unknown_upload_size_reservation_bytes = 1;
        first_operations
            .set_config(cluster_config)
            .await
            .expect("configure deterministic test capacity policy");

        let second_token = first_operations
            .issue_join_token(300, "second RF3 test node".into())
            .await
            .expect("issue second-node join token");
        let third_token = first_operations
            .issue_join_token(300, "third RF3 test node".into())
            .await
            .expect("issue third-node join token");

        let second_rpc = reserve_rpc_address();
        let mut second_config = node_config(directory.path().join("second"), second_rpc, "rack-b");
        second_config.cluster.seeds = vec![first_rpc.to_string()];
        second_config.cluster.join_token = Some(record_store_config::SecretValue::new(
            second_token.token.expose(),
        ));
        let second = initialize(&second_config)
            .await
            .expect("join the second cluster node");
        let second_context = Arc::clone(&second.context);
        let second_storage = Arc::clone(&second.storage);
        let second_process = supervise(second.process);

        let third_rpc = reserve_rpc_address();
        let mut third_config = node_config(directory.path().join("third"), third_rpc, "rack-c");
        third_config.cluster.seeds = vec![first_rpc.to_string()];
        third_config.cluster.join_token = Some(record_store_config::SecretValue::new(
            third_token.token.expose(),
        ));
        let third = initialize(&third_config)
            .await
            .expect("join the third cluster node");
        let third_context = Arc::clone(&third.context);
        let third_process = supervise(third.process);

        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new("replicated-bucket").expect("valid bucket name"),
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
            durability_policy: None,
            cors: None,
        };
        first_metadata
            .create_bucket(&bucket)
            .await
            .expect("commit bucket metadata through consensus");

        const PAYLOAD: &[u8] = b"one streaming write, three independently verified replicas";
        let key = ObjectKey::new("distributed/object.txt").expect("valid object key");
        let put = first_storage
            .put(PutObjectRequest {
                bucket_id: bucket.id,
                key: key.clone(),
                content_type: Some("text/plain".into()),
                custom_metadata: BTreeMap::new(),
                expected_checksum: None,
                object_id: None,
                protocol_etag: None,
                body: upload_stream(stream::once(async {
                    Ok::<Bytes, io::Error>(Bytes::from_static(PAYLOAD))
                })),
            })
            .await
            .expect("RF3 PUT must satisfy its durability policy");

        let placement = first_context
            .placement_for(put.metadata.id)
            .await
            .expect("read committed placement")
            .expect("placement must exist after object commit");
        assert_eq!(placement.desired_replicas, 3);
        assert_eq!(placement.replicas.len(), 3);
        assert!(
            placement
                .replicas
                .iter()
                .all(|replica| replica.state == record_store_cluster::ReplicaState::Healthy)
        );

        for context in [&first_context, &second_context, &third_context] {
            assert!(
                context
                    .local
                    .stat_replica(put.metadata.id)
                    .await
                    .expect("inspect local replica")
                    .is_some(),
                "every selected node must contain durable replica bytes"
            );
        }

        let read = second_storage
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key,
                range: None,
            })
            .await
            .expect("read the committed object through another ingress node");
        let chunks = read
            .body
            .try_collect::<Vec<_>>()
            .await
            .expect("stream verified object bytes");
        assert_eq!(chunks.concat(), PAYLOAD);

        for (cancellation, process) in [first_process, second_process, third_process] {
            cancellation.cancel();
            timeout(Duration::from_secs(5), process)
                .await
                .expect("cluster process shutdown stayed bounded")
                .expect("cluster supervisor task")
                .expect("cluster process shut down cleanly");
        }
    }
}
