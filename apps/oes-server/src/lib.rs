//! Explicit OES server initialization and dual-listener lifecycle orchestration.

use std::{future::Future, sync::Arc, time::Duration};

use oes_api::AppState;
use oes_auth::{CredentialManager, CredentialStoreError, SigningCredentialProvider};
use oes_config::Config;
use oes_core::OrganizationId;
use oes_metadata::{MetadataError, MetadataRepository, RedbMetadataRepository};
use oes_service::{ServiceLimits, Services};
use oes_storage::{LocalFilesystemStore, ObjectStore, StorageError};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Initialized dependencies ready for S3 and management listeners.
pub struct ServerRuntime {
    management: axum::Router,
    s3: axum::Router,
    shutdown_grace_period: Duration,
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
        let cancellation = CancellationToken::new();
        let signal_token = cancellation.clone();
        tokio::spawn(async move {
            shutdown.await;
            signal_token.cancel();
        });
        let s3_shutdown = cancellation.clone().cancelled_owned();
        let api_shutdown = cancellation.cancelled_owned();
        tokio::try_join!(
            oes_api::serve(
                s3_listener,
                self.s3,
                s3_shutdown,
                self.shutdown_grace_period,
            ),
            oes_api::serve(
                api_listener,
                self.management,
                api_shutdown,
                self.shutdown_grace_period,
            )
        )
        .map(|_| ())
        .map_err(StartupError::Http)
    }
}

/// Validates configuration, initializes credentials and durable state, recovers
/// local operations, and runs startup probes.
pub async fn initialize(config: &Config) -> Result<ServerRuntime, StartupError> {
    config.validate().map_err(StartupError::Configuration)?;
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

    let catalog_path = config
        .storage
        .data_directory
        .join("metadata")
        .join("catalog.redb");
    let metadata = Arc::new(RedbMetadataRepository::open(catalog_path).await?);
    let metadata_dependency: Arc<dyn MetadataRepository> = metadata;
    let storage = Arc::new(
        LocalFilesystemStore::open(
            &config.storage.data_directory,
            config.storage.effective_temporary_directory(),
            Arc::clone(&metadata_dependency),
        )
        .await?,
    );
    let storage_dependency: Arc<dyn ObjectStore> = storage;

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
        }
    )?;

    let owner = OrganizationId::from_uuid(uuid::Uuid::from_u128(1));
    let services = Services::new(
        Arc::clone(&storage_dependency),
        Arc::clone(&metadata_dependency),
        owner,
        ServiceLimits {
            maximum_concurrent_operations: config.limits.maximum_concurrent_operations,
            maximum_custom_metadata_entries: config.limits.maximum_custom_metadata_entries,
            maximum_custom_metadata_bytes: config.limits.maximum_custom_metadata_bytes,
        },
    );
    let management = oes_api::router(AppState::new(
        storage_dependency,
        metadata_dependency,
        services.clone(),
        Arc::clone(&credentials),
        owner,
        env!("CARGO_PKG_VERSION"),
    ));
    let credential_provider: Arc<dyn SigningCredentialProvider> = credentials;
    let s3 = oes_s3::router(
        oes_s3::S3State::new(services, credential_provider)
            .with_maximum_header_bytes(config.limits.maximum_header_bytes),
    );

    Ok(ServerRuntime {
        management,
        s3,
        shutdown_grace_period: Duration::from_secs(config.server.shutdown_grace_period_seconds),
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
    info!(address = %config.server.s3_bind, "S3 API listening");
    info!(address = %config.server.api_bind, "management API listening");
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

/// Failures during explicit process initialization or serving.
#[derive(Debug, Error)]
pub enum StartupError {
    /// Resolved configuration was invalid.
    #[error("invalid configuration: {0}")]
    Configuration(oes_config::ConfigError),
    /// Credential initialization failed.
    #[error("credential initialization failed: {0}")]
    Credentials(#[from] CredentialStoreError),
    /// Metadata initialization or probing failed.
    #[error("metadata initialization failed: {0}")]
    Metadata(#[from] MetadataError),
    /// Storage initialization or probing failed.
    #[error("storage initialization failed: {0}")]
    Storage(#[from] StorageError),
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
