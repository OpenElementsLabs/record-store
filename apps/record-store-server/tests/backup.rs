//! What a backup is worth, checked by taking one and restoring it.
//!
//! These tests exist because the properties that matter here cannot be checked
//! by reading the code. A backup is only a backup if a deployment built from it
//! serves the same bytes, keeps the same version history, still honours the
//! same retention, and still knows the same service accounts. And a backup that
//! was interrupted, damaged, or taken without the key that unlocks it has to
//! fail loudly rather than restore into something that looks fine until the
//! first read.
//!
//! Each test drives a real server over its management API, stops it, and works
//! on the durable state it left behind — the same sequence an operator follows.

use std::net::SocketAddr;

use record_store_config::{Config, SecretValue};
use record_store_server::backup::{self, VerificationLevel};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tokio::{net::TcpListener, sync::oneshot};

const ADMIN: &str = "test-system-management-token-32-bytes-long";
const MASTER_KEY: &str = "test-credential-master-key-at-least-32-bytes";
/// The service account every drill creates before the backup and looks for
/// after the restore.
const DRILL_ACCOUNT: &str = "restore-drill";

/// A running deployment, and the handle that stops it.
struct Deployment {
    address: SocketAddr,
    client: Client,
    shutdown: Option<oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<Result<(), record_store_server::StartupError>>,
}

impl Deployment {
    async fn start(config: &Config) -> Self {
        let runtime = record_store_server::initialize(config)
            .await
            .expect("initialize server");
        let api_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind management listener");
        let address = api_listener.local_addr().expect("listener address");
        let s3_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind S3 listener");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(runtime.serve(s3_listener, api_listener, async move {
            let _ = shutdown_rx.await;
        }));
        Self {
            address,
            client: Client::new(),
            shutdown: Some(shutdown_tx),
            server,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn create_bucket(&self, body: Value) {
        let created = self
            .client
            .post(self.url("/api/v1/buckets"))
            .bearer_auth(ADMIN)
            .json(&body)
            .send()
            .await
            .expect("create bucket");
        assert_eq!(created.status(), StatusCode::CREATED, "creating a bucket");
    }

    async fn set_versioning(&self, bucket: &str, versioning: &str) {
        let response = self
            .client
            .put(self.url(&format!("/api/v1/buckets/{bucket}/versioning")))
            .bearer_auth(ADMIN)
            .json(&json!({ "versioning": versioning }))
            .send()
            .await
            .expect("set versioning");
        assert_eq!(response.status(), StatusCode::OK, "enabling versioning");
    }

    async fn upload(&self, bucket: &str, key: &str, body: &[u8]) -> Value {
        let response = self
            .client
            .put(self.url(&format!("/api/v1/buckets/{bucket}/object/{key}")))
            .bearer_auth(ADMIN)
            .header("content-type", "text/plain")
            .body(body.to_vec())
            .send()
            .await
            .expect("upload object");
        assert_eq!(response.status(), StatusCode::CREATED, "uploading {key}");
        response.json().await.expect("object JSON")
    }

    async fn download(&self, bucket: &str, key: &str) -> Vec<u8> {
        let response = self
            .client
            .get(self.url(&format!("/api/v1/buckets/{bucket}/object-content/{key}")))
            .bearer_auth(ADMIN)
            .send()
            .await
            .expect("download object");
        assert_eq!(response.status(), StatusCode::OK, "downloading {key}");
        response.bytes().await.expect("object bytes").to_vec()
    }

    async fn get_json(&self, path: &str) -> Value {
        let response = self
            .client
            .get(self.url(path))
            .bearer_auth(ADMIN)
            .send()
            .await
            .expect("management request");
        assert_eq!(response.status(), StatusCode::OK, "requesting {path}");
        response.json().await.expect("response JSON")
    }

    async fn create_share(&self, bucket: &str, key: &str) -> Value {
        let response = self
            .client
            .post(self.url(&format!("/api/v1/buckets/{bucket}/object-shares/{key}")))
            .bearer_auth(ADMIN)
            .json(&json!({ "label": "restore drill" }))
            .send()
            .await
            .expect("create share");
        assert_eq!(response.status(), StatusCode::CREATED, "creating a share");
        response.json().await.expect("share JSON")
    }

    /// Creates a service account, and returns nothing.
    ///
    /// The creation response carries the account's one-time secret access key.
    /// No test here needs anything out of it — the account is identified by the
    /// name passed in, which is a constant in this file — so the response is
    /// dropped rather than returned. A value that never leaves this function
    /// cannot reach a panic message or a CI log.
    async fn create_service_account(&self, name: &str) {
        let response = self
            .client
            .post(self.url("/api/v1/service-accounts"))
            .bearer_auth(ADMIN)
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("create service account");
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "creating a service account"
        );
    }

    /// Whether an account with this name survives in the deployment.
    ///
    /// Returns a plain answer rather than the listing: entries carry credential
    /// records, and a listing interpolated into a failed assertion is how
    /// credential material ends up in CI output.
    async fn has_service_account(&self, name: &str) -> bool {
        self.get_json("/api/v1/service-accounts")
            .await
            .as_array()
            .expect("account list")
            .iter()
            .filter_map(|entry| entry["account"]["name"].as_str())
            .any(|listed| listed == name)
    }

    /// How many service accounts the deployment knows about.
    async fn service_account_count(&self) -> usize {
        self.get_json("/api/v1/service-accounts")
            .await
            .as_array()
            .expect("account list")
            .len()
    }

    async fn create_lifecycle_rule(&self, bucket: &str, prefix: &str, days: u32) {
        let response = self
            .client
            .post(self.url(&format!("/api/v1/buckets/{bucket}/lifecycle")))
            .bearer_auth(ADMIN)
            .json(&json!({ "prefix": prefix, "expiration": days }))
            .send()
            .await
            .expect("create lifecycle rule");
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "creating a lifecycle rule"
        );
    }

    /// Fetches a share by its public token, the way a recipient would.
    async fn open_share(&self, token: &str) -> StatusCode {
        self.client
            .get(self.url(&format!("/s/{token}")))
            .send()
            .await
            .expect("share request")
            .status()
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.server.await;
    }
}

fn config_for(data_directory: std::path::PathBuf) -> Config {
    let mut config = Config::default();
    config.storage.data_directory = data_directory;
    config.server.shutdown_grace_period_seconds = 2;
    config.auth.root_access_key = Some("test-access".into());
    config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
    config.auth.credential_master_key = Some(SecretValue::new(MASTER_KEY));
    config.auth.management_system_token = Some(SecretValue::new(ADMIN));
    config
}

/// Builds a deployment with something of every kind worth losing in it.
async fn populated_deployment(directory: &TempDir, encrypted: bool) -> (Config, String) {
    let mut config = config_for(directory.path().join("source"));
    config.storage.encryption_enabled = encrypted;
    let deployment = Deployment::start(&config).await;

    deployment.create_bucket(json!({ "name": "records" })).await;
    deployment.set_versioning("records", "enabled").await;
    deployment
        .upload("records", "notes.txt", b"first revision\n")
        .await;
    deployment
        .upload("records", "notes.txt", b"second revision\n")
        .await;
    deployment
        .upload("records", "deep/nested/report.txt", b"a nested object\n")
        .await;

    deployment
        .create_lifecycle_rule("records", "deep/", 90)
        .await;
    let share = deployment.create_share("records", "notes.txt").await;
    deployment.create_service_account(DRILL_ACCOUNT).await;
    let token = share["url"]
        .as_str()
        .expect("share URL")
        .rsplit('/')
        .next()
        .expect("share token")
        .to_owned();

    deployment.stop().await;
    (config, token)
}

/// The acceptance test for the whole feature: everything that mattered before
/// the backup still matters after the restore, on a data directory that started
/// empty.
#[tokio::test]
async fn a_restored_deployment_serves_the_same_objects_versions_shares_and_accounts() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, share_token) = populated_deployment(&directory, false).await;

    let backup_directory = directory.path().join("backup");
    let report = backup::backup(&source_config, &backup_directory, false).expect("take a backup");
    assert_eq!(report.manifest.consistency, "offline-exclusive-lock");
    assert!(
        !report.manifest.secrets_included,
        "a backup must never carry secrets by default"
    );
    assert!(
        report
            .manifest
            .components
            .iter()
            .any(|component| component.name == "objects" && component.file_count >= 3),
        "payloads belong in the backup: {:?}",
        report.manifest.components
    );

    let verification = backup::verify(&backup_directory, VerificationLevel::Full, None)
        .expect("verify the backup");
    assert!(verification.usable, "{:?}", verification.problems);
    assert_eq!(verification.missing_payloads, Some(0));

    // A genuinely clean destination, as a replacement machine would be.
    let restored_config = config_for(directory.path().join("restored"));
    let restore = backup::restore(&restored_config, &backup_directory, VerificationLevel::Full)
        .expect("restore the backup");
    assert_eq!(restore.verified_at_level, "full");
    assert!(!restore.cleared_interrupted_restore);

    let restored = Deployment::start(&restored_config).await;

    assert_eq!(
        restored.download("records", "notes.txt").await,
        b"second revision\n",
        "the current object must come back byte for byte"
    );
    assert_eq!(
        restored.download("records", "deep/nested/report.txt").await,
        b"a nested object\n",
        "a nested key must survive the round trip"
    );

    let versions = restored
        .get_json("/api/v1/buckets/records/object-versions?prefix=notes.txt&limit=100")
        .await;
    let versions = versions["versions"].as_array().expect("version list");
    assert!(
        versions.len() >= 2,
        "history is part of the deployment: {versions:?}"
    );

    assert!(
        restored.has_service_account(DRILL_ACCOUNT).await,
        "the service account that existed before the backup is absent after the restore"
    );

    assert_eq!(
        restored.open_share(&share_token).await,
        StatusCode::OK,
        "a share link issued before the backup must still open after the restore"
    );

    let audit = restored.get_json("/api/v1/audit/events?limit=100").await;
    assert!(
        !audit["events"].as_array().expect("audit events").is_empty(),
        "the audit trail is part of what is restored"
    );

    restored.stop().await;
}

/// An interrupted backup has to be unusable, not merely incomplete. Before the
/// marker existed, a run that died mid-copy left a directory that looked like a
/// backup and failed only when somebody tried to restore it.
#[tokio::test]
async fn an_interrupted_backup_is_refused_rather_than_restored() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");

    // Exactly the state an interrupted run leaves: bytes on disk, marker still
    // present, manifest already written or not.
    std::fs::write(
        backup_directory.join(backup::INCOMPLETE_MARKER),
        b"interrupted",
    )
    .expect("simulate an interruption");

    let verification = backup::verify(&backup_directory, VerificationLevel::Checksums, None)
        .expect("verification runs");
    assert!(!verification.usable);
    assert!(
        verification
            .problems
            .iter()
            .any(|problem| problem.contains("INCOMPLETE")),
        "the operator has to be told why: {:?}",
        verification.problems
    );

    let restored_config = config_for(directory.path().join("restored"));
    let error = backup::restore(
        &restored_config,
        &backup_directory,
        VerificationLevel::Checksums,
    )
    .expect_err("an interrupted backup must not restore");
    assert!(error.to_string().contains("INCOMPLETE"), "{error}");

    // Nothing may have been written into the destination.
    assert!(
        !restored_config
            .storage
            .data_directory
            .join("metadata")
            .exists(),
        "a refused restore must not leave state behind"
    );
}

/// A retry after an interruption must not force the operator to clean up by
/// hand, but must still never touch a complete backup.
#[tokio::test]
async fn a_backup_never_overwrites_a_complete_one_and_retries_over_an_incomplete_one() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");

    let error = backup::backup(&source_config, &backup_directory, false)
        .expect_err("a complete backup is never overwritten");
    assert!(
        error
            .to_string()
            .contains("already holds a completed backup"),
        "{error}"
    );

    let error = backup::backup(&source_config, &backup_directory, true)
        .expect_err("not even with the replace flag");
    assert!(
        error
            .to_string()
            .contains("already holds a completed backup"),
        "{error}"
    );

    // Now make it look interrupted, which is the case the flag is for.
    std::fs::write(
        backup_directory.join(backup::INCOMPLETE_MARKER),
        b"interrupted",
    )
    .expect("simulate an interruption");
    assert!(
        backup::backup(&source_config, &backup_directory, false).is_err(),
        "even an incomplete destination is not replaced silently"
    );
    let report = backup::backup(&source_config, &backup_directory, true)
        .expect("an interrupted attempt may be replaced on request");
    assert!(report.manifest.total_bytes > 0);
    assert!(
        backup::verify(&backup_directory, VerificationLevel::Full, None)
            .expect("verify")
            .usable
    );
}

/// A backup missing a component a restore cannot do without used to restore
/// without complaint, producing a deployment with no service accounts at all.
#[tokio::test]
async fn a_backup_missing_a_required_component_is_refused() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");

    for missing in [
        "metadata/credentials.redb",
        "metadata/catalog.redb",
        "system/storage-format.json",
    ] {
        let damaged = directory
            .path()
            .join(format!("without-{}", missing.replace('/', "-")));
        copy_tree(&backup_directory, &damaged);
        std::fs::remove_file(damaged.join(missing)).expect("drop a component");
        remove_from_manifest(&damaged, missing);

        let verification =
            backup::verify(&damaged, VerificationLevel::Manifest, None).expect("verification runs");
        assert!(
            !verification.usable,
            "a backup without {missing} must not be called usable"
        );
        assert!(
            verification
                .problems
                .iter()
                .any(|problem| problem.contains(missing)),
            "the missing component has to be named: {:?}",
            verification.problems
        );

        let restored_config =
            config_for(directory.path().join(format!("restored-{}", missing.len())));
        assert!(
            backup::restore(&restored_config, &damaged, VerificationLevel::Manifest).is_err(),
            "a backup without {missing} must not restore"
        );
    }
}

/// Damage has to be caught by the checksum pass, and named precisely enough
/// that an operator knows which file to distrust.
#[tokio::test]
async fn modified_and_truncated_components_fail_verification() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");

    let modified = directory.path().join("modified");
    copy_tree(&backup_directory, &modified);
    flip_a_byte(&modified.join("metadata").join("catalog.redb"));
    let verification =
        backup::verify(&modified, VerificationLevel::Checksums, None).expect("verification runs");
    assert!(!verification.usable);
    assert!(
        verification
            .problems
            .iter()
            .any(|problem| problem.contains("catalog.redb") && problem.contains("checksum")),
        "{:?}",
        verification.problems
    );
    // The manifest level must not claim to have caught this, because it did not
    // read the bytes.
    let shallow =
        backup::verify(&modified, VerificationLevel::Manifest, None).expect("verification runs");
    assert!(
        shallow.usable,
        "a structural check cannot detect a content change, and must not pretend to"
    );
    assert_eq!(shallow.files_checksummed, 0);

    let truncated = directory.path().join("truncated");
    copy_tree(&backup_directory, &truncated);
    let victim = truncated.join("metadata").join("audit.redb");
    let shortened = std::fs::read(&victim).expect("read")[..64].to_vec();
    std::fs::write(&victim, shortened).expect("truncate a component");
    let verification =
        backup::verify(&truncated, VerificationLevel::Checksums, None).expect("verification runs");
    assert!(!verification.usable);
    assert!(
        verification
            .problems
            .iter()
            .any(|problem| problem.contains("audit.redb") && problem.contains("bytes")),
        "a truncation must be reported as a size difference: {:?}",
        verification.problems
    );
}

/// A backup written by a future release must be refused with an upgrade
/// instruction rather than opened and misread.
#[tokio::test]
async fn an_incompatible_backup_is_refused_with_an_upgrade_instruction() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    let report = backup::backup(&source_config, &backup_directory, false).expect("take a backup");
    // The manifest names the schema the copied catalog is at, read from it.
    assert_eq!(
        report.manifest.metadata_schema_version,
        record_store_metadata::METADATA_SCHEMA_VERSION
    );

    for (field, value) in [
        ("backup_format_version", json!(99)),
        ("metadata_schema_version", json!(9_999)),
        ("storage_format_version", json!(99)),
    ] {
        let future = directory.path().join(format!("future-{field}"));
        copy_tree(&backup_directory, &future);
        edit_manifest(&future, |manifest| {
            manifest[field] = value.clone();
        });
        let verification =
            backup::verify(&future, VerificationLevel::Manifest, None).expect("verification runs");
        assert!(!verification.usable, "{field} must be refused");
        assert!(
            verification
                .problems
                .iter()
                .any(|problem| problem.contains("upgrade Record Store")),
            "the corrective action is an upgrade: {:?}",
            verification.problems
        );
    }
}

/// Encrypted payloads without the right key are unrecoverable, and the only
/// useful moment to say so is before the restore.
#[tokio::test]
async fn an_encrypted_backup_checks_the_key_without_ever_exposing_it() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, true).await;
    let backup_directory = directory.path().join("backup");
    let report = backup::backup(&source_config, &backup_directory, false).expect("take a backup");
    assert!(
        report.manifest.objects_encrypted,
        "the manifest has to record that these payloads need a key"
    );

    let without_key = backup::verify(&backup_directory, VerificationLevel::Checksums, None)
        .expect("verification runs");
    assert!(!without_key.usable, "no key means no restore");
    assert_eq!(without_key.encryption_key_matches, None);

    let wrong_key = backup::verify(
        &backup_directory,
        VerificationLevel::Checksums,
        Some(b"a-different-master-key-also-32-bytes-long"),
    )
    .expect("verification runs");
    assert_eq!(wrong_key.encryption_key_matches, Some(false));
    assert!(!wrong_key.usable);

    let right_key = backup::verify(
        &backup_directory,
        VerificationLevel::Full,
        Some(MASTER_KEY.as_bytes()),
    )
    .expect("verification runs");
    assert_eq!(right_key.encryption_key_matches, Some(true));
    assert!(right_key.usable, "{:?}", right_key.problems);

    // Nothing in the report may carry the key itself.
    let rendered = serde_json::to_string(&right_key).expect("the report serializes");
    assert!(
        !rendered.contains(MASTER_KEY),
        "a verification report must never repeat the key"
    );

    // A restore configured with the wrong key stops before writing anything.
    let mut wrong = config_for(directory.path().join("wrong-key"));
    wrong.auth.credential_master_key = Some(SecretValue::new(
        "a-different-master-key-also-32-bytes-long",
    ));
    wrong.storage.encryption_enabled = true;
    let error = backup::restore(&wrong, &backup_directory, VerificationLevel::Checksums)
        .expect_err("the wrong key must not restore");
    assert!(error.to_string().contains("master key"), "{error}");
    assert!(!wrong.storage.data_directory.join("objects").exists());

    // And with the right key the payloads really do come back readable.
    let mut right = config_for(directory.path().join("right-key"));
    right.storage.encryption_enabled = true;
    backup::restore(&right, &backup_directory, VerificationLevel::Full).expect("restore");
    let restored = Deployment::start(&right).await;
    assert_eq!(
        restored.download("records", "notes.txt").await,
        b"second revision\n",
        "an encrypted payload must decrypt after a restore with the same key"
    );
    restored.stop().await;
}

/// Credentials, share links and webhook secrets are sealed under the master key
/// whether or not payloads are encrypted. Restoring them under another key
/// produces a deployment that starts and then cannot authenticate its own
/// service accounts, so the restore refuses before it writes anything.
#[tokio::test]
async fn a_plaintext_restore_under_the_wrong_master_key_is_refused_before_writing() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");
    let manifest =
        std::fs::read_to_string(backup_directory.join(backup::MANIFEST_NAME)).expect("manifest");
    assert!(
        !manifest.contains(MASTER_KEY),
        "the reference is not the key"
    );

    let mut wrong = config_for(directory.path().join("wrong-key"));
    wrong.auth.credential_master_key = Some(SecretValue::new(
        "a-different-master-key-also-32-bytes-long",
    ));
    let error = backup::restore(&wrong, &backup_directory, VerificationLevel::Checksums)
        .expect_err("the wrong key must not restore");
    assert!(error.to_string().contains("master key"), "{error}");
    assert!(
        !error.to_string().contains(MASTER_KEY),
        "the refusal names no key"
    );
    for component in ["metadata", "objects", "system"] {
        assert!(
            !wrong.storage.data_directory.join(component).exists(),
            "{component} was written before the refusal"
        );
    }

    let right = config_for(directory.path().join("right-key"));
    backup::restore(&right, &backup_directory, VerificationLevel::Checksums).expect("restore");
    let restored = Deployment::start(&right).await;
    assert!(restored.has_service_account(DRILL_ACCOUNT).await);
    restored.stop().await;
}

/// A restore that dies partway must not leave something that starts and serves
/// half a deployment, and the retry must not need an operator to tidy up first.
#[tokio::test]
async fn an_interrupted_restore_blocks_start_up_and_retries_cleanly() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");

    let restored_config = config_for(directory.path().join("restored"));
    std::fs::create_dir_all(&restored_config.storage.data_directory)
        .expect("create data directory");

    // Exactly what an interruption between the renames leaves behind: the
    // marker, and some but not all of the components.
    std::fs::write(
        restored_config
            .storage
            .data_directory
            .join(backup::RESTORE_MARKER),
        b"interrupted",
    )
    .expect("simulate an interrupted restore");
    std::fs::create_dir_all(restored_config.storage.data_directory.join("metadata"))
        .expect("half-restored metadata");
    std::fs::write(
        restored_config
            .storage
            .data_directory
            .join("metadata")
            .join("catalog.redb"),
        b"partial",
    )
    .expect("a partial component");
    std::fs::create_dir_all(restored_config.storage.data_directory.join("system"))
        .expect("half-restored system records");
    std::fs::copy(
        source_config
            .storage
            .data_directory
            .join("system")
            .join("storage-format.json"),
        restored_config
            .storage
            .data_directory
            .join("system")
            .join("storage-format.json"),
    )
    .expect("a restored storage format record");

    assert!(
        backup::restore_in_progress(&restored_config.storage.data_directory),
        "the marker is what makes the state recognizable"
    );
    // Nor may it be backed up: the copy would look complete and hold a
    // deployment that never existed.
    let half_backup = directory.path().join("half-backup");
    let Err(error) = backup::backup(&restored_config, &half_backup, false) else {
        panic!("a half-restored data directory must not be backed up");
    };
    assert!(
        matches!(error, backup::BackupError::RestoreInProgress(_)),
        "{error}"
    );
    assert!(!half_backup.join(backup::MANIFEST_NAME).exists());
    let Err(error) = record_store_server::initialize(&restored_config).await else {
        panic!("a half-restored deployment must not start");
    };
    assert!(
        error.to_string().contains("restore"),
        "the refusal has to explain itself: {error}"
    );

    // The retry clears what the interrupted attempt left and succeeds.
    let report = backup::restore(
        &restored_config,
        &backup_directory,
        VerificationLevel::Checksums,
    )
    .expect("a retry after an interruption must work without manual cleanup");
    assert!(report.cleared_interrupted_restore);
    assert!(!backup::restore_in_progress(
        &restored_config.storage.data_directory
    ));

    let restored = Deployment::start(&restored_config).await;
    assert_eq!(
        restored.download("records", "notes.txt").await,
        b"second revision\n"
    );
    restored.stop().await;
}

/// The destination of a restore is never merged into, because merging two
/// deployments produces a catalog that disagrees with the payloads beside it.
#[tokio::test]
async fn a_restore_refuses_to_touch_a_populated_data_directory() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");

    // The source deployment itself is the most dangerous possible destination.
    let error = backup::restore(
        &source_config,
        &backup_directory,
        VerificationLevel::Manifest,
    )
    .expect_err("a live deployment must never be restored over");
    assert!(error.to_string().contains("not empty"), "{error}");

    // And its objects are untouched.
    let restored = Deployment::start(&source_config).await;
    assert_eq!(
        restored.download("records", "notes.txt").await,
        b"second revision\n"
    );
    restored.stop().await;
}

/// The full level is the only one that can say anything about payloads, and it
/// has to notice a payload the catalog names but the backup does not carry.
#[tokio::test]
async fn the_full_level_detects_a_payload_the_catalog_still_references() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");

    let damaged = directory.path().join("missing-payload");
    copy_tree(&backup_directory, &damaged);
    let payload = first_payload(&damaged.join("objects")).expect("a payload to remove");
    let relative = payload
        .strip_prefix(&damaged)
        .expect("relative")
        .to_string_lossy()
        .replace('\\', "/");
    std::fs::remove_file(&payload).expect("remove a payload");
    remove_from_manifest(&damaged, &relative);

    // The checksum level cannot see this: every file it knows about is intact.
    let checksums =
        backup::verify(&damaged, VerificationLevel::Checksums, None).expect("verification runs");
    assert!(
        checksums.usable,
        "a checksum pass only proves the listed files are intact: {:?}",
        checksums.problems
    );

    let full = backup::verify(&damaged, VerificationLevel::Full, None).expect("verification runs");
    assert!(!full.usable, "the full level must catch a missing payload");
    assert_eq!(full.missing_payloads, Some(1));
    assert!(
        full.problems
            .iter()
            .any(|problem| problem.contains("reference payloads this backup does not contain")),
        "{:?}",
        full.problems
    );
}

/// Backups taken by the previous release must keep restoring, and must say
/// plainly that they carry metadata only.
#[tokio::test]
async fn a_metadata_only_backup_from_the_previous_format_still_restores() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let legacy = directory.path().join("legacy");
    record_store_server::backup_metadata(&source_config, &legacy).expect("legacy backup");

    let verification =
        backup::verify(&legacy, VerificationLevel::Checksums, None).expect("verification runs");
    assert!(
        verification.usable,
        "an operator's existing backups must not stop working: {:?}",
        verification.problems
    );
    let manifest = verification.manifest.expect("manifest");
    assert_eq!(manifest.backup_format_version, 1);
    assert!(
        manifest
            .components
            .iter()
            .all(|component| component.name == "metadata"),
        "a version 1 backup carries metadata and nothing else: {:?}",
        manifest.components
    );

    let restored_config = config_for(directory.path().join("legacy-restored"));
    let report = backup::restore(&restored_config, &legacy, VerificationLevel::Checksums)
        .expect("a legacy backup restores");
    assert_eq!(report.components.len(), 1);
}

/// A backup of a data directory no server ever opened is refused, because the
/// alternative is a manifest that promises components which do not exist.
#[tokio::test]
async fn an_uninitialized_data_directory_is_not_backed_up() {
    let directory = tempdir().expect("temporary directory");
    let config = config_for(directory.path().join("never-started"));
    std::fs::create_dir_all(&config.storage.data_directory).expect("create the directory");
    let error = backup::backup(&config, &directory.path().join("backup"), false)
        .expect_err("there is nothing here to back up");
    assert!(error.to_string().contains("not an initialized"), "{error}");
}

/// Incomplete uploads are staging files no client ever saw. Carrying them would
/// inflate every backup with bytes the restore has to throw away.
#[tokio::test]
async fn staging_files_are_left_out_of_the_backup() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let staging = source_config
        .storage
        .data_directory
        .join("objects")
        .join("tmp");
    std::fs::create_dir_all(&staging).expect("create a staging directory");
    std::fs::write(staging.join("abandoned.upload"), vec![0_u8; 4096])
        .expect("an abandoned upload");

    let backup_directory = directory.path().join("backup");
    let report = backup::backup(&source_config, &backup_directory, false).expect("take a backup");
    assert!(
        !report
            .manifest
            .files
            .iter()
            .any(|file| file.path.contains("tmp/")),
        "staging files must not be in the manifest: {:?}",
        report.manifest.files
    );
}

/// Retention, legal holds, and lifecycle rules all live in the catalog, so what
/// has to be shown is that the catalog arrives byte for byte — and that the
/// rules an operator can see really are still there afterwards.
///
/// Per-object Object Lock retention can only be set over the S3 protocol, which
/// this suite does not speak; the byte-identity assertion below is what covers
/// it, because a catalog that is identical cannot have lost a retention date.
#[tokio::test]
async fn protection_settings_and_the_catalog_survive_byte_for_byte() {
    let directory = tempdir().expect("temporary directory");
    let (source_config, _) = populated_deployment(&directory, false).await;
    let source_catalog = source_config
        .storage
        .data_directory
        .join("metadata")
        .join("catalog.redb");
    let original = std::fs::read(&source_catalog).expect("read the source catalog");

    let backup_directory = directory.path().join("backup");
    backup::backup(&source_config, &backup_directory, false).expect("take a backup");
    assert_eq!(
        std::fs::read(backup_directory.join("metadata").join("catalog.redb"))
            .expect("read the backed-up catalog"),
        original,
        "the catalog in the backup must be the catalog that was running"
    );

    let restored_config = config_for(directory.path().join("restored"));
    backup::restore(&restored_config, &backup_directory, VerificationLevel::Full).expect("restore");
    assert_eq!(
        std::fs::read(
            restored_config
                .storage
                .data_directory
                .join("metadata")
                .join("catalog.redb")
        )
        .expect("read the restored catalog"),
        original,
        "a restore that changed the catalog could have changed a retention date"
    );

    let restored = Deployment::start(&restored_config).await;
    let versioning = restored
        .get_json("/api/v1/buckets/records/versioning")
        .await;
    assert_eq!(
        versioning["versioning"], "enabled",
        "versioning is a protection setting and must survive: {versioning:?}"
    );
    let rules = restored.get_json("/api/v1/buckets/records/lifecycle").await;
    let rules = rules.as_array().expect("lifecycle rules").clone();
    assert_eq!(
        rules.len(),
        1,
        "a lifecycle rule is deployment state: {rules:?}"
    );
    assert_eq!(rules[0]["prefix"], "deep/");
    restored.stop().await;
}

fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
    std::fs::create_dir_all(destination).expect("create the copy");
    for entry in std::fs::read_dir(source).expect("read the source") {
        let entry = entry.expect("entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy a file");
        }
    }
}

fn edit_manifest(backup: &std::path::Path, edit: impl FnOnce(&mut Value)) {
    let path = backup.join(backup::MANIFEST_NAME);
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("parse");
    edit(&mut manifest);
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).expect("encode")).expect("write");
}

fn remove_from_manifest(backup: &std::path::Path, path: &str) {
    edit_manifest(backup, |manifest| {
        if let Some(files) = manifest["files"].as_array_mut() {
            files.retain(|file| file["path"].as_str() != Some(path));
        }
    });
}

fn flip_a_byte(path: &std::path::Path) {
    let mut bytes = std::fs::read(path).expect("read");
    let position = bytes.len() / 2;
    bytes[position] ^= 0xff;
    std::fs::write(path, bytes).expect("write");
}

fn first_payload(objects: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut pending = vec![objects.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).ok()? {
            let entry = entry.ok()?;
            if entry.file_type().ok()?.is_dir() {
                pending.push(entry.path());
            } else {
                return Some(entry.path());
            }
        }
    }
    None
}

/// Not a property of backup at all: a check that the same state survives an
/// ordinary restart, so a restore failure is never confused with state that was
/// never durable in the first place.
#[tokio::test]
async fn service_accounts_survive_an_ordinary_restart() {
    let directory = tempdir().expect("temporary directory");
    let config = config_for(directory.path().join("data"));
    let deployment = Deployment::start(&config).await;
    deployment.create_service_account("restart-probe").await;
    assert_eq!(
        deployment.service_account_count().await,
        1,
        "the account was not created"
    );
    deployment.stop().await;

    let deployment = Deployment::start(&config).await;
    let survivors = deployment.service_account_count().await;
    deployment.stop().await;
    assert_eq!(
        survivors, 1,
        "the account did not survive an ordinary restart"
    );
}
