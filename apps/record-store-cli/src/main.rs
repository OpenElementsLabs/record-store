use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use record_store_config::{Config, DeploymentMode, SecretValue};
use record_store_core::Bucket;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(
    name = "record-store",
    version,
    about = "Operational CLI for Record Store"
)]
struct Cli {
    /// Emit JSON suitable for automation.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the Record Store CLI version.
    Version,
    /// Start or validate the Record Store server.
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
    /// Inspect or explicitly repair Record Store-owned storage state.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Initialize and inspect a distributed cluster.
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
    /// Inspect or change cluster node lifecycle state.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Explain placement decisions.
    Placement {
        #[command(subcommand)]
        command: PlacementCommand,
    },
    /// Inspect and define storage classes.
    StorageClass {
        #[command(subcommand)]
        command: StorageClassCommand,
    },
    /// Inspect or change storage device lifecycle state.
    Drive {
        #[command(subcommand)]
        command: DriveCommand,
    },
    /// Inspect the durable repair queue.
    Repair {
        #[command(subcommand)]
        command: RepairCommand,
    },
    /// Inspect or trigger safe replica rebalancing.
    Rebalance {
        #[command(subcommand)]
        command: RebalanceCommand,
    },
}

#[derive(Args)]
struct ServerArgs {
    /// TOML configuration file. Environment variables override file values.
    #[arg(long, env = "RECORD_STORE_CONFIG_FILE")]
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
    /// Record Store native management endpoint.
    #[arg(long, default_value = "http://127.0.0.1:7601")]
    endpoint: String,
}

/// Arguments naming one device on one node.
#[derive(Args)]
struct DeviceArgs {
    /// Stable node identifier.
    node: String,
    /// Stable device identifier. This is not the device's current path, which
    /// can change across reboots.
    device: String,
    #[command(flatten)]
    endpoint: EndpointArgs,
}

#[derive(Subcommand)]
enum PlacementCommand {
    /// Predict what a topology change would move, without changing anything.
    Simulate {
        #[command(subcommand)]
        command: SimulateCommand,
    },
    /// Explain where an object is, or would be, placed.
    Explain {
        /// Bucket name.
        bucket: String,
        /// Object key.
        key: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum SimulateCommand {
    /// Adding a node with the given device capacities, in bytes.
    AddNode {
        /// Usable bytes for each device the node would bring. Repeatable.
        #[arg(long = "device-bytes", required = true)]
        device_bytes: Vec<u64>,
        /// Failure-domain labels, for example `rack=b`.
        #[arg(long, default_value = "")]
        failure_domain: String,
        /// Storage class its devices would belong to.
        #[arg(long)]
        storage_class: Option<String>,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Adding one device to a node already in the cluster.
    AddDevice {
        /// Node that would gain the device.
        node: String,
        /// Usable bytes it would contribute.
        #[arg(long)]
        usable_bytes: u64,
        /// Storage class it would belong to.
        #[arg(long)]
        storage_class: Option<String>,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Removing a device, as a drain or a failure would.
    RemoveDevice {
        /// Node holding the device.
        node: String,
        /// Device that would go away.
        device: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum StorageClassCommand {
    /// List defined storage classes.
    List(EndpointArgs),
    /// Inspect one storage class.
    Show {
        /// Class name.
        class: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Define or replace a storage class.
    Set {
        /// Class name.
        class: String,
        /// Copies to keep. Omitted leaves the cluster replication factor.
        #[arg(long)]
        replicas: Option<u8>,
        /// Topology level replicas must be separated across.
        #[arg(long, value_parser = ["device", "node", "host", "rack", "datacenter", "zone", "region"])]
        failure_domain: Option<String>,
        /// Refuse placement that cannot satisfy the failure domain.
        #[arg(long)]
        strict: bool,
        /// Device kinds this class may use. Repeatable; omitted accepts any.
        #[arg(long = "device-kind")]
        device_kinds: Vec<String>,
        /// Percentage of each device's usable capacity to keep free.
        #[arg(long)]
        minimum_free_percent: Option<u8>,
        /// Human-facing description.
        #[arg(long)]
        description: Option<String>,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Remove a storage class.
    Delete {
        /// Class name.
        class: String,
        /// Skip the confirmation prompt, for automation.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum DriveCommand {
    /// List every registered device in the cluster.
    List(EndpointArgs),
    /// List storage this node could use, without registering any of it.
    ///
    /// Discovery never formats, mounts, or claims anything. Add what you want
    /// to a `[[storage.devices]]` entry and restart the node.
    Discover(EndpointArgs),
    /// Inspect one registered device.
    Show(DeviceArgs),
    /// Bring a registered device into service.
    Activate(DeviceArgs),
    /// Stop new placement and move this device's replicas elsewhere.
    Drain(DeviceArgs),
    /// Pause a device without evacuating it.
    Maintenance(DeviceArgs),
    /// Return a drained or maintained device to service.
    Resume(DeviceArgs),
    /// Mark an evacuated device safe to remove.
    ///
    /// Refused while the device still owns replicas, so success means
    /// evacuation genuinely finished.
    Release(DeviceArgs),
    /// Permanently retire a device.
    Retire {
        #[command(flatten)]
        device: DeviceArgs,
        /// Skip the confirmation prompt, for automation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum BucketCommand {
    /// List buckets.
    List(EndpointArgs),
    /// Create a bucket.
    Create {
        /// Storage class new objects are placed on.
        #[arg(long)]
        storage_class: Option<String>,
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

#[derive(Subcommand)]
enum ClusterCommand {
    /// Idempotently initialize or report the configured cluster.
    Init(EndpointArgs),
    /// Show cluster, quorum, capacity, and replication health.
    Status(EndpointArgs),
    /// Issue a short-lived, single-use node join token.
    IssueJoinToken {
        #[arg(long, default_value_t = 3_600)]
        lifetime_seconds: u64,
        #[arg(long, default_value = "record-store node join")]
        description: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum NodeCommand {
    /// Join through an existing member and start this storage node.
    Join {
        /// Existing member's internal RPC address (normally host:7603).
        #[arg(long)]
        control: String,
        /// Short-lived token issued by `record-store cluster issue-join-token`.
        #[arg(long)]
        token: String,
        /// TOML configuration file for this node.
        #[arg(long, env = "RECORD_STORE_CONFIG_FILE")]
        config: Option<PathBuf>,
    },
    /// List registered nodes.
    List(EndpointArgs),
    /// Inspect one stable node identity.
    Inspect {
        id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Stop new placement and move replicas away from a node.
    Drain {
        id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Retain replicas but exclude a node from new placement.
    Maintenance {
        id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Return a drained or maintained node to service.
    Resume {
        id: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Permanently remove a node after durability checks.
    Decommission {
        id: String,
        /// Explicitly acknowledge durability loss when the safety check fails.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum RepairCommand {
    Status(EndpointArgs),
}

#[derive(Subcommand)]
enum RebalanceCommand {
    Status(EndpointArgs),
    Start(EndpointArgs),
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
        Command::Version => println!("record-store {}", env!("CARGO_PKG_VERSION")),
        Command::Server(arguments) => match arguments.command {
            Some(ServerCommand::CheckConfig) => {
                Config::load(arguments.config.as_deref()).context("configuration is invalid")?;
                println!("configuration is valid");
            }
            Some(ServerCommand::BackupMetadata { output }) => {
                let config = Config::load(arguments.config.as_deref())
                    .context("load Record Store configuration")?;
                record_store_server::backup_metadata(&config, &output)
                    .context("back up Record Store metadata")?;
                println!("metadata backup created at {}", output.display());
            }
            Some(ServerCommand::RestoreMetadata { input }) => {
                let config = Config::load(arguments.config.as_deref())
                    .context("load Record Store configuration")?;
                record_store_server::restore_metadata(&config, &input)
                    .context("restore Record Store metadata")?;
                println!("metadata restored from {}", input.display());
            }
            None => {
                let config = Config::load(arguments.config.as_deref())
                    .context("load Record Store configuration")?;
                record_store_observability::init(&config.observability)
                    .context("initialize observability")?;
                record_store_server::run(&config, record_store_server::shutdown_signal())
                    .await
                    .context("run Record Store server")?;
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
        Command::Cluster { command } => cluster(command, json).await?,
        Command::Node { command } => node(command, json).await?,
        Command::Drive { command } => drive(command, json).await?,
        Command::StorageClass { command } => storage_class(command, json).await?,
        Command::Placement { command } => placement(command, json).await?,
        Command::Repair { command } => repair(command, json).await?,
        Command::Rebalance { command } => rebalance(command, json).await?,
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
    if let Ok(token) = env::var("RECORD_STORE_MANAGEMENT_TOKEN") {
        return Ok(builder.bearer_auth(token));
    }
    let access = env::var("RECORD_STORE_ROOT_ACCESS_KEY")
        .context("RECORD_STORE_ROOT_ACCESS_KEY is required")?;
    let secret = env::var("RECORD_STORE_ROOT_SECRET_KEY")
        .context("RECORD_STORE_ROOT_SECRET_KEY is required")?;
    Ok(builder.basic_auth(access, Some(secret)))
}

async fn status(endpoint: &str) -> Result<()> {
    let endpoint = endpoint.trim_end_matches('/');
    let ready_response = client()?
        .get(format!("{endpoint}/ready"))
        .send()
        .await
        .with_context(|| format!("connect to {endpoint}"))?;
    if !ready_response.status().is_success() {
        bail!("server is not ready (HTTP {})", ready_response.status());
    }
    let ready: StatusResponse = ready_response
        .json()
        .await
        .context("decode readiness response")?;
    if ready.status != "ready" {
        bail!("server returned unexpected readiness status");
    }
    // System information is part of the authenticated management plane, so the
    // credential has to be attached. It stays optional: a container healthcheck
    // runs this command with no token, and readiness above is what it asks for.
    let info = match send_admin(client()?.get(format!("{endpoint}/api/v1/system/info"))).await {
        Ok(response) => response
            .json::<serde_json::Value>()
            .await
            .context("decode system info response")?,
        Err(_) => serde_json::Value::Null,
    };
    println!("Ready              yes");
    println!("Management API     {endpoint}");
    if let Some(mode) = info.get("mode") {
        println!("Mode               {}", display_json_scalar(mode));
    }
    if let Some(cluster_id) = info.get("cluster_id") {
        println!("Cluster ID         {}", display_json_scalar(cluster_id));
    }
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
        BucketCommand::Create {
            name,
            storage_class,
            endpoint,
        } => {
            let mut body = serde_json::json!({ "name": &name });
            if let Some(class) = storage_class {
                body["storage_class"] = serde_json::Value::String(class);
            }
            let request = client()?
                .post(format!(
                    "{}/api/v1/buckets",
                    endpoint.endpoint.trim_end_matches('/')
                ))
                .json(&body);
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
        // The wire form is the serialized `VersioningState`, which is snake case.
        BucketVersioningCommand::Enable { name, endpoint } => (name, endpoint, Some("enabled")),
        BucketVersioningCommand::Suspend { name, endpoint } => (name, endpoint, Some("suspended")),
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

async fn cluster(command: ClusterCommand, json: bool) -> Result<()> {
    let request = match command {
        ClusterCommand::Init(endpoint) => {
            client()?.post(api_url(&endpoint, "/api/v1/cluster/init"))
        }
        ClusterCommand::Status(endpoint) => client()?.get(api_url(&endpoint, "/api/v1/cluster")),
        ClusterCommand::IssueJoinToken {
            lifetime_seconds,
            description,
            endpoint,
        } => client()?
            .post(api_url(&endpoint, "/api/v1/cluster/join-tokens"))
            .json(&serde_json::json!({
                "lifetime_seconds": lifetime_seconds,
                "description": description,
            })),
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode cluster response")?;
    if json {
        return print_json(&value);
    }
    if let Some(cluster_id) = value.get("cluster_id") {
        println!("Cluster ID: {}", display_json_scalar(cluster_id));
        if let Some(health) = value.get("health") {
            println!("Health: {}", display_json_scalar(health));
        }
        if let Some(nodes) = value.get("nodes").and_then(serde_json::Value::as_array) {
            println!("Nodes: {}", nodes.len());
        }
        if let Some(replication) = value.get("replication") {
            println!("Replication: {}", serde_json::to_string(replication)?);
        }
        if let Some(repair) = value.get("repair") {
            println!("Repair: {}", serde_json::to_string(repair)?);
        }
        return Ok(());
    }
    print_value(&value, false)
}

async fn node(command: NodeCommand, json: bool) -> Result<()> {
    let command = match command {
        NodeCommand::Join {
            control,
            token,
            config,
        } => {
            let mut config =
                Config::load(config.as_deref()).context("load Record Store configuration")?;
            config.server.mode = DeploymentMode::Cluster;
            config.cluster.seeds = vec![control];
            config.cluster.join_token = Some(SecretValue::new(token));
            config
                .validate()
                .context("validate joined-node configuration")?;
            record_store_observability::init(&config.observability)
                .context("initialize observability")?;
            return record_store_server::run(&config, record_store_server::shutdown_signal())
                .await
                .context("run joined Record Store node");
        }
        other => other,
    };
    let (request, no_content_action) = match command {
        NodeCommand::Join { .. } => bail!("internal join dispatch error"),
        NodeCommand::List(endpoint) => (client()?.get(api_url(&endpoint, "/api/v1/nodes")), None),
        NodeCommand::Inspect { id, endpoint } => (
            client()?.get(api_url(&endpoint, &format!("/api/v1/nodes/{id}"))),
            None,
        ),
        NodeCommand::Drain { id, endpoint } => (
            client()?.post(api_url(&endpoint, &format!("/api/v1/nodes/{id}/drain"))),
            None,
        ),
        NodeCommand::Maintenance { id, endpoint } => (
            client()?.post(api_url(
                &endpoint,
                &format!("/api/v1/nodes/{id}/maintenance"),
            )),
            Some(("maintenance", id)),
        ),
        NodeCommand::Resume { id, endpoint } => (
            client()?.post(api_url(&endpoint, &format!("/api/v1/nodes/{id}/resume"))),
            Some(("resumed", id)),
        ),
        NodeCommand::Decommission {
            id,
            force,
            endpoint,
        } => (
            client()?
                .post(api_url(
                    &endpoint,
                    &format!("/api/v1/nodes/{id}/decommission"),
                ))
                .json(&serde_json::json!({"force": force})),
            None,
        ),
    };
    let response = send_admin(request).await?;
    if let Some((action, id)) = no_content_action {
        return print_action(action, &id, json);
    }
    let value = response
        .json::<serde_json::Value>()
        .await
        .context("decode node response")?;
    print_value(&value, json)
}

async fn placement(command: PlacementCommand, json: bool) -> Result<()> {
    let request = match command {
        PlacementCommand::Simulate { command } => {
            let (endpoint, body) = match command {
                SimulateCommand::AddNode {
                    device_bytes,
                    failure_domain,
                    storage_class,
                    endpoint,
                } => (
                    endpoint,
                    serde_json::json!({
                        "change": "add_node",
                        "devices": device_bytes,
                        "failure_domain": failure_domain,
                        "storage_class": storage_class,
                    }),
                ),
                SimulateCommand::AddDevice {
                    node,
                    usable_bytes,
                    storage_class,
                    endpoint,
                } => (
                    endpoint,
                    serde_json::json!({
                        "change": "add_device",
                        "node_id": node,
                        "usable_bytes": usable_bytes,
                        "storage_class": storage_class,
                    }),
                ),
                SimulateCommand::RemoveDevice {
                    node,
                    device,
                    endpoint,
                } => (
                    endpoint,
                    serde_json::json!({
                        "change": "remove_device",
                        "node_id": node,
                        "device_id": device,
                    }),
                ),
            };
            client()?
                .post(api_url(&endpoint, "/api/v1/placement/simulate"))
                .json(&body)
        }
        PlacementCommand::Explain {
            bucket,
            key,
            endpoint,
        } => explain_request(&bucket, &key, &endpoint)?,
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode placement response")?;
    print_value(&value, json)
}

fn explain_request(
    bucket: &str,
    key: &str,
    endpoint: &EndpointArgs,
) -> Result<reqwest::RequestBuilder> {
    Ok(client()?.get(api_url(
        endpoint,
        &format!("/api/v1/placement/explain/{bucket}/{key}"),
    )))
}

async fn storage_class(command: StorageClassCommand, json: bool) -> Result<()> {
    let (request, no_content_action) = match command {
        StorageClassCommand::List(endpoint) => (
            client()?.get(api_url(&endpoint, "/api/v1/storage-classes")),
            None,
        ),
        StorageClassCommand::Show { class, endpoint } => (
            client()?.get(api_url(
                &endpoint,
                &format!("/api/v1/storage-classes/{class}"),
            )),
            None,
        ),
        StorageClassCommand::Set {
            class,
            replicas,
            failure_domain,
            strict,
            device_kinds,
            minimum_free_percent,
            description,
            endpoint,
        } => {
            // The class is sent in the body as well as the path because the body
            // is the durable record; the server refuses a mismatch rather than
            // guessing which one the operator meant.
            let mut policy = serde_json::json!({
                "class": class,
                "durability": {
                    "strategy": "replication",
                    "replicas": replicas.unwrap_or(3),
                },
                "failure_domain": failure_domain.unwrap_or_else(|| "node".to_owned()),
                "strict_failure_domains": strict,
                "minimum_free_space_percent": minimum_free_percent.unwrap_or(0),
            });
            if !device_kinds.is_empty() {
                policy["device_filter"] = serde_json::json!({ "allowed_kinds": device_kinds });
            }
            if let Some(description) = description {
                policy["description"] = serde_json::Value::String(description);
            }
            (
                client()?
                    .put(api_url(
                        &endpoint,
                        &format!("/api/v1/storage-classes/{class}"),
                    ))
                    .json(&policy),
                None,
            )
        }
        StorageClassCommand::Delete {
            class,
            yes,
            endpoint,
        } => {
            confirm(yes, &format!("Remove storage class {class}?"))?;
            (
                client()?.delete(api_url(
                    &endpoint,
                    &format!("/api/v1/storage-classes/{class}"),
                )),
                Some(("removed", class)),
            )
        }
    };
    let response = send_admin(request).await?;
    if let Some((action, class)) = no_content_action {
        return print_action(action, &class, json);
    }
    let value = response
        .json::<serde_json::Value>()
        .await
        .context("decode storage class response")?;
    print_value(&value, json)
}

async fn drive(command: DriveCommand, json: bool) -> Result<()> {
    let request = match command {
        DriveCommand::List(endpoint) => client()?.get(api_url(&endpoint, "/api/v1/devices")),
        DriveCommand::Discover(endpoint) => {
            client()?.get(api_url(&endpoint, "/api/v1/devices/discovered"))
        }
        DriveCommand::Show(device) => client()?.get(device_url(&device, "")),
        DriveCommand::Activate(device) => client()?.post(device_url(&device, "/activate")),
        DriveCommand::Drain(device) => client()?.post(device_url(&device, "/drain")),
        DriveCommand::Maintenance(device) => client()?.post(device_url(&device, "/maintenance")),
        DriveCommand::Resume(device) => client()?.post(device_url(&device, "/resume")),
        DriveCommand::Release(device) => client()?.post(device_url(&device, "/release")),
        DriveCommand::Retire { device, yes } => {
            // Retiring is the one device command that cannot be walked back, so
            // it asks before acting unless a script opted out.
            confirm(
                yes,
                &format!(
                    "Permanently retire device {} on node {}?",
                    device.device, device.node
                ),
            )?;
            client()?.post(device_url(&device, "/retire"))
        }
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode device response")?;
    print_value(&value, json)
}

fn device_url(device: &DeviceArgs, action: &str) -> String {
    api_url(
        &device.endpoint,
        &format!(
            "/api/v1/nodes/{}/devices/{}{action}",
            device.node, device.device
        ),
    )
}

/// Requires an interactive confirmation before a destructive action.
///
/// A non-interactive session must pass `--yes` explicitly: prompting into a pipe
/// would either hang or silently read nothing, and neither should be mistaken
/// for consent.
fn confirm(assumed: bool, question: &str) -> Result<()> {
    use std::io::{IsTerminal, Write};

    if assumed {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("{question} Refusing without --yes because this is not an interactive terminal");
    }
    print!("{question} [y/N] ");
    std::io::stdout()
        .flush()
        .context("prompt for confirmation")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("read confirmation")?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
        bail!("cancelled");
    }
    Ok(())
}

async fn repair(command: RepairCommand, json: bool) -> Result<()> {
    let RepairCommand::Status(endpoint) = command;
    let value = send_admin(client()?.get(api_url(&endpoint, "/api/v1/repair/status")))
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode repair status")?;
    print_value(&value, json)
}

async fn rebalance(command: RebalanceCommand, json: bool) -> Result<()> {
    let request = match command {
        RebalanceCommand::Status(endpoint) => {
            client()?.get(api_url(&endpoint, "/api/v1/rebalance/status"))
        }
        RebalanceCommand::Start(endpoint) => {
            client()?.post(api_url(&endpoint, "/api/v1/rebalance"))
        }
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode rebalance response")?;
    print_value(&value, json)
}

fn display_json_scalar(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(arguments: &[&str]) -> Cli {
        Cli::try_parse_from(arguments).expect("arguments must parse")
    }

    fn endpoint_of(arguments: &[&str]) -> String {
        match parse(arguments).command {
            Command::Status(endpoint) => endpoint.endpoint,
            other => panic!("expected a status command, got {:?}", DebugCommand(&other)),
        }
    }

    /// Renders just enough of a command to make a failing assertion legible;
    /// the command tree itself deliberately does not derive `Debug`.
    struct DebugCommand<'a>(&'a Command);

    impl std::fmt::Debug for DebugCommand<'_> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let name = match self.0 {
                Command::Version => "version",
                Command::Server(_) => "server",
                Command::Status(_) => "status",
                Command::Bucket { .. } => "bucket",
                Command::ServiceAccount { .. } => "service-account",
                Command::Credential { .. } => "credential",
                Command::Policy { .. } => "policy",
                Command::Webhook { .. } => "webhook",
                Command::Audit(_) => "audit",
                Command::Verify { .. } => "verify",
                Command::Storage { .. } => "storage",
                Command::Cluster { .. } => "cluster",
                Command::Node { .. } => "node",
                Command::Drive { .. } => "drive",
                Command::StorageClass { .. } => "storage-class",
                Command::Placement { .. } => "placement",
                Command::Repair { .. } => "repair",
                Command::Rebalance { .. } => "rebalance",
            };
            formatter.write_str(name)
        }
    }

    /// Clap can only detect a contradictory definition at runtime. Without this
    /// assertion, a duplicated flag or a bad default reaches an operator as a
    /// panic on first use instead of failing the build.
    #[test]
    fn the_command_tree_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// The binary takes no positional subcommand of its own beyond the listed
    /// ones. Silently accepting an unknown word would run the wrong thing.
    #[test]
    fn an_unknown_subcommand_is_refused() {
        assert!(Cli::try_parse_from(["record-store", "serve"]).is_err());
        assert!(Cli::try_parse_from(["record-store", "server", "start"]).is_err());
    }

    #[test]
    fn the_server_command_runs_with_no_arguments_at_all() {
        let Command::Server(arguments) = parse(["record-store", "server"].as_slice()).command
        else {
            panic!("expected the server command");
        };
        assert!(
            arguments.command.is_none(),
            "bare `server` must not select a subcommand"
        );
    }

    #[test]
    fn server_subcommands_carry_their_paths() {
        let Command::Server(arguments) = parse(&[
            "record-store",
            "server",
            "--config",
            "/etc/record-store.toml",
            "check-config",
        ])
        .command
        else {
            panic!("expected the server command");
        };
        assert_eq!(
            arguments.config.as_deref(),
            Some(std::path::Path::new("/etc/record-store.toml"))
        );
        assert!(matches!(
            arguments.command,
            Some(ServerCommand::CheckConfig)
        ));

        let Command::Server(backup) = parse(&[
            "record-store",
            "server",
            "backup-metadata",
            "/backups/today",
        ])
        .command
        else {
            panic!("expected the server command");
        };
        assert!(matches!(
            backup.command,
            Some(ServerCommand::BackupMetadata { output }) if output == *std::path::Path::new("/backups/today")
        ));
    }

    /// The default endpoint is part of the operator contract: running a command
    /// with no `--endpoint` must reach a local server's management port.
    #[test]
    fn commands_default_to_the_local_management_endpoint() {
        assert_eq!(
            endpoint_of(&["record-store", "status"]),
            "http://127.0.0.1:7601"
        );
        assert_eq!(
            endpoint_of(&[
                "record-store",
                "status",
                "--endpoint",
                "https://store.example"
            ]),
            "https://store.example"
        );
    }

    /// `--json` is global, so it has to be accepted on either side of the
    /// subcommand. Automation writes it both ways.
    #[test]
    fn the_json_flag_is_accepted_before_or_after_the_subcommand() {
        assert!(parse(&["record-store", "--json", "status"]).json);
        assert!(parse(&["record-store", "status", "--json"]).json);
        assert!(!parse(&["record-store", "status"]).json);
    }

    #[test]
    fn bucket_commands_bind_their_name_and_endpoint() {
        let Command::Bucket { command } = parse(&[
            "record-store",
            "bucket",
            "create",
            "photos",
            "--endpoint",
            "http://node-a:7601",
        ])
        .command
        else {
            panic!("expected a bucket command");
        };
        let BucketCommand::Create {
            name,
            storage_class,
            endpoint,
        } = command
        else {
            panic!("expected bucket create");
        };
        assert_eq!(name, "photos");
        assert_eq!(endpoint.endpoint, "http://node-a:7601");
        assert_eq!(
            storage_class, None,
            "a bucket created without --storage-class must not be pinned to one"
        );
    }

    #[test]
    fn bucket_versioning_is_a_three_state_switch() {
        for (argument, expected) in [("get", "get"), ("enable", "enable"), ("suspend", "suspend")] {
            let Command::Bucket { command } =
                parse(&["record-store", "bucket", "versioning", argument, "photos"]).command
            else {
                panic!("expected a bucket command");
            };
            let BucketCommand::Versioning { command } = command else {
                panic!("expected bucket versioning");
            };
            let actual = match command {
                BucketVersioningCommand::Get { .. } => "get",
                BucketVersioningCommand::Enable { .. } => "enable",
                BucketVersioningCommand::Suspend { .. } => "suspend",
            };
            assert_eq!(actual, expected);
        }
    }

    /// Decommissioning can destroy durability, so the override must be an
    /// explicit flag that defaults to off.
    #[test]
    fn decommissioning_a_node_requires_an_explicit_force_flag() {
        let Command::Node { command } =
            parse(&["record-store", "node", "decommission", "node-1"]).command
        else {
            panic!("expected a node command");
        };
        assert!(
            matches!(command, NodeCommand::Decommission { force, .. } if !force),
            "force must default to off"
        );

        let Command::Node { command } =
            parse(&["record-store", "node", "decommission", "node-1", "--force"]).command
        else {
            panic!("expected a node command");
        };
        assert!(matches!(command, NodeCommand::Decommission { force, .. } if force));
    }

    /// Joining a cluster is the one command where both values are mandatory:
    /// without them a node would silently start standalone.
    #[test]
    fn joining_a_cluster_requires_both_a_control_address_and_a_token() {
        assert!(Cli::try_parse_from(["record-store", "node", "join"]).is_err());
        assert!(
            Cli::try_parse_from(["record-store", "node", "join", "--control", "node-a:7603"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["record-store", "node", "join", "--token", "abc"]).is_err());

        let Command::Node { command } = parse(&[
            "record-store",
            "node",
            "join",
            "--control",
            "node-a:7603",
            "--token",
            "join-token",
        ])
        .command
        else {
            panic!("expected a node command");
        };
        assert!(matches!(
            command,
            NodeCommand::Join { control, token, .. } if control == "node-a:7603" && token == "join-token"
        ));
    }

    #[test]
    fn audit_queries_default_to_a_bounded_page() {
        let Command::Audit(arguments) = parse(&["record-store", "audit"]).command else {
            panic!("expected an audit command");
        };
        assert_eq!(arguments.limit, 100);
        assert!(arguments.principal.is_none());
        assert!(arguments.operation.is_none());

        let Command::Audit(filtered) = parse(&[
            "record-store",
            "audit",
            "--limit",
            "5",
            "--principal",
            "root",
            "--operation",
            "DeleteBucket",
        ])
        .command
        else {
            panic!("expected an audit command");
        };
        assert_eq!(filtered.limit, 5);
        assert_eq!(filtered.principal.as_deref(), Some("root"));
        assert_eq!(filtered.operation.as_deref(), Some("DeleteBucket"));
    }

    #[test]
    fn a_non_numeric_limit_is_refused_rather_than_silently_defaulted() {
        assert!(Cli::try_parse_from(["record-store", "audit", "--limit", "many"]).is_err());
    }

    /// A trailing slash on an endpoint is the most common operator typo. It has
    /// to collapse, or every request would be sent to a doubled path.
    #[test]
    fn endpoint_paths_are_joined_without_doubling_the_separator() {
        let trailing = EndpointArgs {
            endpoint: "http://127.0.0.1:7601/".to_owned(),
        };
        let bare = EndpointArgs {
            endpoint: "http://127.0.0.1:7601".to_owned(),
        };
        assert_eq!(
            api_url(&trailing, "/api/v1/buckets"),
            "http://127.0.0.1:7601/api/v1/buckets"
        );
        assert_eq!(
            api_url(&bare, "/api/v1/buckets"),
            "http://127.0.0.1:7601/api/v1/buckets"
        );
        assert_eq!(
            api_url(&trailing, "/api/v1/buckets"),
            api_url(&bare, "/api/v1/buckets")
        );
    }

    #[test]
    fn repeated_trailing_slashes_all_collapse() {
        let endpoint = EndpointArgs {
            endpoint: "http://127.0.0.1:7601///".to_owned(),
        };
        assert_eq!(api_url(&endpoint, "/ready"), "http://127.0.0.1:7601/ready");
    }

    /// Operator output should read as plain text, not as a quoted JSON string,
    /// while non-string values keep a faithful JSON rendering.
    #[test]
    fn scalars_render_for_humans_without_gaining_quotes() {
        assert_eq!(
            display_json_scalar(&serde_json::json!("standalone")),
            "standalone"
        );
        assert_eq!(display_json_scalar(&serde_json::json!(7)), "7");
        assert_eq!(display_json_scalar(&serde_json::json!(true)), "true");
        assert_eq!(display_json_scalar(&serde_json::Value::Null), "null");
        assert_eq!(
            display_json_scalar(&serde_json::json!({"a": 1})),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn a_missing_readiness_field_is_a_decode_failure_not_a_default() {
        assert!(serde_json::from_value::<StatusResponse>(serde_json::json!({})).is_err());
        let parsed: StatusResponse =
            serde_json::from_value(serde_json::json!({"status": "ready"})).expect("decode");
        assert_eq!(parsed.status, "ready");
    }

    #[test]
    fn a_name_request_serialises_to_the_field_the_api_expects() {
        let body = serde_json::to_value(NameRequest { name: "photos" }).expect("serialise");
        assert_eq!(body, serde_json::json!({"name": "photos"}));
    }
}
