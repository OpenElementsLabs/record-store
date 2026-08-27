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
    oes_server::run(&config, oes_server::shutdown_signal())
        .await
        .context("run OES server")
}
