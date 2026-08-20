use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use oes_config::Config;
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(name = "oes", version, about = "Operational CLI for OES")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the OES CLI version.
    Version,
    /// Server configuration and status operations.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ServerCommand {
    /// Load and validate server configuration without starting OES.
    CheckConfig {
        /// TOML configuration file. Environment variables override file values.
        #[arg(long, env = "OES_CONFIG_FILE")]
        config: Option<PathBuf>,
    },
    /// Query the readiness endpoint of a running server.
    Status {
        /// OES HTTP endpoint.
        #[arg(long, default_value = "http://127.0.0.1:9000")]
        endpoint: String,
    },
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    status: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Version => println!("oes {}", env!("CARGO_PKG_VERSION")),
        Command::Server {
            command: ServerCommand::CheckConfig { config },
        } => {
            Config::load(config.as_deref()).context("configuration is invalid")?;
            println!("configuration is valid");
        }
        Command::Server {
            command: ServerCommand::Status { endpoint },
        } => status(&endpoint).await?,
    }
    Ok(())
}

async fn status(endpoint: &str) -> Result<()> {
    let url = format!("{}/ready", endpoint.trim_end_matches('/'));
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build HTTP client")?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("connect to {endpoint}"))?;
    if !response.status().is_success() {
        bail!("server is not ready (HTTP {})", response.status());
    }
    let body: StatusResponse = response.json().await.context("decode readiness response")?;
    if body.status != "ready" {
        bail!("server returned unexpected readiness status");
    }
    println!("ready");
    Ok(())
}
