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
    CapacityAwarePlacement, ClusterCommand, ClusterIdentity, DeviceCapacity, DeviceHealth,
    DeviceKind, DeviceRecord, DeviceState, FailureDomain, HardwareMetadata, NodeCapacity,
    NodeCredential, NodeIdentity, NodeIdentityStore, NodeRegistration, NodeState, NodeVersions,
    PlacementWeight, StorageClass,
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
use record_store_storage::{
    DeviceStore, LocalFilesystemStore, ObjectStore, ReplicaStore, StorageError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

/// Restricts journal draining to the member that currently leads.
///
/// The storage-event journal is replicated, so every member can see every
/// pending event. The delivery outbox is not: it is node-local. Draining from
/// more than one member at a time would therefore put each event into several
/// outboxes and deliver it several times.
///
/// Leadership is the gate because it is already the cluster's single
/// serialization point and it moves on its own when a member fails. What it
/// does *not* give is exactly-once delivery across a handover: the member
/// taking over resumes from its own outbox position, which may be behind the
/// one the previous leader had reached, so events near the handover can be
/// delivered twice. Subscribers deduplicate on the event identifier, which is
/// allocated when the mutation committed and is the same on every member.
pub struct LeaderEventPumpGate {
    consensus: Arc<MetadataConsensus>,
}

impl LeaderEventPumpGate {
    /// Creates a gate over one member's consensus handle.
    #[must_use]
    pub const fn new(consensus: Arc<MetadataConsensus>) -> Self {
        Self { consensus }
    }
}

#[async_trait::async_trait]
impl record_store_service::EventPumpGate for LeaderEventPumpGate {
    async fn active(&self) -> bool {
        // A read barrier succeeds only where leadership is confirmed by a
        // quorum. A follower, a member in a minority partition, and a member
        // that merely believes it leads all fail it, which is the answer this
        // gate needs.
        self.consensus.read_barrier_index().await.is_ok()
    }
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
    let consensus_directory = data_directory.join("metadata").join("consensus");
    // Checked before anything is created or bound. `load_or_create` would mint a
    // replacement identity for a node that already owns durable cluster state,
    // and that replacement is itself a durable change — made, as it happens, on
    // the way to refusing to start. The order matters more than the check.
    if !identity_store.exists()
        && record_store_consensus::holds_consensus_state(&consensus_directory)
    {
        let cluster = record_store_consensus::recovery::inspect(&consensus_directory)
            .await
            .ok()
            .and_then(|assessment| assessment.cluster)
            .map(|identity| identity.cluster_id)
            .unwrap_or_default();
        return Err(ClusterStartupError::IdentityLost { cluster });
    }
    let mut identity = identity_store.load_or_create(Utc::now())?;
    let versions = NodeVersions::current(env!("CARGO_PKG_VERSION"));
    let tls = tls_settings(config);
    tls.validate()?;
    let credential_store = LocalCredentialStore::new(data_directory);
    let mut credential = credential_store.load()?;
    let advertise_address = config.server.effective_rpc_advertise();
    let profile = node_profile(identity.node_id, config, measure_capacity(data_directory));
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
    let mut consensus_settings =
        ConsensusSettings::new(member_id, &advertise_address, consensus_directory.clone());
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
    // Checked before anything is written, and on every start rather than only
    // on the bootstrap path: a mismatch between a node's identity and its
    // replicated state is just as wrong when seeds are configured.
    refuse_to_invent_authority(config, &identity, &consensus).await?;
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
    let local = Arc::new(open_local_store(config, local_metadata.clone()).await?);
    let local_replica: Arc<dyn ReplicaStore> = local.clone();
    // The data directory is always a device. Anything declared under
    // `storage.devices` is an additional one, opened the same way, so the data
    // plane can route to each independently.
    let mut stores: Vec<(record_store_core::DeviceId, Arc<dyn ReplicaStore>)> = vec![(
        DeviceRecord::legacy_id(identity.node_id),
        Arc::clone(&local_replica),
    )];
    for device in &config.storage.devices {
        let store = open_device_store(config, device, local_metadata.clone()).await?;
        stores.push((
            declared_device_id(identity.node_id, &device.name),
            Arc::new(store) as Arc<dyn ReplicaStore>,
        ));
    }
    let local_devices = Arc::new(DeviceStore::new(
        DeviceRecord::legacy_id(identity.node_id),
        stores,
    )?);
    let transport = Arc::new(RpcReplicaTransport::new(Arc::clone(&pool)));
    let context = Arc::new(ClusterContext {
        node_id: identity.node_id,
        cluster: Arc::clone(&cluster),
        metadata: Arc::clone(&metadata),
        local: Arc::clone(&local_devices),
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
            Arc::clone(&local_devices),
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

/// Counts stored payloads, cheaply and only far enough to answer "any?".
///
/// The startup guard needs one fact: does this node hold data that a second,
/// independent cluster would inherit? A full scan would be wasted — the answer
/// stops mattering after the first few — so the walk is bounded.
fn stored_payload_sample(data_directory: &Path, limit: usize) -> usize {
    fn walk(directory: &Path, depth: usize, limit: usize, found: &mut usize) {
        if *found >= limit || depth > 3 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            if *found >= limit {
                return;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(&entry.path(), depth + 1, limit, found),
                Ok(kind) if kind.is_file() => *found += 1,
                _ => {}
            }
        }
    }
    let mut found = 0;
    walk(&data_directory.join("objects"), 0, limit, &mut found);
    found
}

/// Refuses to start when starting would invent authority rather than resume it.
///
/// Three situations are refused, and they are different mistakes:
///
/// * a node that belongs to a cluster, still holds its data, and has lost its
///   consensus state — starting it alone would form a second cluster around that
///   data, and two clusters holding one identity can never be reconciled;
/// * a node whose identity file and replicated state name different clusters —
///   one of the two was replaced, and serving either under the other's name is
///   worse than not starting;
/// * durable cluster state with no identity to own it — the identity file was
///   lost, and a fresh one would silently adopt another node's data.
///
/// A node with seeds configured is exempt from the first: it has somewhere to
/// learn the truth from, so rejoining is a recovery rather than an invention.
async fn refuse_to_invent_authority(
    config: &Config,
    identity: &NodeIdentity,
    consensus: &Arc<MetadataConsensus>,
) -> Result<(), ClusterStartupError> {
    let recorded = consensus.state().cluster().identity().await?;
    match (identity.cluster_id, recorded.as_ref()) {
        (Some(bound), Some(state)) if bound != state.cluster_id => {
            return Err(ClusterStartupError::ClusterIdentityMismatch {
                identity: bound,
                state: state.cluster_id,
            });
        }
        (None, Some(state)) => {
            return Err(ClusterStartupError::IdentityLost {
                cluster: state.cluster_id,
            });
        }
        _ => {}
    }

    // The dangerous case: bound to a cluster, no metadata state left, nobody to
    // ask, and data on disk to take hostage.
    if let Some(cluster) = identity.cluster_id
        && recorded.is_none()
        && config.cluster.seeds.is_empty()
    {
        let payloads = stored_payload_sample(&config.storage.data_directory, 1);
        if payloads > 0 {
            return Err(ClusterStartupError::WouldFormSecondCluster { cluster, payloads });
        }
    }
    Ok(())
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
                    recovery_generation: 0,
                    recovery_id: None,
                    recovered_at: None,
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
                management_endpoint: config.cluster.management_endpoint.clone(),
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
        management_endpoint: None,
        storage_class: StorageClass::new(&config.cluster.storage_class)?,
        failure_domain: FailureDomain::new(profile.failure_domain.clone().into_iter().collect())?,
        capacity: NodeCapacity {
            total_bytes: profile.total_bytes,
            available_bytes: profile.available_bytes,
            replica_bytes: profile.replica_bytes,
            temporary_bytes: profile.temporary_bytes,
        },
        devices: serde_json::from_str(&profile.devices_json).unwrap_or_default(),
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

fn node_profile(
    node_id: record_store_core::NodeId,
    config: &Config,
    capacity: NodeCapacity,
) -> NodeProfile {
    let failure_domain = FailureDomain::parse(&config.cluster.failure_domain)
        .map(|domain| domain.labels().clone())
        .unwrap_or_default();
    let node_class = StorageClass::new(&config.cluster.storage_class).unwrap_or_default();
    let mut devices = vec![DeviceRecord::legacy_directory(
        node_id,
        Some(config.storage.data_directory.clone()),
        node_class.clone(),
        DeviceCapacity {
            raw_bytes: capacity.total_bytes,
            usable_bytes: capacity.total_bytes,
            allocated_bytes: capacity.replica_bytes,
            reserved_bytes: 0,
            available_bytes: capacity.available_bytes,
        },
    )];
    for declared in &config.storage.devices {
        // Capacity is measured per device, because that is the whole point of
        // declaring them separately: two mounts have two different amounts of
        // room, and placement weights depend on knowing which is which.
        let measured = measure_capacity(&declared.path);
        let class = declared
            .storage_class
            .as_deref()
            .and_then(|value| StorageClass::new(value).ok())
            .unwrap_or_else(|| node_class.clone());
        devices.push(DeviceRecord {
            id: declared_device_id(node_id, &declared.name),
            node_id,
            current_path: Some(declared.path.clone()),
            stable_hardware_identifier: None,
            kind: DeviceKind::FilesystemDirectory,
            storage_class: class,
            capacity: DeviceCapacity {
                raw_bytes: measured.total_bytes,
                usable_bytes: measured.total_bytes,
                allocated_bytes: measured.replica_bytes,
                reserved_bytes: 0,
                available_bytes: measured.available_bytes,
            },
            configured_weight: declared
                .weight
                .and_then(|weight| PlacementWeight::new(weight).ok())
                .unwrap_or_default(),
            // Nothing has observed this device's hardware, and saying `Healthy`
            // would be inventing a reading nobody took.
            health: DeviceHealth::Unknown,
            state: DeviceState::Active,
            hardware: HardwareMetadata::default(),
            movement_concurrency: declared.movement_concurrency,
        });
    }
    NodeProfile {
        storage_class: config.cluster.storage_class.clone(),
        failure_domain: failure_domain.into_iter().collect(),
        total_bytes: capacity.total_bytes,
        available_bytes: capacity.available_bytes,
        replica_bytes: capacity.replica_bytes,
        temporary_bytes: capacity.temporary_bytes,
        started_at: Utc::now().to_rfc3339(),
        s3_endpoint: config.cluster.s3_endpoint.clone().unwrap_or_default(),
        devices_json: serde_json::to_string(&devices).unwrap_or_else(|_| "[]".to_owned()),
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

/// Derives a stable device identity from the node and the declared name.
///
/// Deriving rather than storing means a node restarting with the same
/// configuration keeps the same devices, so a restart never orphans the replicas
/// already placed on them. Renaming a device in configuration therefore declares
/// a different device, which is why the name is documented as identity.
fn declared_device_id(
    node_id: record_store_core::NodeId,
    name: &str,
) -> record_store_core::DeviceId {
    let mut hasher = Sha256::new();
    hasher.update(b"record-store.device.v1");
    hasher.update(node_id.as_uuid().as_bytes());
    hasher.update(name.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    record_store_core::DeviceId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

/// Opens one declared device's replica store.
///
/// Encryption follows the node: a deployment does not get to encrypt one drive
/// and leave another in the clear without saying so.
async fn open_device_store(
    config: &Config,
    device: &record_store_config::StorageDeviceConfig,
    metadata: Arc<dyn MetadataRepository>,
) -> Result<LocalFilesystemStore, StorageError> {
    let temporary = record_store_config::StorageConfig::device_temporary_directory(device);
    if config.storage.encryption_enabled {
        let master_key = config
            .auth
            .credential_master_key
            .as_ref()
            .ok_or(StorageError::EncryptionKeyRequired)?;
        LocalFilesystemStore::open_encrypted(
            &device.path,
            temporary,
            metadata,
            master_key.expose().as_bytes(),
        )
        .await
    } else {
        LocalFilesystemStore::open(&device.path, temporary, metadata).await
    }
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
    Domain(#[from] record_store_core::CoreError),
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
    /// Starting would have formed a second cluster from a survivor's data.
    #[error(
        "this node already belongs to cluster {cluster}, holds {payloads} stored payload(s), and \
         has no metadata consensus state left. Starting it would form a *second* cluster around \
         that data, which can never be reconciled with the original. Either configure \
         `cluster.seeds` so it rejoins the existing cluster, or, if the original cluster's quorum \
         is genuinely unrecoverable, run the recovery procedure against a surviving member."
    )]
    WouldFormSecondCluster {
        /// Cluster this node's durable identity is bound to.
        cluster: record_store_core::ClusterId,
        /// Payloads found on this node's disk.
        payloads: usize,
    },
    /// The node's identity and its replicated state disagree about which cluster it is in.
    #[error(
        "this node's durable identity says it belongs to cluster {identity}, but its metadata \
         state belongs to cluster {state}. One of the two was replaced. Refusing to start rather \
         than serving one cluster's data under another's name."
    )]
    ClusterIdentityMismatch {
        /// Cluster the identity file names.
        identity: record_store_core::ClusterId,
        /// Cluster the replicated state names.
        state: record_store_core::ClusterId,
    },
    /// Durable cluster state exists but the node identity that owned it is gone.
    #[error(
        "this node holds metadata for cluster {cluster} but has no durable node identity; the \
         identity file was lost or replaced. Restore it, or treat this node as permanently lost \
         and admit a clean replacement."
    )]
    IdentityLost {
        /// Cluster the replicated state names.
        cluster: record_store_core::ClusterId,
    },
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
        WriteOrigin,
    };
    use record_store_storage::{GetObjectRequest, PutObjectRequest, upload_stream};
    use tempfile::tempdir;
    use tokio::time::timeout;

    use super::*;

    /// Device identity has to survive a restart.
    ///
    /// It is derived from the node and the configured name rather than stored,
    /// so a node restarting with the same configuration keeps the same devices.
    /// If it did not, every replica already placed on a declared drive would be
    /// orphaned by a reboot.
    #[test]
    fn a_declared_device_keeps_its_identity_across_restarts() {
        let node = record_store_core::NodeId::new();
        assert_eq!(
            declared_device_id(node, "nvme0"),
            declared_device_id(node, "nvme0")
        );

        // A different name is a different device, which is why the name is
        // documented as identity rather than as a label.
        assert_ne!(
            declared_device_id(node, "nvme0"),
            declared_device_id(node, "nvme1")
        );

        // And the same name on another node is another device.
        assert_ne!(
            declared_device_id(node, "nvme0"),
            declared_device_id(record_store_core::NodeId::new(), "nvme0")
        );

        // Never collides with the node's own data directory.
        assert_ne!(
            declared_device_id(node, "nvme0"),
            DeviceRecord::legacy_id(node)
        );
    }

    /// A node advertises every device it serves, so the cluster map can place
    /// across them. Advertising only the data directory is what limited the
    /// whole multi-device architecture to one drive per node.
    #[test]
    fn a_node_advertises_its_declared_devices() {
        let node = record_store_core::NodeId::new();
        let mut config = Config::default();
        config.cluster.storage_class = "standard".into();
        config.storage.devices = vec![
            record_store_config::StorageDeviceConfig {
                name: "nvme0".into(),
                path: std::path::PathBuf::from("/mnt/nvme0"),
                storage_class: Some("hot".into()),
                weight: Some(2_000),
                movement_concurrency: None,
            },
            record_store_config::StorageDeviceConfig {
                name: "hdd0".into(),
                path: std::path::PathBuf::from("/mnt/hdd0"),
                storage_class: None,
                weight: None,
                movement_concurrency: None,
            },
        ];

        let profile = node_profile(node, &config, NodeCapacity::default());
        let devices: Vec<DeviceRecord> =
            serde_json::from_str(&profile.devices_json).expect("devices");

        assert_eq!(
            devices.len(),
            3,
            "the data directory plus two declared drives"
        );
        assert_eq!(devices[0].id, DeviceRecord::legacy_id(node));

        let nvme = &devices[1];
        assert_eq!(nvme.id, declared_device_id(node, "nvme0"));
        assert_eq!(nvme.storage_class.as_str(), "hot");
        assert_eq!(nvme.configured_weight.get(), 2_000);
        assert_eq!(
            nvme.current_path.as_deref(),
            Some(std::path::Path::new("/mnt/nvme0"))
        );
        // Nothing has inspected this hardware, and inventing a reading would be
        // worse than admitting there is none.
        assert_eq!(nvme.health, DeviceHealth::Unknown);

        let hdd = &devices[2];
        assert_eq!(
            hdd.storage_class.as_str(),
            "standard",
            "a device that names no class inherits the node's"
        );
        assert_eq!(hdd.configured_weight, PlacementWeight::default());
    }

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

    /// Starts a node, then puts it down completely, returning its cluster.
    ///
    /// redb keeps a data directory exclusively locked for as long as any handle
    /// to it is alive, so a test that restarts a node — or inspects its files
    /// offline — has to release every handle, not just stop the supervisor.
    async fn start_then_stop(config: &Config) -> record_store_core::ClusterId {
        let node = initialize(config).await.expect("start the cluster node");
        let cluster_id = node
            .context
            .cluster
            .identity()
            .await
            .expect("read identity")
            .expect("initialized")
            .cluster_id;
        let ClusterDependencies {
            storage,
            metadata,
            context,
            consensus,
            operations,
            task_health,
            process,
        } = node;
        let (cancellation, task) = supervise(process);
        cancellation.cancel();
        let _ = task.await;
        drop(storage);
        drop(metadata);
        drop(context);
        drop(operations);
        drop(task_health);
        drop(consensus);
        cluster_id
    }

    /// The disaster that has to be refused: a node that belonged to a cluster,
    /// still holds its data, and has lost the metadata state that said what that
    /// data meant.
    ///
    /// Starting it alone would form a *second* cluster around the surviving
    /// payloads. Both would carry the same identifier and neither could ever be
    /// reconciled with the other, so the failure has to happen at startup — not
    /// after the node has served a read from data it has no authority over.
    #[tokio::test]
    async fn a_survivor_whose_metadata_state_is_gone_refuses_to_form_a_second_cluster() {
        let directory = tempdir().expect("temporary cluster directory");
        let data = directory.path().join("survivor");
        let config = node_config(data.clone(), reserve_rpc_address(), "rack-a");

        let cluster_id = start_then_stop(&config).await;

        // The node's payloads survive; its consensus state does not. This is a
        // lost disk, a botched restore, or a cleanup script.
        std::fs::create_dir_all(data.join("objects").join("ab").join("cd")).expect("payload shard");
        std::fs::write(
            data.join("objects").join("ab").join("cd").join("payload"),
            b"surviving bytes",
        )
        .expect("a payload that outlived the metadata");
        std::fs::remove_dir_all(data.join("metadata").join("consensus"))
            .expect("lose the consensus state");

        let refused = initialize(&config).await.err();
        let Some(ClusterStartupError::WouldFormSecondCluster { cluster, payloads }) = refused
        else {
            panic!("a survivor with data and no metadata must refuse to start: {refused:?}");
        };
        assert_eq!(cluster, cluster_id, "the refusal names the cluster at risk");
        assert!(payloads > 0, "and says why it is refusing");
    }

    /// The same node, given somewhere to learn the truth from, is not inventing
    /// authority — it is rejoining. Refusing that would turn an ordinary
    /// recovery into an outage, so the guard is about being *alone*, not about
    /// having lost state.
    #[tokio::test]
    async fn a_survivor_with_seeds_configured_is_allowed_to_rejoin_instead() {
        let directory = tempdir().expect("temporary cluster directory");
        let data = directory.path().join("survivor");
        let mut config = node_config(data.clone(), reserve_rpc_address(), "rack-a");

        start_then_stop(&config).await;

        std::fs::create_dir_all(data.join("objects").join("ab").join("cd")).expect("payload shard");
        std::fs::write(
            data.join("objects").join("ab").join("cd").join("payload"),
            b"surviving bytes",
        )
        .expect("payload");
        std::fs::remove_dir_all(data.join("metadata").join("consensus"))
            .expect("lose the consensus state");

        // A seed it can ask. Startup will fail because nothing is listening
        // there, but it must fail for *that* reason rather than by refusing to
        // form a second cluster.
        config.cluster.seeds = vec![reserve_rpc_address().to_string()];
        let outcome = initialize(&config).await.err();
        assert!(
            !matches!(
                outcome,
                Some(ClusterStartupError::WouldFormSecondCluster { .. })
            ),
            "a node with a seed is rejoining, not inventing authority: {outcome:?}"
        );
    }

    /// An identity file and a replicated state that name different clusters mean
    /// one of the two was replaced. Serving either under the other's name is
    /// worse than not starting, so neither is chosen.
    #[tokio::test]
    async fn a_node_whose_identity_and_state_disagree_refuses_to_pick_one() {
        let directory = tempdir().expect("temporary cluster directory");
        let data = directory.path().join("confused");
        let config = node_config(data.clone(), reserve_rpc_address(), "rack-a");

        start_then_stop(&config).await;

        // Somebody restored the wrong identity file next to this data.
        let identity_path = data.join("node-identity.json");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&identity_path).expect("read identity"))
                .expect("identity is JSON");
        document["cluster_id"] =
            serde_json::Value::String(record_store_core::ClusterId::new().to_string());
        std::fs::write(
            &identity_path,
            serde_json::to_vec(&document).expect("encode identity"),
        )
        .expect("write a foreign identity");

        let refused = initialize(&config).await.err();
        assert!(
            matches!(
                refused,
                Some(ClusterStartupError::ClusterIdentityMismatch { .. })
            ),
            "a node must not serve one cluster's data under another's name: {refused:?}"
        );
    }

    /// Durable cluster state with no identity to own it means the identity file
    /// was lost. A fresh one would silently adopt this data under a new node
    /// identity, so the node stops and says what is missing.
    #[tokio::test]
    async fn a_node_that_lost_its_identity_file_refuses_to_adopt_its_own_data() {
        let directory = tempdir().expect("temporary cluster directory");
        let data = directory.path().join("orphaned");
        let config = node_config(data.clone(), reserve_rpc_address(), "rack-a");

        start_then_stop(&config).await;

        std::fs::remove_file(data.join("node-identity.json")).expect("lose the identity file");

        let refused = initialize(&config).await.err();
        assert!(
            matches!(refused, Some(ClusterStartupError::IdentityLost { .. })),
            "a node with state but no identity must not invent one: {refused:?}"
        );
    }

    /// Ordinary restart, which everything above must not have broken: the same
    /// node, unchanged, starts again and still holds its cluster.
    #[tokio::test]
    async fn an_ordinary_restart_resumes_the_same_cluster() {
        let directory = tempdir().expect("temporary cluster directory");
        let data = directory.path().join("restarting");
        let config = node_config(data, reserve_rpc_address(), "rack-a");

        let cluster_id = start_then_stop(&config).await;

        let second = initialize(&config).await.expect("restart the same node");
        let restarted = second
            .context
            .cluster
            .identity()
            .await
            .expect("read identity")
            .expect("initialized");
        assert_eq!(
            restarted.cluster_id, cluster_id,
            "a restart resumes the cluster rather than forming one"
        );
        assert_eq!(
            restarted.recovery_generation, 0,
            "an ordinary restart is not a recovery and must not look like one"
        );
        let (cancellation, task) = supervise(second.process);
        cancellation.cancel();
        let _ = task.await;
    }

    /// Loosens the capacity policy so a tiny test payload can be placed.
    ///
    /// CI and developer machines are often already above the default high
    /// watermark, which would make placement fail for an environmental reason
    /// that has nothing to do with what is being tested.
    async fn relax_capacity_policy(operations: &ClusterOperations, context: &ClusterContext) {
        let mut cluster_config = context
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
        operations
            .set_config(cluster_config)
            .await
            .expect("configure deterministic test capacity policy");
    }

    /// The whole of disaster recovery, end to end, against real nodes.
    ///
    /// A three-node cluster stores an object with three replicas. Two nodes are
    /// then lost permanently, which takes the metadata quorum with them. The
    /// survivor is recovered offline, restarted, and has to come back as a
    /// coherent cluster: its own identity, its object history, and — the part
    /// that actually matters — the object's bytes, verified against what was
    /// written rather than merely a successful status code.
    #[tokio::test]
    async fn a_cluster_recovered_from_one_survivor_still_serves_its_verified_objects() {
        let directory = tempdir().expect("temporary cluster directory");
        let first_rpc = reserve_rpc_address();
        let first_config = node_config(directory.path().join("first"), first_rpc, "rack-a");
        let first = initialize(&first_config)
            .await
            .expect("initialize the first cluster node");
        relax_capacity_policy(&first.operations, &first.context).await;

        let second_token = first
            .operations
            .issue_join_token(300, "second node".into())
            .await
            .expect("issue a join token");
        let third_token = first
            .operations
            .issue_join_token(300, "third node".into())
            .await
            .expect("issue a join token");

        let mut second_config = node_config(
            directory.path().join("second"),
            reserve_rpc_address(),
            "rack-b",
        );
        second_config.cluster.seeds = vec![first_rpc.to_string()];
        second_config.cluster.join_token = Some(record_store_config::SecretValue::new(
            second_token.token.expose(),
        ));
        let second = initialize(&second_config)
            .await
            .expect("join the second node");

        let mut third_config = node_config(
            directory.path().join("third"),
            reserve_rpc_address(),
            "rack-c",
        );
        third_config.cluster.seeds = vec![first_rpc.to_string()];
        third_config.cluster.join_token = Some(record_store_config::SecretValue::new(
            third_token.token.expose(),
        ));
        let third = initialize(&third_config)
            .await
            .expect("join the third node");

        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new("recovered-bucket").expect("valid bucket name"),
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
            storage_class: None,
            durability_policy: None,
            object_lock: None,
            cors: None,
        };
        first
            .metadata
            .create_bucket(&bucket)
            .await
            .expect("commit bucket metadata");

        const PAYLOAD: &[u8] = b"written before the quorum was lost, and readable after";
        let key = ObjectKey::new("recovered/object.txt").expect("valid object key");
        let put = first
            .storage
            .put(PutObjectRequest {
                bucket_id: bucket.id,
                key: key.clone(),
                content_type: Some("text/plain".into()),
                custom_metadata: BTreeMap::new(),
                expected_checksum: None,
                object_id: None,
                protocol_etag: None,
                object_lock: None,
                origin: WriteOrigin::Direct,
                body: upload_stream(stream::once(async {
                    Ok::<Bytes, io::Error>(Bytes::from_static(PAYLOAD))
                })),
            })
            .await
            .expect("the write must satisfy its durability policy");
        let committed_checksum = put.metadata.checksum.clone();
        let cluster_id = first
            .context
            .cluster
            .identity()
            .await
            .expect("read identity")
            .expect("initialized")
            .cluster_id;
        let survivor_member = first.consensus.member_id();

        // Every node stops, and every handle with it: recovery runs offline.
        for node in [first, second, third] {
            let ClusterDependencies {
                storage,
                metadata,
                context,
                consensus,
                operations,
                task_health,
                process,
            } = node;
            let (cancellation, task) = supervise(process);
            cancellation.cancel();
            let _ = task.await;
            drop(storage);
            drop(metadata);
            drop(context);
            drop(operations);
            drop(task_health);
            drop(consensus);
        }
        // Two of three are gone for good.
        std::fs::remove_dir_all(directory.path().join("second")).expect("lose the second node");
        std::fs::remove_dir_all(directory.path().join("third")).expect("lose the third node");

        // Without recovery, the survivor alone cannot elect: one of three voters
        // is not a majority. That is the state an operator is recovering from,
        // and it is correct rather than broken.
        let consensus_directory = first_config
            .storage
            .data_directory
            .join("metadata")
            .join("consensus");
        let assessment = record_store_consensus::recovery::inspect(&consensus_directory)
            .await
            .expect("inspect the survivor");
        assert!(assessment.recoverable, "{assessment:?}");
        assert_eq!(assessment.voters.len(), 3, "{assessment:?}");

        let report = record_store_consensus::recovery::recover_single_member(
            &consensus_directory,
            record_store_consensus::RecoveryIntent {
                cluster_id,
                member_id: survivor_member,
                address: first_rpc.to_string(),
                reason: "two of three voters lost permanently".to_owned(),
                accept_data_loss: true,
            },
        )
        .await
        .expect("rebuild authority around the survivor");
        assert_eq!(report.recovery_generation, 1);
        assert_eq!(
            report.payloads_held_here, report.payloads_total,
            "this survivor held a replica of everything, so nothing should be reported as \
             needing another holder: {report:?}"
        );

        // The recovered node comes back as a working cluster.
        let recovered = initialize(&first_config)
            .await
            .expect("the recovered member must start");
        let identity = recovered
            .context
            .cluster
            .identity()
            .await
            .expect("read identity")
            .expect("initialized");
        assert_eq!(
            identity.cluster_id, cluster_id,
            "recovery keeps the cluster's identity rather than creating a new one"
        );
        assert_eq!(identity.recovery_generation, 1);

        // The object is still there — and its bytes are the bytes that were
        // written, checked rather than assumed.
        let read = recovered
            .storage
            .get(GetObjectRequest {
                bucket_id: bucket.id,
                key: key.clone(),
                range: None,
            })
            .await
            .expect("the recovered cluster must still serve the object");
        assert_eq!(read.metadata.checksum, committed_checksum);
        use futures_util::StreamExt as _;
        let mut body = read.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk.expect("read a payload chunk"));
        }
        assert_eq!(
            bytes.as_slice(),
            PAYLOAD,
            "a recovered cluster must return the bytes that were written, not merely a 200"
        );

        // Writes are a different question from reads, and the answer is not the
        // convenient one. A single member cannot satisfy a policy that requires
        // two acknowledgements, and the cluster refuses rather than quietly
        // acknowledging at one. That refusal is the guarantee working: an
        // operator who wants writes back must restore capacity or deliberately
        // lower the policy, not have it lowered for them.
        assert!(
            !report.writable_alone(),
            "this cluster's policy needs more than one acknowledgement: {report:?}"
        );
        let refused = recovered
            .storage
            .put(PutObjectRequest {
                bucket_id: bucket.id,
                key: ObjectKey::new("recovered/after.txt").expect("valid object key"),
                content_type: None,
                custom_metadata: BTreeMap::new(),
                expected_checksum: None,
                object_id: None,
                protocol_etag: None,
                object_lock: None,
                origin: WriteOrigin::Direct,
                body: upload_stream(stream::once(async {
                    Ok::<Bytes, io::Error>(Bytes::from_static(b"after recovery"))
                })),
            })
            .await;
        let Err(record_store_storage::StorageError::DurabilityNotMet {
            required, achieved, ..
        }) = refused
        else {
            panic!("a lone survivor must not acknowledge a write below its policy: {refused:?}");
        };
        assert!(
            required > achieved,
            "required {required}, achieved {achieved}"
        );

        // Recovery rebuilt metadata *authority*, not the data plane's view of who
        // exists: the lost nodes are still recorded as members, and placement
        // will keep choosing them until they are retired. Retiring them is an
        // operator step, and a forced one, because their replicas are genuinely
        // gone rather than movable.
        for node_id in &report.other_nodes {
            recovered
                .operations
                .decommission(*node_id, true)
                .await
                .expect("retire a node that is never coming back");
        }

        // Lowering the policy is a separate, explicit operator decision. Only
        // with both done does the recovered cluster serve writes again.
        let mut single_member_policy = recovered
            .context
            .config()
            .await
            .expect("read the cluster configuration");
        single_member_policy.replication_factor = 1;
        single_member_policy.write_acknowledgement =
            record_store_cluster::WriteAcknowledgement::Count(1);
        recovered
            .operations
            .set_config(single_member_policy)
            .await
            .expect("accept an explicitly lowered durability policy");
        recovered
            .storage
            .put(PutObjectRequest {
                bucket_id: bucket.id,
                key: ObjectKey::new("recovered/after.txt").expect("valid object key"),
                content_type: None,
                custom_metadata: BTreeMap::new(),
                expected_checksum: None,
                object_id: None,
                protocol_etag: None,
                object_lock: None,
                origin: WriteOrigin::Direct,
                body: upload_stream(stream::once(async {
                    Ok::<Bytes, io::Error>(Bytes::from_static(b"after recovery"))
                })),
            })
            .await
            .expect("the recovered cluster serves writes once its policy is satisfiable");

        let (cancellation, task) = supervise(recovered.process);
        cancellation.cancel();
        let _ = task.await;
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
            storage_class: None,
            durability_policy: None,
            object_lock: None,
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
                object_lock: None,
                origin: WriteOrigin::Direct,
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
