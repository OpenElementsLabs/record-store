use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use oes_config::Config;
use oes_core::Bucket;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "oes", version, about = "Operational CLI for OES")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the OES CLI version.
    Version,
    /// Start or validate the OES server.
    Server(ServerArgs),
    /// Query a running server's readiness.
    Status(EndpointArgs),
    /// Administer buckets through the native API.
    Bucket {
        #[command(subcommand)]
        command: BucketCommand,
    },
    /// Administer service accounts through the native API.
    ServiceAccount {
        #[command(subcommand)]
        command: ServiceAccountCommand,
    },
}

#[derive(Args)]
struct ServerArgs {
    /// TOML configuration file. Environment variables override file values.
    #[arg(long, env = "OES_CONFIG_FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<ServerCommand>,
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Validate configuration without starting listeners.
    CheckConfig,
}

#[derive(Args, Clone)]
struct EndpointArgs {
    /// OES native management endpoint.
    #[arg(long, default_value = "http://127.0.0.1:7601")]
    endpoint: String,
}

#[derive(Subcommand)]
enum BucketCommand {
    /// List buckets.
    List(EndpointArgs),
    /// Create a bucket.
    Create {
        /// Validated S3 bucket name.
        name: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Delete an empty bucket.
    Delete {
        /// Bucket name.
        name: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum ServiceAccountCommand {
    /// List service accounts.
    List(EndpointArgs),
    /// Create an account and print its secret once.
    Create {
        /// Operator-facing account name.
        name: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Disable an account and its credential.
    Revoke {
        /// Service account identifier.
        id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Deserialize)]
struct StatusResponse {
    status: String,
}

#[derive(Serialize)]
struct NameRequest<'a> {
    name: &'a str,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Version => println!("oes {}", env!("CARGO_PKG_VERSION")),
        Command::Server(arguments) => match arguments.command {
            Some(ServerCommand::CheckConfig) => {
                Config::load(arguments.config.as_deref()).context("configuration is invalid")?;
                println!("configuration is valid");
            }
            None => {
                let config =
                    Config::load(arguments.config.as_deref()).context("load OES configuration")?;
                oes_observability::init(&config.observability)
                    .context("initialize observability")?;
                oes_server::run(&config, oes_server::shutdown_signal())
                    .await
                    .context("run OES server")?;
            }
        },
        Command::Status(endpoint) => status(&endpoint.endpoint).await?,
        Command::Bucket { command } => bucket(command).await?,
        Command::ServiceAccount { command } => service_account(command).await?,
    }
    Ok(())
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build HTTP client")
}

fn admin_request(builder: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
    let access = env::var("OES_ROOT_ACCESS_KEY").context("OES_ROOT_ACCESS_KEY is required")?;
    let secret = env::var("OES_ROOT_SECRET_KEY").context("OES_ROOT_SECRET_KEY is required")?;
    Ok(builder.basic_auth(access, Some(secret)))
}

async fn status(endpoint: &str) -> Result<()> {
    let response = client()?
        .get(format!("{}/ready", endpoint.trim_end_matches('/')))
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

async fn bucket(command: BucketCommand) -> Result<()> {
    match command {
        BucketCommand::List(endpoint) => {
            let request = client()?.get(format!(
                "{}/api/v1/buckets",
                endpoint.endpoint.trim_end_matches('/')
            ));
            let response = send_admin(request).await?;
            for bucket in response
                .json::<Vec<Bucket>>()
                .await
                .context("decode bucket list")?
            {
                println!("{}", bucket.name);
            }
        }
        BucketCommand::Create { name, endpoint } => {
            let request = client()?
                .post(format!(
                    "{}/api/v1/buckets",
                    endpoint.endpoint.trim_end_matches('/')
                ))
                .json(&NameRequest { name: &name });
            let bucket = send_admin(request)
                .await?
                .json::<Bucket>()
                .await
                .context("decode created bucket")?;
            println!("{}", bucket.name);
        }
        BucketCommand::Delete { name, endpoint } => {
            let request = client()?.delete(format!(
                "{}/api/v1/buckets/{name}",
                endpoint.endpoint.trim_end_matches('/')
            ));
            send_admin(request).await?;
            println!("deleted {name}");
        }
    }
    Ok(())
}

async fn service_account(command: ServiceAccountCommand) -> Result<()> {
    match command {
        ServiceAccountCommand::List(endpoint) => {
            let request = client()?.get(format!(
                "{}/api/v1/service-accounts",
                endpoint.endpoint.trim_end_matches('/')
            ));
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode service-account list")?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        ServiceAccountCommand::Create { name, endpoint } => {
            let request = client()?
                .post(format!(
                    "{}/api/v1/service-accounts",
                    endpoint.endpoint.trim_end_matches('/')
                ))
                .json(&NameRequest { name: &name });
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode issued credential")?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        ServiceAccountCommand::Revoke { id, endpoint } => {
            let request = client()?.delete(format!(
                "{}/api/v1/service-accounts/{id}",
                endpoint.endpoint.trim_end_matches('/')
            ));
            send_admin(request).await?;
            println!("revoked {id}");
        }
    }
    Ok(())
}

async fn send_admin(builder: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let response = admin_request(builder)?
        .send()
        .await
        .context("send management request")?;
    if response.status().is_success() {
        Ok(response)
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("management API returned HTTP {status}: {body}")
    }
}
