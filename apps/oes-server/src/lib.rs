//! Explicit OES server initialization and lifecycle orchestration.

use std::{future::Future, sync::Arc, time::Duration};

use oes_api::AppState;
use oes_config::Config;
use oes_metadata::{MetadataError, MetadataRepository, RedbMetadataRepository};
use oes_storage::{LocalFilesystemStore, ObjectStore, StorageError};
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::info;

/// Initialized dependencies ready to be attached to an HTTP listener.
pub struct ServerRuntime {
    state: AppState,
    maximum_request_size: usize,
    shutdown_grace_period: Duration,
}

impl ServerRuntime {
    /// Serves an initialized runtime on an existing listener.
    pub async fn serve<F>(self, listener: TcpListener, shutdown: F) -> Result<(), StartupError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let application = oes_api::router(self.state, self.maximum_request_size);
        oes_api::serve(listener, application, shutdown, self.shutdown_grace_period)
            .await
            .map_err(StartupError::Http)
    }
}

/// Validates configuration, initializes durable dependencies, and runs startup probes.
pub async fn initialize(config: &Config) -> Result<ServerRuntime, StartupError> {
    config.validate().map_err(StartupError::Configuration)?;

    let catalog_path = config
        .storage
        .data_directory
        .join("metadata")
        .join("catalog.redb");
    let metadata = Arc::new(RedbMetadataRepository::open(catalog_path).await?);
    let metadata_dependency: Arc<dyn MetadataRepository> = metadata.clone();

    let storage = Arc::new(
        LocalFilesystemStore::open(
            &config.storage.data_directory,
            config.storage.effective_temporary_directory(),
            metadata_dependency.clone(),
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

    Ok(ServerRuntime {
        state: AppState::new(
            storage_dependency,
            metadata_dependency,
            env!("CARGO_PKG_VERSION"),
        ),
        maximum_request_size: config.server.max_request_size_bytes as usize,
        shutdown_grace_period: Duration::from_secs(config.server.shutdown_grace_period_seconds),
    })
}

/// Initializes and serves OES at the configured address.
pub async fn run<F>(config: &Config, shutdown: F) -> Result<(), StartupError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let runtime = initialize(config).await?;
    let address = config.listen_address();
    let listener = TcpListener::bind(address)
        .await
        .map_err(StartupError::Listen)?;
    info!(address = %address, "OES server listening");
    runtime.serve(listener, shutdown).await
}

/// Failures during explicit process initialization or serving.
#[derive(Debug, Error)]
pub enum StartupError {
    /// Resolved configuration was invalid.
    #[error("invalid configuration: {0}")]
    Configuration(oes_config::ConfigError),
    /// Metadata initialization or probing failed.
    #[error("metadata initialization failed: {0}")]
    Metadata(#[from] MetadataError),
    /// Storage initialization or probing failed.
    #[error("storage initialization failed: {0}")]
    Storage(#[from] StorageError),
    /// The configured TCP address could not be bound.
    #[error("failed to bind HTTP listener: {0}")]
    Listen(#[source] std::io::Error),
    /// HTTP serving or graceful shutdown failed.
    #[error("HTTP lifecycle failed: {0}")]
    Http(oes_api::ServerError),
}
