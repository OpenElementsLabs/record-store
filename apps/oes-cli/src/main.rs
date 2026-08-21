use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use oes_config::Config;
use oes_core::Bucket;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "oes", version, about = "Operational CLI for OES")]
struct Cli {
    /// Emit JSON suitable for automation.
    #[arg(long, global = true)]
    json: bool,
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
    /// Rotate or change service-account credential state.
    Credential {
        #[command(subcommand)]
        command: CredentialCommand,
    },
    /// Administer authorization policies.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Administer signed storage-event webhooks.
    Webhook {
        #[command(subcommand)]
        command: WebhookCommand,
    },
    /// Query the durable security audit trail.
    Audit(AuditArgs),
    /// Verify persisted checksums.
    Verify {
        #[command(subcommand)]
        command: VerifyCommand,
    },
    /// Inspect or explicitly repair OES-owned storage state.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
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
    /// Create a consistent offline metadata backup directory.
    BackupMetadata { output: PathBuf },
    /// Restore a validated offline backup into an empty metadata directory.
    RestoreMetadata { input: PathBuf },
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
    /// Inspect or change bucket versioning.
    Versioning {
        #[command(subcommand)]
        command: BucketVersioningCommand,
    },
}

#[derive(Subcommand)]
enum BucketVersioningCommand {
    Get {
        name: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Enable {
        name: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Suspend {
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
    /// Inspect one account.
    Inspect {
        id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Enable an account.
    Enable {
        id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Disable an account without deleting it.
    Disable {
        id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum CredentialCommand {
    Rotate {
        account_id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Enable {
        account_id: String,
        credential_id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Disable {
        account_id: String,
        credential_id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Issue an automatically expiring credential inheriting the account's policies.
    Temporary {
        account_id: String,
        #[arg(long, default_value_t = 3600)]
        expires_in_seconds: u64,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum PolicyCommand {
    List(EndpointArgs),
    /// Create a policy from a JSON request document.
    Create {
        file: PathBuf,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Attach {
        policy_id: String,
        account_id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Detach {
        policy_id: String,
        account_id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum WebhookCommand {
    List(EndpointArgs),
    /// Create a webhook from a JSON request document.
    Create {
        file: PathBuf,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Deliveries {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Args)]
struct AuditArgs {
    #[arg(long, default_value_t = 100)]
    limit: usize,
    #[arg(long)]
    principal: Option<String>,
    #[arg(long)]
    operation: Option<String>,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Subcommand)]
enum VerifyCommand {
    Object {
        bucket: String,
        key: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Bucket {
        bucket: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum StorageCommand {
    Inspect {
        #[arg(long, default_value_t = 100_000)]
        maximum_entries: usize,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Repair {
        #[arg(long, default_value_t = 100_000)]
        maximum_entries: usize,
        /// Apply deletion of positively identified orphan payloads.
        #[arg(long)]
        apply: bool,
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
    let arguments = Cli::parse();
    let json = arguments.json;
    match arguments.command {
        Command::Version => println!("oes {}", env!("CARGO_PKG_VERSION")),
        Command::Server(arguments) => match arguments.command {
            Some(ServerCommand::CheckConfig) => {
                Config::load(arguments.config.as_deref()).context("configuration is invalid")?;
                println!("configuration is valid");
            }
            Some(ServerCommand::BackupMetadata { output }) => {
                let config =
                    Config::load(arguments.config.as_deref()).context("load OES configuration")?;
                oes_server::backup_metadata(&config, &output).context("back up OES metadata")?;
                println!("metadata backup created at {}", output.display());
            }
            Some(ServerCommand::RestoreMetadata { input }) => {
                let config =
                    Config::load(arguments.config.as_deref()).context("load OES configuration")?;
                oes_server::restore_metadata(&config, &input).context("restore OES metadata")?;
                println!("metadata restored from {}", input.display());
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
        Command::Bucket { command } => bucket(command, json).await?,
        Command::ServiceAccount { command } => service_account(command, json).await?,
        Command::Credential { command } => credential(command, json).await?,
        Command::Policy { command } => policy(command, json).await?,
        Command::Webhook { command } => webhook(command, json).await?,
        Command::Audit(arguments) => audit(arguments, json).await?,
        Command::Verify { command } => verify(command, json).await?,
        Command::Storage { command } => storage(command, json).await?,
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
    if let Ok(token) = env::var("OES_MANAGEMENT_TOKEN") {
        return Ok(builder.bearer_auth(token));
    }
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

async fn bucket(command: BucketCommand, json: bool) -> Result<()> {
    match command {
        BucketCommand::List(endpoint) => {
            let request = client()?.get(format!(
                "{}/api/v1/buckets",
                endpoint.endpoint.trim_end_matches('/')
            ));
            let response = send_admin(request).await?;
            let buckets = response
                .json::<Vec<Bucket>>()
                .await
                .context("decode bucket list")?;
            if json {
                print_json(&buckets)?;
            } else {
                for bucket in buckets {
                    println!("{}", bucket.name);
                }
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
            if json {
                print_json(&bucket)?;
            } else {
                println!("{}", bucket.name);
            }
        }
        BucketCommand::Delete { name, endpoint } => {
            let request = client()?.delete(format!(
                "{}/api/v1/buckets/{name}",
                endpoint.endpoint.trim_end_matches('/')
            ));
            send_admin(request).await?;
            if json {
                print_json(&serde_json::json!({"deleted": name}))?;
            } else {
                println!("deleted {name}");
            }
        }
        BucketCommand::Versioning { command } => bucket_versioning(command, json).await?,
    }
    Ok(())
}

async fn service_account(command: ServiceAccountCommand, json: bool) -> Result<()> {
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
            print_value(&value, json)?;
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
            print_value(&value, true)?;
        }
        ServiceAccountCommand::Revoke { id, endpoint } => {
            let request = client()?.delete(format!(
                "{}/api/v1/service-accounts/{id}",
                endpoint.endpoint.trim_end_matches('/')
            ));
            send_admin(request).await?;
            if json {
                print_json(&serde_json::json!({"deleted": id}))?;
            } else {
                println!("deleted {id}");
            }
        }
        ServiceAccountCommand::Inspect { id, endpoint } => {
            let request = client()?.get(api_url(
                &endpoint,
                &format!("/api/v1/service-accounts/{id}"),
            ));
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode service account")?;
            print_value(&value, json)?;
        }
        ServiceAccountCommand::Enable { id, endpoint } => {
            set_account_status(&endpoint, &id, true, json).await?
        }
        ServiceAccountCommand::Disable { id, endpoint } => {
            set_account_status(&endpoint, &id, false, json).await?
        }
    }
    Ok(())
}

async fn bucket_versioning(command: BucketVersioningCommand, json: bool) -> Result<()> {
    let (name, endpoint, state) = match command {
        BucketVersioningCommand::Get { name, endpoint } => (name, endpoint, None),
        BucketVersioningCommand::Enable { name, endpoint } => (name, endpoint, Some("Enabled")),
        BucketVersioningCommand::Suspend { name, endpoint } => (name, endpoint, Some("Suspended")),
    };
    let url = api_url(&endpoint, &format!("/api/v1/buckets/{name}/versioning"));
    let request = if let Some(state) = state {
        client()?
            .put(url)
            .json(&serde_json::json!({"versioning": state}))
    } else {
        client()?.get(url)
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode versioning response")?;
    print_value(&value, json)
}

async fn set_account_status(
    endpoint: &EndpointArgs,
    id: &str,
    enabled: bool,
    json: bool,
) -> Result<()> {
    let request = client()?
        .put(api_url(
            endpoint,
            &format!("/api/v1/service-accounts/{id}/status"),
        ))
        .json(&serde_json::json!({"enabled": enabled}));
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode account status")?;
    print_value(&value, json)
}

async fn credential(command: CredentialCommand, json: bool) -> Result<()> {
    match command {
        CredentialCommand::Rotate {
            account_id,
            endpoint,
        } => {
            let request = client()?
                .post(api_url(
                    &endpoint,
                    &format!("/api/v1/service-accounts/{account_id}/credentials"),
                ))
                .json(&serde_json::json!({}));
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode rotated credential")?;
            print_value(&value, true)?;
        }
        CredentialCommand::Enable {
            account_id,
            credential_id,
            endpoint,
        } => {
            set_credential_status(&endpoint, &account_id, &credential_id, true, json).await?;
        }
        CredentialCommand::Disable {
            account_id,
            credential_id,
            endpoint,
        } => {
            set_credential_status(&endpoint, &account_id, &credential_id, false, json).await?;
        }
        CredentialCommand::Temporary {
            account_id,
            expires_in_seconds,
            endpoint,
        } => {
            let request = client()?
                .post(api_url(
                    &endpoint,
                    &format!("/api/v1/service-accounts/{account_id}/temporary-credentials"),
                ))
                .json(&serde_json::json!({"expires_in_seconds": expires_in_seconds}));
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode temporary credential")?;
            print_value(&value, true)?;
        }
    }
    Ok(())
}

async fn set_credential_status(
    endpoint: &EndpointArgs,
    account_id: &str,
    credential_id: &str,
    enabled: bool,
    json: bool,
) -> Result<()> {
    let request = client()?
        .put(api_url(
            endpoint,
            &format!("/api/v1/service-accounts/{account_id}/credentials/{credential_id}/status"),
        ))
        .json(&serde_json::json!({"enabled": enabled}));
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode credential status")?;
    print_value(&value, json)
}

async fn policy(command: PolicyCommand, json: bool) -> Result<()> {
    match command {
        PolicyCommand::List(endpoint) => {
            let request = client()?.get(api_url(&endpoint, "/api/v1/policies"));
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode policies")?;
            print_value(&value, json)?;
        }
        PolicyCommand::Create { file, endpoint } => {
            let value: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&file)
                    .with_context(|| format!("read policy file {}", file.display()))?,
            )
            .context("decode policy JSON")?;
            let request = client()?
                .post(api_url(&endpoint, "/api/v1/policies"))
                .json(&value);
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode created policy")?;
            print_value(&value, json)?;
        }
        PolicyCommand::Attach {
            policy_id,
            account_id,
            endpoint,
        } => {
            let request = client()?.put(api_url(
                &endpoint,
                &format!("/api/v1/policies/{policy_id}/bindings/{account_id}"),
            ));
            send_admin(request).await?;
            print_action("attached", &policy_id, json)?;
        }
        PolicyCommand::Detach {
            policy_id,
            account_id,
            endpoint,
        } => {
            let request = client()?.delete(api_url(
                &endpoint,
                &format!("/api/v1/policies/{policy_id}/bindings/{account_id}"),
            ));
            send_admin(request).await?;
            print_action("detached", &policy_id, json)?;
        }
    }
    Ok(())
}

async fn webhook(command: WebhookCommand, json: bool) -> Result<()> {
    let (request, context) = match command {
        WebhookCommand::List(endpoint) => (
            client()?.get(api_url(&endpoint, "/api/v1/webhooks")),
            "decode webhooks",
        ),
        WebhookCommand::Create { file, endpoint } => {
            let value: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&file)
                    .with_context(|| format!("read webhook file {}", file.display()))?,
            )
            .context("decode webhook JSON")?;
            (
                client()?
                    .post(api_url(&endpoint, "/api/v1/webhooks"))
                    .json(&value),
                "decode created webhook",
            )
        }
        WebhookCommand::Deliveries { limit, endpoint } => (
            client()?
                .get(api_url(&endpoint, "/api/v1/webhook-deliveries"))
                .query(&[("limit", limit)]),
            "decode webhook deliveries",
        ),
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context(context)?;
    print_value(&value, json)
}

async fn audit(arguments: AuditArgs, json: bool) -> Result<()> {
    let mut query = vec![("limit", arguments.limit.to_string())];
    if let Some(principal) = arguments.principal {
        query.push(("principal", principal));
    }
    if let Some(operation) = arguments.operation {
        query.push(("operation", operation));
    }
    let request = client()?
        .get(api_url(&arguments.endpoint, "/api/v1/audit/events"))
        .query(&query);
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode audit events")?;
    print_value(&value, json)
}

async fn verify(command: VerifyCommand, json: bool) -> Result<()> {
    let request = match command {
        VerifyCommand::Object {
            bucket,
            key,
            endpoint,
        } => client()?.post(api_url(
            &endpoint,
            &format!("/api/v1/verify/objects/{bucket}/{key}"),
        )),
        VerifyCommand::Bucket { bucket, endpoint } => client()?.post(api_url(
            &endpoint,
            &format!("/api/v1/verify/buckets/{bucket}"),
        )),
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode verification result")?;
    print_value(&value, json)
}

async fn storage(command: StorageCommand, json: bool) -> Result<()> {
    let request = match command {
        StorageCommand::Inspect {
            maximum_entries,
            endpoint,
        } => client()?
            .get(api_url(&endpoint, "/api/v1/storage/inspect"))
            .query(&[("maximum_entries", maximum_entries)]),
        StorageCommand::Repair {
            maximum_entries,
            apply,
            endpoint,
        } => client()?
            .post(api_url(&endpoint, "/api/v1/storage/repair"))
            .json(&serde_json::json!({
                "maximum_entries": maximum_entries,
                "dry_run": !apply,
            })),
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode storage result")?;
    print_value(&value, json)
}

fn api_url(endpoint: &EndpointArgs, path: &str) -> String {
    format!("{}{path}", endpoint.endpoint.trim_end_matches('/'))
}

fn print_value(value: &serde_json::Value, json: bool) -> Result<()> {
    if json {
        print_json(value)
    } else if let Some(array) = value.as_array() {
        for item in array {
            println!("{}", serde_json::to_string(item)?);
        }
        Ok(())
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
        Ok(())
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_action(action: &str, id: &str, json: bool) -> Result<()> {
    if json {
        print_json(&serde_json::json!({"action": action, "id": id}))
    } else {
        println!("{action} {id}");
        Ok(())
    }
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
