//! Explicit OES server initialization and dual-listener lifecycle orchestration.

mod cluster;

use std::{
    fs::{File, OpenOptions},
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use oes_api::{AppState, ClusterManagement, ManagementAuth, MetricsAuth};
use oes_audit::{AuditError, AuditRepository, RedbAuditRepository};
use oes_auth::{Authorizer, CredentialManager, CredentialStoreError, SigningCredentialProvider};
use oes_config::Config;
use oes_core::OrganizationId;
use oes_events::{EventError, EventRepository, RedbEventRepository, WebhookConfig, WebhookWorker};
use oes_lifecycle::{LifecycleError, LifecycleWorker};
use oes_metadata::{MetadataError, MetadataRepository, RedbMetadataRepository};
use oes_service::{ServiceLimits, Services};
use oes_storage::{LocalFilesystemStore, ObjectStore, StorageError};
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
    webhook_worker: WebhookWorker,
    lifecycle_worker: LifecycleWorker,
    process_lock: File,
    cleanup_storage: Arc<dyn ObjectStore>,
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
        let lifecycle_shutdown = cancellation;
        let cleanup_shutdown = lifecycle_shutdown.clone();
        let cluster_shutdown = lifecycle_shutdown.clone();
        tokio::try_join!(
            async {
                oes_api::serve(
                    s3_listener,
                    self.s3,
                    s3_shutdown,
                    self.shutdown_grace_period,
                )
                .await
                .map_err(StartupError::Http)
            },
            async {
                oes_api::serve(
                    api_listener,
                    self.management,
                    api_shutdown,
                    self.shutdown_grace_period,
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
    let services = Services::new_with_events(
        Arc::clone(&storage_dependency),
        Arc::clone(&metadata_dependency),
        owner,
        ServiceLimits {
            maximum_concurrent_operations: config.limits.maximum_concurrent_operations,
            maximum_custom_metadata_entries: config.limits.maximum_custom_metadata_entries,
            maximum_custom_metadata_bytes: config.limits.maximum_custom_metadata_bytes,
        },
        Some(Arc::clone(&event_dependency)),
    );
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
    .with_events(Arc::clone(&event_dependency));
    if let Some(dependencies) = &cluster_dependencies {
        management_state = management_state.with_cluster(ClusterManagement::new(
            Arc::clone(&dependencies.context),
            Arc::clone(&dependencies.consensus),
            Arc::clone(&dependencies.operations),
            Arc::clone(&dependencies.task_health),
        ));
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
    let management = oes_api::router(management_state);
    let authorizer: Arc<dyn Authorizer> = credentials.clone();
    let credential_provider: Arc<dyn SigningCredentialProvider> = credentials;
    let s3 = oes_s3::router(
        oes_s3::S3State::new(services, credential_provider)
            .with_authorizer(authorizer)
            .with_audit(audit_dependency)
            .with_root_s3_enabled(config.auth.root_s3_enabled)
            .with_maximum_header_bytes(config.limits.maximum_header_bytes),
    );

    Ok(ServerRuntime {
        management,
        s3,
        shutdown_grace_period: Duration::from_secs(config.server.shutdown_grace_period_seconds),
        webhook_worker: WebhookWorker::new(
            event_dependency,
            Duration::from_secs(config.webhooks.poll_interval_seconds),
        ),
        lifecycle_worker,
        process_lock,
        cleanup_storage,
        cluster_process: cluster_dependencies.map(|dependencies| dependencies.process),
    })
}

/// Initializes and serves OES at both configured addresses.
pub async fn run<F>(config: &Config, shutdown: F) -> Result<(), StartupError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let runtime = initialize(config).await?;
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
    info!(mode = %config.server.mode, "OES starting");
    info!(address = %config.server.s3_bind, "S3 API listening");
    info!(address = %config.server.api_bind, "management API listening");
    if !config.server.mode.clustered() {
        info!("OES started in standalone mode; internal cluster RPC is not listening");
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

/// Creates a consistent offline backup of OES metadata databases.
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
        metadata_schema_version: oes_metadata::METADATA_SCHEMA_VERSION,
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
        || manifest.metadata_schema_version > oes_metadata::METADATA_SCHEMA_VERSION
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

fn acquire_data_lock(data_directory: &Path) -> Result<File, std::io::Error> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_directory.join(".oes.lock"))?;
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
    Configuration(oes_config::ConfigError),
    #[error("metadata backup I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("the data directory is in use by another OES process: {0}")]
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
    Configuration(oes_config::ConfigError),
    /// Credential initialization failed.
    #[error("credential initialization failed: {0}")]
    Credentials(#[from] CredentialStoreError),
    /// Audit initialization or probing failed.
    #[error("audit initialization failed: {0}")]
    Audit(#[from] AuditError),
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
    Http(oes_api::ServerError),
}
