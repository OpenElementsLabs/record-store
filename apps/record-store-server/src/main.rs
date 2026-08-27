use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use record_store_config::Config;

#[derive(Debug, Parser)]
#[command(
    name = "record-store-server",
    version,
    about = "Record Store storage server"
)]
struct Arguments {
    /// TOML configuration file. Environment variables override file values.
    #[arg(long, env = "RECORD_STORE_CONFIG_FILE")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let config =
        Config::load(arguments.config.as_deref()).context("load Record Store configuration")?;
    record_store_observability::init(&config.observability).context("initialize observability")?;
    record_store_server::run(&config, record_store_server::shutdown_signal())
        .await
        .context("run Record Store server")
}
