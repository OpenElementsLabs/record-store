//! Explicit Record Store server initialization and dual-listener lifecycle orchestration.

pub mod backup;
mod cluster;
pub mod discovery;
pub mod preflight;

use std::{
    fs::{File, OpenOptions},
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::Utc;
use record_store_api::{
    AppState, ClusterManagement, ManagementAuth, MetricsAuth, SharingManagement,
};
use record_store_audit::{AuditError, AuditRepository, RedbAuditRepository};
use record_store_auth::{
    Authorizer, CredentialManager, CredentialStoreError, SigningCredentialProvider,
};
use record_store_config::{Config, ConfigError};
use record_store_core::OrganizationId;
use record_store_events::{
    EventError, EventRepository, RedbEventRepository, WebhookConfig, WebhookWorker,
};
use record_store_lifecycle::{LifecycleError, LifecycleWorker};
use record_store_metadata::{MetadataError, MetadataRepository, RedbMetadataRepository};
use record_store_service::{ObjectLockLimits, ServiceLimits, Services, StorageEventPump};
use record_store_sharing::{CapabilityStore, SharingPolicy, SharingService, TicketIssuer};
use record_store_storage::{LocalFilesystemStore, ObjectStore, StorageError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

/// Initialized dependencies ready for S3 and management listeners.
pub struct ServerRuntime {
    management: axum::Router,
    s3: axum::Router,
    shutdown_grace_period: Duration,
    header_read_timeout: Duration,
    webhook_worker: WebhookWorker,
    event_pump: StorageEventPump,
    lifecycle_worker: LifecycleWorker,
    process_lock: File,
    cleanup_storage: Arc<dyn ObjectStore>,
    clock_services: Services,
    clock_watermark_interval: Duration,
    metrics_history: Arc<record_store_api::history::MetricsHistory>,
    metrics_services: Services,
    cluster_process: Option<cluster::ClusterProcess>,
}

impl ServerRuntime {
    /// Serves both listeners and applies the same shutdown signal to each.
    pub async fn serve<F>(
        self,
        s3_listener: TcpListener,
        api_listener: TcpListener,
        shutdown: F,
    ) -> Result<(), StartupError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let _process_lock = self.process_lock;
        let cancellation = CancellationToken::new();
        let signal_token = cancellation.clone();
        tokio::spawn(async move {
            shutdown.await;
            signal_token.cancel();
        });
        let s3_shutdown = cancellation.clone().cancelled_owned();
        let api_shutdown = cancellation.clone().cancelled_owned();
        let webhook_shutdown = cancellation.clone();
        let event_pump_shutdown = cancellation.clone();
        let lifecycle_shutdown = cancellation;
        let cleanup_shutdown = lifecycle_shutdown.clone();
        let cluster_shutdown = lifecycle_shutdown.clone();
        let clock_shutdown = lifecycle_shutdown.clone();
        let metrics_shutdown = lifecycle_shutdown.clone();
        tokio::try_join!(
            async {
                record_store_api::serve(
                    s3_listener,
                    self.s3,
                    s3_shutdown,
                    self.shutdown_grace_period,
                    self.header_read_timeout,
                )
                .await
                .map_err(StartupError::Http)
            },
            async {
                record_store_api::serve(
                    api_listener,
                    self.management,
                    api_shutdown,
                    self.shutdown_grace_period,
                    self.header_read_timeout,
                )
                .await
                .map_err(StartupError::Http)
            },
            async {
                self.webhook_worker
                    .run(webhook_shutdown)
                    .await
                    .map_err(StartupError::Events)
            },
            async {
                // Drains what committed mutations already made durable, so a
                // restart resumes delivery rather than starting from whatever
                // happens next.
                self.event_pump.run(event_pump_shutdown).await;
                Ok::<(), StartupError>(())
            },
            async {
                self.lifecycle_worker
                    .run(lifecycle_shutdown)
                    .await
                    .map_err(StartupError::Lifecycle)
            },
            async {
                run_payload_cleanup(self.cleanup_storage, cleanup_shutdown).await;
                Ok::<(), StartupError>(())
            },
            async {
                run_clock_watermark(
                    self.clock_services,
                    self.clock_watermark_interval,
                    clock_shutdown,
                )
                .await;
                Ok::<(), StartupError>(())
            },
            async {
                run_metrics_sampler(
                    self.metrics_services,
                    self.metrics_history,
                    metrics_shutdown,
                )
                .await;
                Ok::<(), StartupError>(())
            },
            async {
                match self.cluster_process {
                    Some(process) => process
                        .supervise(cluster_shutdown.cancelled_owned())
                        .await
                        .map_err(StartupError::Cluster),
                    None => {
                        cluster_shutdown.cancelled().await;
                        Ok(())
                    }
                }
            },
        )
        .map(|_| ())
    }
}

/// Validates configuration, initializes credentials and durable state, recovers
/// local operations, and runs startup probes.
pub async fn initialize(config: &Config) -> Result<ServerRuntime, StartupError> {
    config.validate().map_err(StartupError::Configuration)?;
    // Preconditions the machine has to meet, checked before any database is
    // created. Without this, a read-only volume or a temporary directory on
    // another filesystem is discovered after the data lock is taken and half
    // the subsystems are open, which is both slower to diagnose and messier to
    // recover from.
    let preflight = preflight::startup_checks(config);
    if preflight.has_failures() {
        return Err(StartupError::Preflight(preflight.failure_summary()));
    }
    for check in preflight
        .checks
        .iter()
        .filter(|check| check.status == preflight::Status::Warn)
    {
        warn!(check = check.name, detail = %check.detail, "start-up check");
    }
    std::fs::create_dir_all(&config.storage.data_directory).map_err(StartupError::DataDirectory)?;
    let process_lock =
        acquire_data_lock(&config.storage.data_directory).map_err(StartupError::DataDirectory)?;
    let (root_access_key, root_secret_key) = config
        .root_credentials()
        .map_err(StartupError::Configuration)?;
    let credentials = Arc::new(
        CredentialManager::open(
            config
                .storage
                .data_directory
                .join("metadata")
                .join("credentials.redb"),
            root_access_key,
            root_secret_key.expose().as_bytes(),
            config
                .auth
                .credential_master_key
                .as_ref()
                .map(|key| key.expose().as_bytes()),
        )
        .await?,
    );

    let cluster_dependencies = if config.server.mode.clustered() {
        Some(cluster::initialize(config).await?)
    } else {
        None
    };
    let metadata_dependency: Arc<dyn MetadataRepository> = match &cluster_dependencies {
        Some(dependencies) => Arc::clone(&dependencies.metadata),
        None => {
            let catalog_path = config
                .storage
                .data_directory
                .join("metadata")
                .join("catalog.redb");
            Arc::new(RedbMetadataRepository::open(catalog_path).await?)
        }
    };
    let audit = Arc::new(
        RedbAuditRepository::open(
            config
                .storage
                .data_directory
                .join("metadata")
                .join("audit.redb"),
        )
        .await?,
    );
    let audit_dependency: Arc<dyn AuditRepository> = audit;
    let webhook_config = WebhookConfig {
        allow_http: config.webhooks.allow_http,
        allow_private_networks: config.webhooks.allow_private_networks,
        request_timeout: Duration::from_secs(config.webhooks.request_timeout_seconds),
        maximum_attempts: config.webhooks.maximum_attempts,
        poll_interval: Duration::from_secs(config.webhooks.poll_interval_seconds),
    };
    let events = Arc::new(
        RedbEventRepository::open(
            config
                .storage
                .data_directory
                .join("metadata")
                .join("events.redb"),
            config
                .auth
                .credential_master_key
                .as_ref()
                .map(|key| key.expose().as_bytes()),
            webhook_config.clone(),
        )
        .await?,
    );
    let event_dependency: Arc<dyn EventRepository> = events;
    let storage_dependency: Arc<dyn ObjectStore> = match &cluster_dependencies {
        Some(dependencies) => Arc::clone(&dependencies.storage),
        None => Arc::new(if config.storage.encryption_enabled {
            let master_key = config
                .auth
                .credential_master_key
                .as_ref()
                .ok_or(StorageError::EncryptionKeyRequired)?;
            LocalFilesystemStore::open_encrypted(
                &config.storage.data_directory,
                config.storage.effective_temporary_directory(),
                Arc::clone(&metadata_dependency),
                master_key.expose().as_bytes(),
            )
            .await?
        } else {
            LocalFilesystemStore::open(
                &config.storage.data_directory,
                config.storage.effective_temporary_directory(),
                Arc::clone(&metadata_dependency),
            )
            .await?
        }),
    };
    let cleanup_storage = Arc::clone(&storage_dependency);

    tokio::try_join!(
        async {
            storage_dependency
                .check_ready()
                .await
                .map_err(StartupError::Storage)
        },
        async {
            metadata_dependency
                .check_ready()
                .await
                .map_err(StartupError::Metadata)
        },
        async {
            event_dependency
                .check_ready()
                .await
                .map_err(StartupError::Events)
        }
    )?;

    let owner = OrganizationId::from_uuid(uuid::Uuid::from_u128(1));
    let services = Services::new_with_audit(
        Arc::clone(&storage_dependency),
        Arc::clone(&metadata_dependency),
        owner,
        ServiceLimits {
            maximum_concurrent_operations: config.limits.maximum_concurrent_operations,
            admission_wait_limit_seconds: config.limits.admission_wait_limit_seconds,
            maximum_custom_metadata_entries: config.limits.maximum_custom_metadata_entries,
            maximum_custom_metadata_bytes: config.limits.maximum_custom_metadata_bytes,
            object_lock: ObjectLockLimits {
                clock_backwards_tolerance_seconds: config
                    .object_lock
                    .clock_backwards_tolerance_seconds,
            },
        },
        Arc::clone(&audit_dependency),
    );
    // Storage events are journalled by the catalog inside the transaction that
    // commits each mutation. The pump is what moves them into the delivery
    // outbox; without it the journal simply grows and nothing is delivered, so
    // it is part of the runtime rather than an optional extra.
    let mut event_pump = StorageEventPump::new(
        Arc::clone(&metadata_dependency),
        Arc::clone(&event_dependency),
        // A second, not the webhook poll interval: the pump is what makes an
        // event visible at all, so the delay before it runs is the delay
        // before anything — the console feed included — can see the event. A
        // pass over an empty journal is one local range read.
        Duration::from_secs(1),
    );
    if let Some(dependencies) = &cluster_dependencies {
        // Every member holds the same journal, so draining from more than one
        // would publish each event into several node-local outboxes and
        // deliver it several times. The gate keeps that to one member.
        event_pump = event_pump.with_activation_gate(Arc::new(cluster::LeaderEventPumpGate::new(
            Arc::clone(&dependencies.consensus),
        )));
    }
    let lifecycle_worker = LifecycleWorker::open(
        config
            .storage
            .data_directory
            .join("metadata")
            .join("lifecycle.redb"),
        Arc::clone(&metadata_dependency),
        services.clone(),
        Arc::clone(&audit_dependency),
        Duration::from_secs(config.lifecycle.interval_seconds),
        config.lifecycle.batch_size,
    )
    .await?;
    // Capability tokens are encrypted at rest under the deployment's master key
    // when one is configured, and under the root secret otherwise — the same
    // derivation the credential store uses. Verification never depends on it, so
    // a key change costs the ability to redisplay a link and nothing else.
    let sharing_key_material = config.auth.credential_master_key.as_ref().map_or_else(
        || root_secret_key.expose().to_owned(),
        |key| key.expose().to_owned(),
    );
    let sharing_store = CapabilityStore::open(
        config
            .storage
            .data_directory
            .join("metadata")
            .join("sharing.redb"),
        sharing_key_material.as_bytes(),
    )
    .await?;
    let sharing_service = Arc::new(SharingService::new(
        sharing_store,
        SharingPolicy {
            shares_enabled: config.sharing.shares_enabled,
            embeds_enabled: config.sharing.embeds_enabled,
            maximum_lifetime: (config.sharing.maximum_lifetime_days > 0)
                .then(|| chrono::Duration::days(i64::from(config.sharing.maximum_lifetime_days))),
            require_expiration: config.sharing.require_expiration,
            require_share_password: config.sharing.require_share_password,
            maximum_access_count: config.sharing.maximum_access_count,
            password_attempts_per_window: config.sharing.password_attempts_per_minute,
            token_probes_per_window: config.sharing.token_probes_per_minute,
            abuse_window: Duration::from_secs(60),
            unlock_lifetime: chrono::Duration::hours(i64::from(
                config.sharing.unlock_lifetime_hours,
            )),
        },
        TicketIssuer::derive(sharing_key_material.as_bytes())?,
    ));
    // Parsed once, here, so a malformed entry stops start-up rather than
    // quietly reverting the deployment to socket-address attribution.
    let trusted_proxies = config
        .server
        .parsed_trusted_proxies()
        .map_err(|error| StartupError::Configuration(ConfigError::Validation(error.to_string())))?;
    // One ring, shared: the sampler writes it and the endpoint reads it.
    let metrics_history = Arc::new(record_store_api::history::MetricsHistory::new(Utc::now()));
    let mut management_state = AppState::new(
        storage_dependency,
        metadata_dependency,
        services.clone(),
        Arc::clone(&credentials),
        Arc::clone(&audit_dependency),
        owner,
        env!("CARGO_PKG_VERSION"),
    )
    .with_mode(config.server.mode)
    .with_trusted_proxies(trusted_proxies.clone())
    .with_metrics_history(Arc::clone(&metrics_history))
    .with_events(Arc::clone(&event_dependency))
    .with_sharing(SharingManagement::new(
        Arc::clone(&sharing_service),
        config.sharing.normalized_share_base_url(),
        config.effective_embed_base_url(),
        config.sharing.preview_text_limit_bytes,
    ));
    // Proof bundles are signed with a key derived from the deployment master
    // key. Without one the endpoint reports bundles as unavailable rather than
    // emitting an unsigned document that would be mistaken for a signed one.
    if let Some(master_key) = config.auth.credential_master_key.as_ref() {
        management_state = management_state.with_proof_signer(Arc::new(
            record_store_proof::BundleSigner::from_master_key(master_key.expose().as_bytes())
                .map_err(StartupError::Proof)?,
        ));
    }
    if let Some(dependencies) = &cluster_dependencies {
        // Discovery never proposes storage that is already in use, so the
        // node's own data directory and every declared device are excluded.
        let mut in_use = vec![config.storage.data_directory.clone()];
        in_use.extend(
            config
                .storage
                .devices
                .iter()
                .map(|device| device.path.clone()),
        );
        management_state = management_state.with_cluster(
            ClusterManagement::new(
                Arc::clone(&dependencies.context),
                Arc::clone(&dependencies.consensus),
                Arc::clone(&dependencies.operations),
                Arc::clone(&dependencies.task_health),
            )
            .with_discovery(Arc::new(crate::discovery::MountDiscovery::new(in_use))),
        );
    }
    if let Some(token) = &config.auth.management_system_token {
        management_state = management_state.with_management_auth(ManagementAuth::bearer_tokens(
            token.expose().as_bytes(),
            config
                .auth
                .management_storage_token
                .as_ref()
                .map(|value| value.expose().as_bytes()),
            config
                .auth
                .management_auditor_token
                .as_ref()
                .map(|value| value.expose().as_bytes()),
        ));
    } else {
        warn!(
            "dedicated management token is not configured; legacy root Basic authentication remains enabled"
        );
    }
    if let Some(token) = &config.auth.metrics_scrape_token {
        management_state = management_state
            .with_metrics_auth(MetricsAuth::bearer_token(token.expose().as_bytes()));
    } else {
        warn!("metrics scrape token is not configured; the metrics endpoint remains closed");
    }
    // The embed surface is built from the same state but mounted on the storage
    // listener below, so an asset URL never touches the management plane.
    let embed_delivery = record_store_api::embed_router(management_state.clone());
    let management = record_store_api::router(management_state);
    let authorizer: Arc<dyn Authorizer> = credentials.clone();
    let credential_provider: Arc<dyn SigningCredentialProvider> = credentials;
    let s3 = record_store_s3::router(
        record_store_s3::S3State::new(services.clone(), credential_provider)
            .with_authorizer(authorizer)
            .with_audit(audit_dependency)
            .with_trusted_proxies(trusted_proxies)
            .with_root_s3_enabled(config.auth.root_s3_enabled)
            .with_maximum_header_bytes(config.limits.maximum_header_bytes),
    )
    // Merged rather than nested, so embed delivery sits alongside the S3
    // operations without passing through their SigV4 layer. The `/e/` prefix
    // cannot shadow a bucket, because a bucket name is at least three
    // characters.
    .merge(embed_delivery);

    Ok(ServerRuntime {
        management,
        s3,
        shutdown_grace_period: Duration::from_secs(config.server.shutdown_grace_period_seconds),
        header_read_timeout: Duration::from_secs(config.server.header_read_timeout_seconds),
        webhook_worker: WebhookWorker::new(
            event_dependency,
            Duration::from_secs(config.webhooks.poll_interval_seconds),
        ),
        event_pump,
        lifecycle_worker,
        process_lock,
        cleanup_storage,
        clock_services: services.clone(),
        clock_watermark_interval: Duration::from_secs(
            config.object_lock.clock_watermark_interval_seconds,
        ),
        metrics_history,
        metrics_services: services,
        cluster_process: cluster_dependencies.map(|dependencies| dependencies.process),
    })
}

/// Initializes and serves Record Store at both configured addresses.
pub async fn run<F>(config: &Config, shutdown: F) -> Result<(), StartupError>
where
    F: Future<Output = ()> + Send + 'static,
{
    // Bound before initialization, not after. An address another process
    // already holds is the most common start-up failure there is, and binding
    // last meant meeting it only once every database was open and the data lock
    // taken — a minute of work to report a fault that is knowable immediately.
    // Nothing is served from these sockets until `serve` runs below.
    let s3_listener = TcpListener::bind(config.server.s3_bind)
        .await
        .map_err(|source| StartupError::Listen {
            interface: "S3",
            source,
        })?;
    let api_listener = TcpListener::bind(config.server.api_bind)
        .await
        .map_err(|source| StartupError::Listen {
            interface: "management",
            source,
        })?;
    let runtime = initialize(config).await?;
    info!(mode = %config.server.mode, "Record Store starting");
    info!(address = %config.server.s3_bind, "S3 API listening");
    info!(address = %config.server.api_bind, "management API listening");
    if !config.server.mode.clustered() {
        info!("Record Store started in standalone mode; internal cluster RPC is not listening");
    }
    runtime.serve(s3_listener, api_listener, shutdown).await
}

/// Waits for Ctrl+C or SIGTERM and is shared by both server entry points.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(error) = result {
                            tracing::error!(%error, "failed to listen for Ctrl+C");
                        }
                    }
                    received = terminate.recv() => {
                        if received.is_none() {
                            tracing::error!("termination signal stream ended unexpectedly");
                        }
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "failed to install termination signal handler");
                let _result = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for Ctrl+C");
    }

    tracing::info!("shutdown requested");
}

/// Keeps the observed-time high-water mark moving while the node is idle.
///
/// Object Lock judges a retention against the furthest point in time this
/// deployment has ever seen. Without a ticker that mark would only advance when
/// somebody happened to write, so a node that sat quiet over a weekend would
/// have nothing to compare a clock against on Monday.
async fn run_clock_watermark(
    services: Services,
    interval: Duration,
    cancellation: CancellationToken,
) {
    run_observation_loop(interval, cancellation, || async {
        // A refusal here is the whole point of the mark, and the service has
        // already logged it. Nothing else to do but keep checking: the clock
        // may yet be corrected.
        let _ = services.locks.observe_clock().await;
    })
    .await;
}

/// Samples the service counters into the bounded history the console reads.
///
/// A rate needs two readings, so somebody has to wait for the second one. Doing
/// it here means the server waits, in the background, from the moment it starts
/// — rather than the person who opened the metrics page waiting while it
/// happens in front of them.
///
/// One sample is taken immediately so a node that has only just started still
/// answers with something, and the ring is bounded, so this runs for months
/// without growing.
async fn run_metrics_sampler(
    services: Services,
    history: Arc<record_store_api::history::MetricsHistory>,
    cancellation: CancellationToken,
) {
    history.observe(Utc::now(), services.metrics.snapshot());
    let interval = Duration::from_secs(record_store_api::history::SAMPLE_INTERVAL_SECONDS);
    run_observation_loop(interval, cancellation, || async {
        // Reading four atomics cannot fail, so there is nothing to report and
        // nothing that could hold up a shutdown.
        history.observe(Utc::now(), services.metrics.snapshot());
    })
    .await;
}

/// Runs a periodic observation that never outlives its cancellation signal.
///
/// The observation is raced against cancellation rather than awaited inside the
/// tick arm. That distinction is the whole point of this helper: in a cluster
/// the observation is a consensus proposal, which can take arbitrarily long or
/// never resolve, and a supervised task still sitting inside one holds up the
/// entire graceful shutdown. An operator meets that as a node that will not
/// stop.
async fn run_observation_loop<F, Fut>(
    interval: Duration,
    cancellation: CancellationToken,
    observe: F,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // A tokio interval's first tick completes immediately. There is nothing to
    // observe the instant the catalog was opened, and in a cluster this would
    // aim a replicated write at the one moment the node is least able to serve
    // one.
    ticker.tick().await;
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return,
            _ = ticker.tick() => {}
        }
        tokio::select! {
            () = cancellation.cancelled() => return,
            () = observe() => {}
        }
    }
}

async fn run_payload_cleanup(storage: Arc<dyn ObjectStore>, cancellation: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!("payload cleanup worker started");
    loop {
        tokio::select! {
            () = cancellation.cancelled() => {
                info!("payload cleanup worker stopped");
                return;
            }
            _ = interval.tick() => match storage.cleanup_pending(1_000).await {
                Ok(completed) if completed > 0 => info!(completed, "processed deferred payload cleanup"),
                Ok(_) => {},
                Err(error) => tracing::error!(%error, "deferred payload cleanup scan failed"),
            }
        }
    }
}

const METADATA_BACKUP_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct MetadataBackupManifest {
    backup_format_version: u32,
    metadata_schema_version: u64,
    created_unix_seconds: u64,
    files: Vec<MetadataBackupFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MetadataBackupFile {
    name: String,
    size: u64,
    sha256: String,
}

/// Creates a consistent offline backup of Record Store metadata databases.
///
/// The operation refuses to run while a server owns the data-directory lock and
/// never includes object payload bytes or secret configuration values.
pub fn backup_metadata(config: &Config, output: &Path) -> Result<(), MetadataBackupError> {
    config
        .validate()
        .map_err(MetadataBackupError::Configuration)?;
    std::fs::create_dir_all(&config.storage.data_directory).map_err(MetadataBackupError::Io)?;
    let _lock = acquire_data_lock(&config.storage.data_directory)
        .map_err(MetadataBackupError::DataDirectoryInUse)?;
    if output.exists() {
        return Err(MetadataBackupError::DestinationExists(output.to_path_buf()));
    }
    std::fs::create_dir(output).map_err(MetadataBackupError::Io)?;
    let source = config.storage.data_directory.join("metadata");
    let mut files = Vec::new();
    if source.exists() {
        for entry in std::fs::read_dir(source).map_err(MetadataBackupError::Io)? {
            let entry = entry.map_err(MetadataBackupError::Io)?;
            if !entry
                .file_type()
                .map_err(MetadataBackupError::Io)?
                .is_file()
            {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| MetadataBackupError::InvalidFilename)?;
            if !name.ends_with(".redb") {
                continue;
            }
            let destination = output.join(&name);
            let (size, sha256) = copy_with_checksum(&entry.path(), &destination)?;
            files.push(MetadataBackupFile { name, size, sha256 });
        }
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    let manifest = MetadataBackupManifest {
        backup_format_version: METADATA_BACKUP_FORMAT_VERSION,
        metadata_schema_version: record_store_metadata::METADATA_SCHEMA_VERSION,
        created_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        files,
    };
    let encoded = serde_json::to_vec_pretty(&manifest)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("manifest.json"))
        .map_err(MetadataBackupError::Io)?;
    file.write_all(&encoded).map_err(MetadataBackupError::Io)?;
    file.sync_all().map_err(MetadataBackupError::Io)?;
    Ok(())
}

/// Restores a validated offline metadata backup into an empty metadata location.
pub fn restore_metadata(config: &Config, input: &Path) -> Result<(), MetadataBackupError> {
    config
        .validate()
        .map_err(MetadataBackupError::Configuration)?;
    std::fs::create_dir_all(&config.storage.data_directory).map_err(MetadataBackupError::Io)?;
    let _lock = acquire_data_lock(&config.storage.data_directory)
        .map_err(MetadataBackupError::DataDirectoryInUse)?;
    let target = config.storage.data_directory.join("metadata");
    if target.exists()
        && std::fs::read_dir(&target)
            .map_err(MetadataBackupError::Io)?
            .next()
            .is_some()
    {
        return Err(MetadataBackupError::RestoreTargetNotEmpty(target));
    }
    let manifest_bytes =
        std::fs::read(input.join("manifest.json")).map_err(MetadataBackupError::Io)?;
    if manifest_bytes.len() > 1024 * 1024 {
        return Err(MetadataBackupError::InvalidManifest);
    }
    let manifest: MetadataBackupManifest = serde_json::from_slice(&manifest_bytes)?;
    if manifest.backup_format_version != METADATA_BACKUP_FORMAT_VERSION
        || manifest.metadata_schema_version > record_store_metadata::METADATA_SCHEMA_VERSION
    {
        return Err(MetadataBackupError::IncompatibleVersion);
    }
    let temporary = config
        .storage
        .data_directory
        .join(format!("metadata.restore-{}", Uuid::new_v4().simple()));
    std::fs::create_dir(&temporary).map_err(MetadataBackupError::Io)?;
    for expected in &manifest.files {
        if !valid_backup_filename(&expected.name) {
            return Err(MetadataBackupError::InvalidFilename);
        }
        let (size, sha256) =
            copy_with_checksum(&input.join(&expected.name), &temporary.join(&expected.name))?;
        if size != expected.size || sha256 != expected.sha256 {
            return Err(MetadataBackupError::ChecksumMismatch(expected.name.clone()));
        }
    }
    if target.exists() {
        std::fs::remove_dir(&target).map_err(MetadataBackupError::Io)?;
    }
    std::fs::rename(temporary, target).map_err(MetadataBackupError::Io)?;
    Ok(())
}

pub(crate) fn acquire_data_lock(data_directory: &Path) -> Result<File, std::io::Error> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_directory.join(".record-store.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    Ok(lock)
}

fn copy_with_checksum(
    source: &Path,
    destination: &Path,
) -> Result<(u64, String), MetadataBackupError> {
    let mut source = File::open(source).map_err(MetadataBackupError::Io)?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(MetadataBackupError::Io)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    loop {
        let read = source.read(&mut buffer).map_err(MetadataBackupError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        destination
            .write_all(&buffer[..read])
            .map_err(MetadataBackupError::Io)?;
        size = size.saturating_add(read as u64);
    }
    destination.sync_all().map_err(MetadataBackupError::Io)?;
    Ok((size, hex::encode(hasher.finalize())))
}

fn valid_backup_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.ends_with(".redb")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Offline metadata backup/restore failure categories.
#[derive(Debug, Error)]
pub enum MetadataBackupError {
    #[error("invalid configuration: {0}")]
    Configuration(record_store_config::ConfigError),
    #[error("metadata backup I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("the data directory is in use by another Record Store process: {0}")]
    DataDirectoryInUse(#[source] std::io::Error),
    #[error("backup destination already exists: {}", .0.display())]
    DestinationExists(PathBuf),
    #[error("restore target is not empty: {}", .0.display())]
    RestoreTargetNotEmpty(PathBuf),
    #[error("backup manifest is invalid")]
    InvalidManifest,
    #[error("backup format or metadata schema is incompatible")]
    IncompatibleVersion,
    #[error("backup contains an invalid filename")]
    InvalidFilename,
    #[error("backup checksum did not match for {0}")]
    ChecksumMismatch(String),
    #[error("backup manifest encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
}

/// Failures during explicit process initialization or serving.
#[derive(Debug, Error)]
pub enum StartupError {
    /// Resolved configuration was invalid.
    #[error("invalid configuration: {0}")]
    Configuration(record_store_config::ConfigError),
    /// A start-up precondition of the machine itself was not met.
    #[error("start-up checks failed: {0}")]
    Preflight(String),
    /// Credential initialization failed.
    #[error("credential initialization failed: {0}")]
    Credentials(#[from] CredentialStoreError),
    /// Audit initialization or probing failed.
    #[error("audit initialization failed: {0}")]
    Audit(#[from] AuditError),
    /// Capability store initialization failed.
    #[error("sharing initialization failed: {0}")]
    Sharing(#[from] record_store_sharing::SharingError),
    /// Event and webhook initialization or supervision failed.
    #[error("event subsystem failed: {0}")]
    Events(#[from] EventError),
    /// Lifecycle initialization or supervision failed.
    #[error("lifecycle subsystem failed: {0}")]
    Lifecycle(#[from] LifecycleError),
    /// Data directory or exclusive process lock could not be prepared.
    #[error("data directory is unavailable or already in use: {0}")]
    DataDirectory(#[source] std::io::Error),
    /// Metadata initialization or probing failed.
    #[error("metadata initialization failed: {0}")]
    Metadata(#[from] MetadataError),
    /// Storage initialization or probing failed.
    #[error("storage initialization failed: {0}")]
    Storage(#[from] StorageError),
    /// Distributed cluster initialization or supervision failed.
    #[error("cluster subsystem failed: {0}")]
    Cluster(#[from] cluster::ClusterStartupError),
    /// A configured TCP address could not be bound.
    #[error("failed to bind {interface} HTTP listener: {source}")]
    Listen {
        /// Listener role.
        interface: &'static str,
        /// Socket error.
        #[source]
        source: std::io::Error,
    },
    /// HTTP serving or graceful shutdown failed.
    #[error("HTTP lifecycle failed: {0}")]
    Http(record_store_api::ServerError),
    #[error("deriving the proof bundle signing key failed: {0}")]
    Proof(record_store_proof::ProofError),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Notify;

    use super::*;

    /// A supervised worker must never outlive its cancellation, even when the
    /// work it is doing never finishes.
    ///
    /// This is not hypothetical. The Object Lock clock observation is a
    /// consensus proposal in a cluster, and a node mid-join has no leader to
    /// accept it. Awaiting that inside the tick arm made graceful shutdown wait
    /// for a write that might never land, which an operator meets as a node
    /// that will not stop.
    #[tokio::test]
    async fn an_observation_that_never_finishes_still_stops_at_cancellation() {
        let cancellation = CancellationToken::new();
        let observing = Arc::new(Notify::new());
        let worker = {
            let observing = Arc::clone(&observing);
            tokio::spawn(run_observation_loop(
                Duration::from_millis(10),
                cancellation.clone(),
                move || {
                    let observing = Arc::clone(&observing);
                    async move {
                        observing.notify_one();
                        // Stands in for a proposal with no leader to accept it.
                        std::future::pending::<()>().await
                    }
                },
            ))
        };

        // Cancel only once the worker is genuinely inside the observation, so
        // this tests the in-flight case rather than the idle one.
        tokio::time::timeout(Duration::from_secs(5), observing.notified())
            .await
            .expect("the worker reaches its first observation");
        cancellation.cancel();

        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("a worker stuck in an observation must still stop when cancelled")
            .expect("worker task");
    }

    /// An idle worker stops too, without waiting for the next tick to come
    /// around. A long interval must not become a long shutdown.
    #[tokio::test]
    async fn a_worker_between_ticks_stops_without_waiting_for_the_next_one() {
        let cancellation = CancellationToken::new();
        let worker = tokio::spawn(run_observation_loop(
            // Far longer than the bound below: if the loop waited for a tick,
            // this could not pass.
            Duration::from_secs(3_600),
            cancellation.clone(),
            || async {},
        ));
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("cancellation does not wait for the next tick")
            .expect("worker task");
    }

    /// The first tick of a tokio interval fires immediately. Consuming it keeps
    /// a replicated write out of the boot window, where a cluster node has not
    /// necessarily joined a consensus group yet.
    #[tokio::test]
    async fn the_loop_does_not_observe_immediately_on_startup() {
        let cancellation = CancellationToken::new();
        let observations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = {
            let observations = Arc::clone(&observations);
            tokio::spawn(run_observation_loop(
                Duration::from_secs(3_600),
                cancellation.clone(),
                move || {
                    let observations = Arc::clone(&observations);
                    async move {
                        observations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                },
            ))
        };

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            observations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the worker must wait a full interval before its first observation"
        );
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("bounded stop")
            .expect("worker task");
    }
}
