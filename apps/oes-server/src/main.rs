use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use oes_config::Config;

#[derive(Debug, Parser)]
#[command(name = "oes-server", version, about = "OES storage server")]
struct Arguments {
    /// TOML configuration file. Environment variables override file values.
    #[arg(long, env = "OES_CONFIG_FILE")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let config = Config::load(arguments.config.as_deref()).context("load OES configuration")?;
    oes_observability::init(&config.observability).context("initialize observability")?;
    oes_server::run(&config, shutdown_signal())
        .await
        .context("run OES server")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let terminate = signal(SignalKind::terminate());
        match terminate {
            Ok(mut terminate) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(error) = result {
                            tracing::error!(%error, "failed to listen for Ctrl+C");
                        }
                    }
                    signal = terminate.recv() => {
                        if signal.is_none() {
                            tracing::error!("termination signal stream ended unexpectedly");
                        }
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "failed to install termination signal handler");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for Ctrl+C");
    }

    tracing::info!("shutdown requested");
}
