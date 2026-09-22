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
    /// Export the audit trail, or report what Object Lock holds.
    AuditExport {
        #[command(subcommand)]
        command: AuditExportCommand,
    },
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
    /// Report whether this machine can run the configured deployment.
    ///
    /// Checks the data directory, its permissions, the filesystem arrangement
    /// that makes payload publication atomic, the on-disk storage format, free
    /// space, the configured addresses, and which key material is present. No
    /// secret value is ever printed.
    Doctor,
    /// Back up a stopped deployment: payloads, metadata, and system records.
    Backup {
        /// Destination directory. Must be empty or absent.
        output: PathBuf,
        /// Replace a destination holding an interrupted backup. A completed
        /// backup is never overwritten.
        #[arg(long)]
        replace_incomplete: bool,
    },
    /// Check a backup without restoring it.
    VerifyBackup {
        /// Backup directory.
        input: PathBuf,
        /// How much to check: `manifest`, `checksums`, or `full`. A
        /// metadata-only check is never reported as a full verification.
        #[arg(long, default_value = "checksums")]
        level: String,
    },
    /// Restore a verified backup into an empty data directory.
    Restore {
        /// Backup directory.
        input: PathBuf,
        /// Verification to run before anything is written.
        #[arg(long, default_value = "checksums")]
        level: String,
    },
    /// Deprecated. Use `backup`, which also covers payloads and system records.
    BackupMetadata { output: PathBuf },
    /// Deprecated. Use `restore`, which also restores payloads.
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
    /// Inspect or change Object Lock on a bucket.
    ///
    /// Object Lock is enabled when a bucket is created and never afterwards,
    /// because enabling it later would claim protection over versions that were
    /// written without it. Create a locked bucket over S3 with
    /// `x-amz-bucket-object-lock-enabled: true`.
    ObjectLock {
        #[command(subcommand)]
        command: BucketObjectLockCommand,
    },
    /// Inspect or change bucket versioning.
    Versioning {
        #[command(subcommand)]
        command: BucketVersioningCommand,
    },
}

#[derive(Subcommand)]
enum BucketObjectLockCommand {
    /// Show a bucket's Object Lock configuration.
    Show {
        /// Bucket name.
        name: String,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Replace the default retention applied to new object versions.
    ///
    /// The default is materialized onto each version as it is written, so
    /// changing it never alters a version that already exists.
    SetDefault {
        /// Bucket name.
        name: String,
        /// Retention mode. COMPLIANCE cannot be shortened or bypassed by
        /// anyone, including the root credential.
        #[arg(long, value_parser = ["GOVERNANCE", "COMPLIANCE"])]
        mode: String,
        /// Retention period in whole days. Mutually exclusive with --years.
        #[arg(long, conflicts_with = "years")]
        days: Option<u16>,
        /// Retention period in whole years, counted as 365 days each.
        #[arg(long)]
        years: Option<u16>,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Show the retention and legal hold on one object version.
    ///
    /// Read-only. Placing or releasing a retention is an S3 action governed by
    /// S3 policy; the management plane deliberately offers no second door to it.
    Status {
        /// Bucket name.
        name: String,
        /// Object key.
        key: String,
        /// Version to inspect. Defaults to the current version.
        #[arg(long)]
        version_id: Option<String>,
        #[command(flatten)]
        endpoint: EndpointArgs,
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
enum AuditExportCommand {
    /// Write a bounded, streamed export of an audit range to a directory.
    ///
    /// The directory holds the records, a manifest naming the range and who
    /// exported it, the covering checkpoint roots, and a SHA256SUMS file over
    /// all three. The export never loads the range into memory, and the server
    /// records that it was made.
    ///
    /// SHA256SUMS establishes that the copy reached you unaltered. It does not
    /// establish that the log was not edited before the copy was taken; only a
    /// checkpoint covering the range does that.
    Export {
        /// Start of the range, RFC 3339. Inclusive.
        #[arg(long)]
        from: String,
        /// End of the range, RFC 3339. Exclusive, so adjacent exports tile.
        #[arg(long)]
        to: String,
        /// Record format.
        #[arg(long, default_value = "json", value_parser = ["json", "csv"])]
        format: String,
        /// Directory to create and write the export into.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Report which buckets have Object Lock and what it currently holds.
    RetentionReport {
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Recompute the audit hash chain and report whether it still verifies.
    ///
    /// This detects a record that was edited, removed, or reordered by anyone
    /// who could not also rewrite every later link — which is what an operator
    /// with database access would have to do. It does not detect an operator
    /// who rewrote the whole log and every hash in it; only an external anchor
    /// over a checkpoint reaches that far.
    ///
    /// A long log is walked in spans: follow `next_from` until it is absent.
    VerifyChain {
        /// Sequence to start from. Zero verifies from the genesis value.
        #[arg(long, default_value_t = 0)]
        from: u64,
        /// Records to examine in this span.
        #[arg(long, default_value_t = 1_000)]
        limit: usize,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
}

#[derive(Subcommand)]
enum VerifyCommand {
    /// Verify an object's stored checksum, and optionally emit a proof bundle.
    Object {
        bucket: String,
        key: String,
        /// Version to describe. Defaults to the current version.
        #[arg(long)]
        version_id: Option<String>,
        /// Write a portable proof bundle to this path.
        ///
        /// The bundle is a signed JSON document describing the object version:
        /// its identity, the SHA-256 recorded when it was written, and the
        /// deployment's public verification key. It contains no credentials,
        /// no capability tokens, and not the object itself, so it is safe to
        /// send to whoever needs to check the file.
        #[arg(long, value_name = "FILE")]
        proof: Option<PathBuf>,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    /// Check a proof bundle against a file, offline.
    ///
    /// Contacts nothing. It recomputes the file's SHA-256, checks the bundle's
    /// signature, and prints every check it performed along with every one it
    /// could not perform, so a passing result never claims more than it
    /// established.
    Proof {
        /// The bundle to check.
        bundle: PathBuf,
        /// The file the bundle should describe.
        #[arg(long, value_name = "FILE")]
        object: PathBuf,
        /// The deployment's published public key, hex.
        ///
        /// Without this the signature is only checked against the key carried
        /// inside the bundle, which establishes that the bundle is internally
        /// consistent but not which deployment produced it.
        #[arg(long, value_name = "HEX")]
        public_key: Option<String>,
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
    /// Report what a stopped member's metadata state holds, changing nothing.
    ///
    /// Run this on every survivor before recovering: the member that has
    /// applied the most is the one to rebuild from, and that cannot be known
    /// without looking.
    InspectState {
        /// TOML configuration file for the stopped member.
        #[arg(long, env = "RECORD_STORE_CONFIG_FILE")]
        config: Option<PathBuf>,
    },
    /// Rebuild metadata authority around this stopped member. Disaster recovery.
    ///
    /// Only for a cluster whose voter majority is permanently lost. It discards
    /// any metadata the lost quorum committed but never replicated here, and it
    /// cannot be undone. Running it on two survivors separately produces two
    /// clusters that can never be reconciled.
    Recover {
        /// Cluster this member belongs to, as `inspect-state` reports it.
        #[arg(long)]
        cluster_id: String,
        /// Why this is being done. Kept in the cluster's own record.
        #[arg(long)]
        reason: String,
        /// Acknowledge that unreplicated committed metadata is lost.
        #[arg(long)]
        accept_data_loss: bool,
        /// TOML configuration file for the stopped member.
        #[arg(long, env = "RECORD_STORE_CONFIG_FILE")]
        config: Option<PathBuf>,
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
    /// Hold every active rebalance without discarding its progress.
    Pause(EndpointArgs),
    /// Return paused rebalances to service.
    Resume(EndpointArgs),
    /// Set the byte-per-second ceiling for one transfer. Zero disables it.
    Throttle {
        /// Bytes per second.
        bytes_per_second: u64,
        #[command(flatten)]
        endpoint: EndpointArgs,
    },
    Status(EndpointArgs),
    Start(EndpointArgs),
}

#[derive(Deserialize)]
struct StatusResponse {
    status: String,
}

/// Exit codes the maintenance commands use.
///
/// A backup script needs to tell "this backup is damaged" from "the disk is
/// full" from "somebody is still running the server", and a single non-zero
/// code cannot say which. These are stable: automation may match on them.
mod exit {
    /// The command did what was asked.
    pub const OK: i32 = 0;
    /// Something unexpected went wrong.
    pub const FAILED: i32 = 1;
    /// The configuration or the arguments were not usable.
    pub const CONFIGURATION: i32 = 2;
    /// The backup cannot be restored, or verification found problems.
    pub const UNUSABLE_BACKUP: i32 = 3;
    /// The destination already holds something this must not overwrite.
    pub const DESTINATION_CONFLICT: i32 = 4;
    /// The destination filesystem cannot hold the copy.
    pub const INSUFFICIENT_SPACE: i32 = 5;
    /// A Record Store process still holds the data directory.
    pub const DATA_DIRECTORY_IN_USE: i32 = 6;
    /// Diagnostic checks failed.
    pub const CHECKS_FAILED: i32 = 7;
}

fn backup_exit_code(error: &record_store_server::backup::BackupError) -> i32 {
    use record_store_server::backup::BackupError::*;

    match error {
        Configuration(_) | NotInitialized(_) => exit::CONFIGURATION,
        DataDirectoryInUse(_) => exit::DATA_DIRECTORY_IN_USE,
        DestinationHoldsBackup(_) | DestinationNotEmpty(_) => exit::DESTINATION_CONFLICT,
        InsufficientSpace { .. } => exit::INSUFFICIENT_SPACE,
        MissingComponent(_) | NoManifest(_) | InvalidManifest | Unusable(_)
        | ChecksumMismatch(_) | UnsafePath(_) | Catalog(_) => exit::UNUSABLE_BACKUP,
        Io(_) | Encoding(_) => exit::FAILED,
    }
}

/// Reports the failure on stderr, and the machine-readable form on stdout when
/// asked, so a JSON consumer always gets JSON.
fn report_backup_error(error: &record_store_server::backup::BackupError, json: bool) -> i32 {
    let code = backup_exit_code(error);
    if json {
        println!(
            "{}",
            serde_json::json!({ "ok": false, "error": error.to_string(), "exit_code": code })
        );
    }
    eprintln!("error: {error}");
    code
}

fn doctor(config: &record_store_config::Config, json: bool) -> i32 {
    let report = record_store_server::preflight::inspect(config, true);
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => {
                eprintln!("error: {error}");
                return exit::FAILED;
            }
        }
    } else {
        for check in &report.checks {
            let marker = match check.status {
                record_store_server::preflight::Status::Pass => "ok  ",
                record_store_server::preflight::Status::Warn => "warn",
                record_store_server::preflight::Status::Fail => "FAIL",
            };
            println!("{marker}  {:<28}  {}", check.name, check.detail);
            if let Some(remedy) = &check.remedy {
                println!("      {:<28}  -> {remedy}", "");
            }
        }
    }
    if report.has_failures() {
        exit::CHECKS_FAILED
    } else {
        exit::OK
    }
}

fn run_backup(
    config: &record_store_config::Config,
    output: &std::path::Path,
    replace_incomplete: bool,
    json: bool,
) -> i32 {
    match record_store_server::backup::backup(config, output, replace_incomplete) {
        Ok(report) => {
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(rendered) => println!("{rendered}"),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return exit::FAILED;
                    }
                }
            } else {
                println!("backup complete: {}", report.destination.display());
                for component in &report.manifest.components {
                    println!(
                        "  {:<10}  {:>8} files  {:>14} bytes",
                        component.name, component.file_count, component.bytes
                    );
                }
                println!("  consistency  {}", report.manifest.consistency);
                println!("  secrets      not included; recover key material separately");
            }
            exit::OK
        }
        Err(error) => report_backup_error(&error, json),
    }
}

fn run_verify_backup(
    input: &std::path::Path,
    level: &str,
    master_key: Option<&[u8]>,
    json: bool,
) -> i32 {
    let Some(level) = record_store_server::backup::VerificationLevel::parse(level) else {
        eprintln!("error: unknown verification level; expected manifest, checksums, or full");
        return exit::CONFIGURATION;
    };
    match record_store_server::backup::verify(input, level, master_key) {
        Ok(report) => {
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(rendered) => println!("{rendered}"),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return exit::FAILED;
                    }
                }
            } else {
                println!("backup   {}", report.backup.display());
                println!("level    {}", report.level);
                println!("usable   {}", if report.usable { "yes" } else { "no" });
                println!(
                    "checked  {} files, {} bytes read",
                    report.files_checksummed, report.bytes_read
                );
                if let Some(references) = report.payload_references_checked {
                    println!(
                        "payloads {references} references, {} missing, {} unreferenced",
                        report.missing_payloads.unwrap_or_default(),
                        report.unreferenced_payloads.unwrap_or_default()
                    );
                }
                println!(
                    "key      {}",
                    match report.encryption_key_matches {
                        Some(true) => "the supplied master key matches these payloads",
                        Some(false) => "the supplied master key does NOT match these payloads",
                        None => "not checked (unencrypted backup, or no key supplied)",
                    }
                );
                for problem in &report.problems {
                    println!("problem  {problem}");
                }
            }
            if report.usable {
                exit::OK
            } else {
                exit::UNUSABLE_BACKUP
            }
        }
        Err(error) => report_backup_error(&error, json),
    }
}

fn run_restore(
    config: &record_store_config::Config,
    input: &std::path::Path,
    level: &str,
    json: bool,
) -> i32 {
    let Some(level) = record_store_server::backup::VerificationLevel::parse(level) else {
        eprintln!("error: unknown verification level; expected manifest, checksums, or full");
        return exit::CONFIGURATION;
    };
    match record_store_server::backup::restore(config, input, level) {
        Ok(report) => {
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(rendered) => println!("{rendered}"),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return exit::FAILED;
                    }
                }
            } else {
                println!(
                    "restored {} into {}",
                    report.source.display(),
                    report.data_directory.display()
                );
                println!("verified at level {}", report.verified_at_level);
                for component in &report.components {
                    println!(
                        "  {:<10}  {:>8} files  {:>14} bytes",
                        component.name, component.file_count, component.bytes
                    );
                }
                if report.cleared_interrupted_restore {
                    println!("  cleared an interrupted earlier restore first");
                }
                for outstanding in &report.outstanding {
                    println!("  note: {outstanding}");
                }
            }
            exit::OK
        }
        Err(error) => report_backup_error(&error, json),
    }
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
            Some(ServerCommand::Doctor) => {
                let config = Config::load(arguments.config.as_deref())
                    .context("load Record Store configuration")?;
                std::process::exit(doctor(&config, json));
            }
            Some(ServerCommand::Backup {
                output,
                replace_incomplete,
            }) => {
                let config = Config::load(arguments.config.as_deref())
                    .context("load Record Store configuration")?;
                std::process::exit(run_backup(&config, &output, replace_incomplete, json));
            }
            Some(ServerCommand::VerifyBackup { input, level }) => {
                // A backup is often checked on a machine that is not the
                // deployment, where no root credentials are set and a full
                // configuration cannot load. That must not stop the check, so a
                // missing configuration only costs the key comparison.
                let master_key =
                    Config::load(arguments.config.as_deref())
                        .ok()
                        .and_then(|config| {
                            config
                                .auth
                                .credential_master_key
                                .as_ref()
                                .map(|key| key.expose().as_bytes().to_vec())
                        });
                std::process::exit(run_verify_backup(
                    &input,
                    &level,
                    master_key.as_deref(),
                    json,
                ));
            }
            Some(ServerCommand::Restore { input, level }) => {
                let config = Config::load(arguments.config.as_deref())
                    .context("load Record Store configuration")?;
                std::process::exit(run_restore(&config, &input, &level, json));
            }
            Some(ServerCommand::BackupMetadata { output }) => {
                let config = Config::load(arguments.config.as_deref())
                    .context("load Record Store configuration")?;
                eprintln!(
                    "warning: backup-metadata copies metadata only. `record-store server backup` \
                     also copies payloads and system records, which a restore needs."
                );
                record_store_server::backup_metadata(&config, &output)
                    .context("back up Record Store metadata")?;
                println!("metadata backup created at {}", output.display());
            }
            Some(ServerCommand::RestoreMetadata { input }) => {
                let config = Config::load(arguments.config.as_deref())
                    .context("load Record Store configuration")?;
                eprintln!(
                    "warning: restore-metadata restores metadata only. `record-store server \
                     restore` also restores payloads and system records."
                );
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
        Command::AuditExport { command } => audit_export(command, json).await?,
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
        BucketCommand::ObjectLock { command } => bucket_object_lock(command, json).await?,
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

async fn bucket_object_lock(command: BucketObjectLockCommand, json: bool) -> Result<()> {
    let request = match command {
        BucketObjectLockCommand::Show { name, endpoint } => client()?.get(api_url(
            &endpoint,
            &format!("/api/v1/buckets/{name}/object-lock"),
        )),
        BucketObjectLockCommand::SetDefault {
            name,
            mode,
            days,
            years,
            endpoint,
        } => {
            // Clap's `conflicts_with` rules out naming both; naming neither is
            // still possible and means no period at all, which is not a rule.
            let period = match (days, years) {
                (Some(days), None) => serde_json::json!({"unit": "days", "value": days}),
                (None, Some(years)) => serde_json::json!({"unit": "years", "value": years}),
                _ => {
                    anyhow::bail!("a default retention needs exactly one of --days or --years");
                }
            };
            client()?
                .put(api_url(
                    &endpoint,
                    &format!("/api/v1/buckets/{name}/object-lock"),
                ))
                .json(&serde_json::json!({
                    "object_lock": {
                        "default_retention": {"mode": mode.to_lowercase(), "period": period}
                    }
                }))
        }
        BucketObjectLockCommand::Status {
            name,
            key,
            version_id,
            endpoint,
        } => {
            let url = api_url(
                &endpoint,
                &format!("/api/v1/buckets/{name}/object-lock/{key}"),
            );
            let request = client()?.get(url);
            match version_id {
                Some(version_id) => request.query(&[("version_id", version_id)]),
                None => request,
            }
        }
    };
    let value = send_admin(request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode object lock response")?;
    print_value(&value, json)
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
        VerifyCommand::Proof {
            bundle,
            object,
            public_key,
        } => return verify_proof(&bundle, &object, public_key.as_deref()).await,
        VerifyCommand::Object {
            bucket,
            key,
            version_id,
            proof,
            endpoint,
        } => {
            if let Some(destination) = proof {
                return write_proof_bundle(
                    &endpoint,
                    &bucket,
                    &key,
                    version_id.as_deref(),
                    &destination,
                    json,
                )
                .await;
            }
            client()?.post(api_url(
                &endpoint,
                &format!("/api/v1/verify/objects/{bucket}/{key}"),
            ))
        }
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

/// Returns the consensus directory of a stopped member.
fn consensus_directory(config: Option<PathBuf>) -> Result<PathBuf> {
    let config = Config::load(config.as_deref()).context("configuration is invalid")?;
    Ok(config
        .storage
        .data_directory
        .join("metadata")
        .join("consensus"))
}

/// Reports what a stopped member holds, without touching it.
async fn inspect_state(config: Option<PathBuf>, json: bool) -> Result<()> {
    let directory = consensus_directory(config)?;
    let assessment = record_store_consensus::recovery::inspect(&directory)
        .await
        .with_context(|| format!("inspect the consensus state in {}", directory.display()))?;
    if json {
        return print_json(&serde_json::to_value(&assessment)?);
    }
    println!("{}", assessment.summary);
    if let Some(cluster) = &assessment.cluster {
        println!("Cluster:            {}", cluster.cluster_id);
        println!("Recovery generation: {}", cluster.recovery_generation);
    }
    match assessment.last_applied {
        Some(index) => println!("Applied index:      {index}"),
        None => println!("Applied index:      none"),
    }
    println!("Unapplied entries:  {}", assessment.log_entries);
    println!(
        "Recorded members:   {}",
        assessment
            .members
            .iter()
            .map(|(id, address)| format!("{id} ({address})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    match &assessment.snapshot {
        record_store_consensus::SnapshotHealth::Absent => println!("Snapshot:           none"),
        record_store_consensus::SnapshotHealth::Present { index } => println!(
            "Snapshot:           present{}",
            index.map_or_else(String::new, |index| format!(" at index {index}"))
        ),
        record_store_consensus::SnapshotHealth::Damaged { reason } => {
            println!("Snapshot:           DAMAGED — {reason}");
        }
    }
    if !assessment.recoverable {
        println!("\nThis member cannot be recovered from.");
    }
    Ok(())
}

/// Rebuilds metadata authority around one stopped member.
async fn recover_cluster(
    config: Option<PathBuf>,
    cluster_id: &str,
    reason: String,
    accept_data_loss: bool,
    json: bool,
) -> Result<()> {
    let directory = consensus_directory(config.clone())?;
    let cluster_id = record_store_core::ClusterId::from_uuid(
        cluster_id
            .parse()
            .context("--cluster-id must be the identifier `cluster inspect-state` reports")?,
    );
    let loaded = Config::load(config.as_deref()).context("configuration is invalid")?;
    let identity =
        record_store_cluster::NodeIdentityStore::new(&loaded.storage.data_directory).load()?;
    let member_id = identity
        .and_then(|identity| identity.raft_id)
        .context("this data directory has no consensus member identifier")?;
    let address = loaded.server.effective_rpc_advertise();

    let report = record_store_consensus::recovery::recover_single_member(
        &directory,
        record_store_consensus::RecoveryIntent {
            cluster_id,
            member_id,
            address,
            reason,
            accept_data_loss,
        },
    )
    .await
    .context("rebuild metadata authority")?;

    if json {
        return print_json(&serde_json::to_value(&report)?);
    }
    println!(
        "Rebuilt metadata authority for cluster {}",
        report.cluster_id
    );
    println!("  member:               {}", report.member_id);
    println!("  recovery generation:  {}", report.recovery_generation);
    println!("  recovery id:          {}", report.recovery_id);
    println!(
        "  voters removed:       {}",
        report
            .removed_voters
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  log entries discarded: {}", report.discarded_log_entries);
    println!(
        "  payloads:             {} total, {} readable here, {} needing another holder",
        report.payloads_total, report.payloads_held_here, report.payloads_elsewhere
    );
    if !report.fully_readable() {
        println!(
            "\n{} payload(s) have no replica on this member. They stay unreadable until one of \
             their other holders returns, or must be restored from an external backup.",
            report.payloads_elsewhere
        );
    }
    println!(
        "\nStart this member, confirm it elects, then re-admit the other nodes with fresh join \
         tokens. Do not run this command on another survivor."
    );
    Ok(())
}

async fn cluster(command: ClusterCommand, json: bool) -> Result<()> {
    match command {
        ClusterCommand::InspectState { config } => return inspect_state(config, json).await,
        ClusterCommand::Recover {
            cluster_id,
            reason,
            accept_data_loss,
            config,
        } => {
            return recover_cluster(config, &cluster_id, reason, accept_data_loss, json).await;
        }
        _ => {}
    }
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
        ClusterCommand::InspectState { .. } | ClusterCommand::Recover { .. } => {
            unreachable!("handled above; these never reach a management API")
        }
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
        RebalanceCommand::Pause(endpoint) => {
            client()?.post(api_url(&endpoint, "/api/v1/rebalance/pause"))
        }
        RebalanceCommand::Resume(endpoint) => {
            client()?.post(api_url(&endpoint, "/api/v1/rebalance/resume"))
        }
        RebalanceCommand::Throttle {
            bytes_per_second,
            endpoint,
        } => client()?
            .post(api_url(&endpoint, "/api/v1/rebalance/throttle"))
            .json(&serde_json::json!({ "bytes_per_second": bytes_per_second })),
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
    let retry = builder.try_clone();
    let response = admin_request(builder)?
        .send()
        .await
        .context("send management request")?;
    if response.status().is_success() {
        return Ok(response);
    }
    // Some operations are planned by whichever member holds metadata
    // leadership, and any other member answers with where to go. The redirect is
    // followed here rather than by the HTTP client because credentials must be
    // re-applied: a client library drops them across hosts, correctly.
    //
    // Exactly one hop. Leadership can move again while this is in flight, and a
    // client that chased every redirect would turn an election into a loop.
    if response.status() == reqwest::StatusCode::TEMPORARY_REDIRECT
        && let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
        && let Some(retry) = retry
    {
        let mut request = admin_request(retry)?
            .build()
            .context("rebuild the request for the metadata leader")?;
        // Only the destination changes: the method, headers, and body are the
        // ones the caller meant, which is what a 307 promises to preserve.
        *request.url_mut() = location.parse().with_context(|| {
            format!("the leader redirect named an unusable address: {location}")
        })?;
        let response = client()?
            .execute(request)
            .await
            .with_context(|| format!("follow redirect to the metadata leader at {location}"))?;
        return if response.status().is_success() {
            Ok(response)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("the metadata leader at {location} returned HTTP {status}: {body}")
        };
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    bail!("management API returned HTTP {status}: {body}")
}

/// Fetches a signed proof bundle and writes it to a file.
///
/// The bundle is produced and signed by the server, which is what holds the
/// deployment master key. Nothing secret reaches this process.
async fn write_proof_bundle(
    endpoint: &EndpointArgs,
    bucket: &str,
    key: &str,
    version_id: Option<&str>,
    destination: &std::path::Path,
    json: bool,
) -> Result<()> {
    let url = api_url(endpoint, &format!("/api/v1/buckets/{bucket}/proof/{key}"));
    let request = client()?.get(url);
    let request = match version_id {
        Some(version_id) => request.query(&[("version_id", version_id)]),
        None => request,
    };
    let bundle = send_admin(request)
        .await?
        .json::<record_store_proof::ProofBundle>()
        .await
        .context("decode proof bundle")?;
    let document = serde_json::to_string_pretty(&bundle).context("encode proof bundle")?;
    tokio::fs::write(destination, document.as_bytes())
        .await
        .with_context(|| format!("write {}", destination.display()))?;
    if json {
        print_value(
            &serde_json::json!({
                "proof": destination.display().to_string(),
                "key_id": bundle.deployment.key_id,
                "version_id": bundle.object.version_id,
            }),
            true,
        )?;
    } else {
        println!("wrote proof bundle to {}", destination.display());
        println!(
            "  object     {}/{}",
            bundle.object.bucket, bundle.object.key
        );
        println!("  version    {}", bundle.object.version_id);
        println!("  sha256     {}", bundle.payload.sha256);
        println!("  signed by  {}", bundle.deployment.key_id);
        println!(
            "\nCheck it anywhere with:\n  record-store verify proof {} --object <file>",
            destination.display()
        );
    }
    Ok(())
}

/// Checks a proof bundle against a file without contacting anything.
async fn verify_proof(
    bundle_path: &std::path::Path,
    object_path: &std::path::Path,
    public_key: Option<&str>,
) -> Result<()> {
    let document = tokio::fs::read(bundle_path)
        .await
        .with_context(|| format!("read {}", bundle_path.display()))?;
    let bundle: record_store_proof::ProofBundle =
        serde_json::from_slice(&document).context("parse proof bundle")?;
    let expected = public_key
        .map(|value| hex::decode(value.trim()).context("decode --public-key as hex"))
        .transpose()?;
    let verdict = record_store_proof::verify_bundle(&bundle, object_path, expected.as_deref())
        .await
        .context("verify proof bundle")?;

    println!(
        "{}/{} version {}",
        bundle.object.bucket, bundle.object.key, bundle.object.version_id
    );
    for check in &verdict.checks {
        println!(
            "  [{}] {}: {}",
            check.status.marker(),
            check.name,
            check.detail
        );
    }
    let unproved = verdict.unproved();
    if verdict.is_verified() {
        println!("\nVERIFIED: every check that could be performed passed.");
        if !unproved.is_empty() {
            // Naming the gaps beside the verdict is the point. A reader who
            // stops at the word "verified" must still see what it did not cover.
            println!("This does NOT establish:");
            for check in unproved {
                println!("  - {}", check.name);
            }
        }
    } else {
        println!("\nFAILED: at least one check did not pass.");
        // A non-zero exit so a script cannot mistake failure for success.
        std::process::exit(1);
    }
    Ok(())
}

/// Runs the auditor-facing export commands.
async fn audit_export(command: AuditExportCommand, json: bool) -> Result<()> {
    match command {
        AuditExportCommand::RetentionReport { endpoint } => {
            let request = client()?.get(api_url(&endpoint, "/api/v1/reports/retention"));
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode retention report")?;
            print_value(&value, json)
        }
        AuditExportCommand::VerifyChain {
            from,
            limit,
            endpoint,
        } => {
            let request = client()?
                .get(api_url(&endpoint, "/api/v1/audit/chain"))
                .query(&[("from", from.to_string()), ("limit", limit.to_string())]);
            let value = send_admin(request)
                .await?
                .json::<serde_json::Value>()
                .await
                .context("decode audit chain verification")?;
            print_value(&value, json)
        }
        AuditExportCommand::Export {
            from,
            to,
            format,
            out,
            endpoint,
        } => write_audit_export(&endpoint, &from, &to, &format, &out, json).await,
    }
}

/// Writes an export directory: records, manifest, checkpoints and SHA256SUMS.
///
/// The records are streamed to disk and hashed as they pass, so an export of a
/// year costs one buffer rather than a year of memory, and SHA256SUMS needs no
/// second read of the file.
async fn write_audit_export(
    endpoint: &EndpointArgs,
    from: &str,
    to: &str,
    format: &str,
    out: &std::path::Path,
    json: bool,
) -> Result<()> {
    use sha2::{Digest, Sha256};

    // The manifest is fetched first: it is what records the export in the audit
    // trail, so a refused export never streams a single record.
    let manifest_request = client()?
        .get(api_url(endpoint, "/api/v1/audit/export/manifest"))
        .query(&[("from", from), ("to", to), ("format", format)]);
    let manifest = send_admin(manifest_request)
        .await?
        .json::<serde_json::Value>()
        .await
        .context("decode export manifest")?;
    let record_file = manifest["record_file"]
        .as_str()
        .context("manifest names no record file")?
        .to_owned();

    tokio::fs::create_dir_all(out)
        .await
        .with_context(|| format!("create {}", out.display()))?;

    let manifest_bytes = serde_json::to_vec_pretty(&manifest).context("encode manifest")?;
    let checkpoint_bytes =
        serde_json::to_vec_pretty(&manifest["checkpoints"]).context("encode checkpoints")?;
    tokio::fs::write(out.join("manifest.json"), &manifest_bytes)
        .await
        .context("write manifest.json")?;
    tokio::fs::write(out.join("checkpoints.json"), &checkpoint_bytes)
        .await
        .context("write checkpoints.json")?;

    let records_request = client()?
        .get(api_url(endpoint, "/api/v1/audit/export"))
        .query(&[("from", from), ("to", to), ("format", format)]);
    let mut response = send_admin(records_request).await?;
    let records_path = out.join(&record_file);
    let mut file = tokio::fs::File::create(&records_path)
        .await
        .with_context(|| format!("create {}", records_path.display()))?;
    let mut hasher = Sha256::new();
    let mut bytes_written = 0_u64;
    while let Some(chunk) = response.chunk().await.context("read export stream")? {
        hasher.update(&chunk);
        bytes_written += chunk.len() as u64;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .context("write export records")?;
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .context("flush export records")?;
    let records_digest = hex::encode(hasher.finalize());

    // sha256sum(1) format, so an auditor can check it with the tool they have
    // rather than one this project ships.
    let sums = format!(
        "{records_digest}  {record_file}\n{}  manifest.json\n{}  checkpoints.json\n",
        hex::encode(Sha256::digest(&manifest_bytes)),
        hex::encode(Sha256::digest(&checkpoint_bytes)),
    );
    tokio::fs::write(out.join("SHA256SUMS"), sums.as_bytes())
        .await
        .context("write SHA256SUMS")?;

    if json {
        print_value(
            &serde_json::json!({
                "directory": out.display().to_string(),
                "export_id": manifest["export_id"],
                "record_file": record_file,
                "bytes": bytes_written,
                "sha256": records_digest,
                "checkpoints": manifest["checkpoints"]["status"],
            }),
            true,
        )
    } else {
        println!("wrote audit export to {}", out.display());
        println!(
            "  export id   {}",
            manifest["export_id"].as_str().unwrap_or("?")
        );
        println!(
            "  exported by {}",
            manifest["exported_by"].as_str().unwrap_or("?")
        );
        println!("  records     {record_file} ({bytes_written} bytes)");
        println!("  sha256      {records_digest}");
        if manifest["checkpoints"]["status"] == "unavailable" {
            println!(
                "\nNo checkpoint covers this range. SHA256SUMS shows this copy reached you\n\
                 unaltered; it does not show the log was unedited before the copy was taken."
            );
        }
        println!(
            "\nCheck the copy with:\n  cd {} && sha256sum -c SHA256SUMS",
            out.display()
        );
        Ok(())
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
                Command::AuditExport { .. } => "audit-export",
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

    /// The maintenance commands are what a backup script drives, so their
    /// arguments and defaults are pinned here rather than discovered at 3am.
    #[test]
    fn the_maintenance_commands_parse_with_safe_defaults() {
        let Command::Server(doctor) = parse(&["record-store", "server", "doctor"]).command else {
            panic!("expected the server command");
        };
        assert!(matches!(doctor.command, Some(ServerCommand::Doctor)));

        let Command::Server(backup) =
            parse(&["record-store", "server", "backup", "/backups/today"]).command
        else {
            panic!("expected the server command");
        };
        let Some(ServerCommand::Backup {
            output,
            replace_incomplete,
        }) = backup.command
        else {
            panic!("expected the backup subcommand");
        };
        assert_eq!(output, *std::path::Path::new("/backups/today"));
        assert!(
            !replace_incomplete,
            "replacing an earlier attempt has to be asked for explicitly"
        );

        let Command::Server(verify) =
            parse(&["record-store", "server", "verify-backup", "/backups/today"]).command
        else {
            panic!("expected the server command");
        };
        let Some(ServerCommand::VerifyBackup { level, .. }) = verify.command else {
            panic!("expected the verify-backup subcommand");
        };
        assert_eq!(
            level, "checksums",
            "the default must read the bytes, not just the manifest"
        );

        let Command::Server(restore) =
            parse(&["record-store", "server", "restore", "/backups/today"]).command
        else {
            panic!("expected the server command");
        };
        let Some(ServerCommand::Restore { level, .. }) = restore.command else {
            panic!("expected the restore subcommand");
        };
        assert_eq!(level, "checksums");
    }

    /// An unknown level has to be refused rather than quietly downgraded to the
    /// cheapest check.
    #[test]
    fn an_unrecognized_verification_level_is_not_silently_accepted() {
        assert!(record_store_server::backup::VerificationLevel::parse("thorough").is_none());
        for level in ["manifest", "checksums", "full"] {
            assert_eq!(
                record_store_server::backup::VerificationLevel::parse(level)
                    .expect("a documented level")
                    .as_str(),
                level,
                "a level must report itself by the name it was asked for"
            );
        }
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
