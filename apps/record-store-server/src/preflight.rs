//! Start-up preconditions an operator can check before trusting a deployment.
//!
//! Validating configuration values catches a typo. It does not catch the data
//! directory somebody mounted read-only, the port another service already
//! holds, or a temporary directory on a different filesystem from the objects
//! it is supposed to be renamed into — which silently turns atomic publication
//! into a copy that can be interrupted halfway.
//!
//! These checks look at the machine rather than the file. They are run by
//! `record-store server doctor` as a diagnostic, and the subset that would
//! otherwise fail after the databases are already open is run at start-up, so
//! a deployment that cannot work says so before it touches any state.
//!
//! Nothing here reads or prints a secret value. Checks that concern key
//! material report only whether it is present and whether it is consistent
//! with what is already on disk.

use std::{
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
};

use record_store_config::Config;
use serde::Serialize;

/// How a single check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The precondition holds.
    Pass,
    /// The deployment will run, but something an operator should know is true.
    Warn,
    /// The deployment cannot work as configured.
    Fail,
}

/// One precondition, its outcome, and what to do about it.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Stable identifier, safe to match on in automation.
    pub name: &'static str,
    /// Outcome.
    pub status: Status,
    /// What was observed. Never contains a secret value.
    pub detail: String,
    /// The corrective action, present whenever the outcome is not a pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Pass,
            detail: detail.into(),
            remedy: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

/// Every check, and the worst outcome among them.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Overall outcome: the worst status of any check.
    pub status: Status,
    /// Checks in the order they ran.
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether any check failed.
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.status == Status::Fail
    }

    /// The failures, as one message suitable for a start-up error.
    #[must_use]
    pub fn failure_summary(&self) -> String {
        self.checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .map(|check| match &check.remedy {
                Some(remedy) => format!("{}: {} — {remedy}", check.name, check.detail),
                None => format!("{}: {}", check.name, check.detail),
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Runs every diagnostic check.
///
/// `bind_ports` is false when the caller is about to bind the listeners itself,
/// because probing a port by binding it and letting go is a race that the real
/// bind settles a moment later anyway.
#[must_use]
pub fn inspect(config: &Config, bind_ports: bool) -> Report {
    let mut checks = Vec::new();
    checks.push(configuration_check(config));
    checks.extend(data_directory_checks(config));
    checks.push(temporary_directory_check(config));
    checks.push(storage_format_check(config));
    checks.push(restore_marker_check(config));
    checks.push(free_space_check(config));
    if bind_ports {
        checks.extend(port_checks(config));
    }
    checks.extend(secret_checks(config));
    let status = checks
        .iter()
        .map(|check| check.status)
        .max()
        .unwrap_or(Status::Pass);
    Report { status, checks }
}

/// The checks that must hold before any durable state is opened.
///
/// Run from start-up. A deployment that trips one of these would otherwise
/// discover it after creating databases and taking the data lock — or, in the
/// case of an occupied port, after every subsystem is already running.
#[must_use]
pub fn startup_checks(config: &Config) -> Report {
    let mut checks = Vec::new();
    checks.extend(data_directory_checks(config));
    checks.push(temporary_directory_check(config));
    checks.push(restore_marker_check(config));
    let status = checks
        .iter()
        .map(|check| check.status)
        .max()
        .unwrap_or(Status::Pass);
    Report { status, checks }
}

fn configuration_check(config: &Config) -> Check {
    match config.validate() {
        Ok(()) => Check::pass("configuration", "every configured value is within range"),
        Err(error) => Check::fail(
            "configuration",
            error.to_string(),
            "correct the listed settings in the configuration file or environment",
        ),
    }
}

fn data_directory_checks(config: &Config) -> Vec<Check> {
    let directory = &config.storage.data_directory;
    let mut checks = Vec::new();
    let existing = match std::fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => Some(metadata),
        Ok(_) => {
            checks.push(Check::fail(
                "data_directory",
                format!("{} exists but is not a directory", directory.display()),
                "point storage.data_directory at a directory",
            ));
            return checks;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Not an error: the server creates it. What matters is whether the
            // parent allows that.
            let parent = directory.parent().unwrap_or(Path::new("."));
            checks.push(match writable(parent) {
                Ok(()) => Check::pass(
                    "data_directory",
                    format!("{} will be created on start-up", directory.display()),
                ),
                Err(error) => Check::fail(
                    "data_directory",
                    format!("{} cannot be created: {error}", directory.display()),
                    format!(
                        "grant the Record Store user write access to {}",
                        parent.display()
                    ),
                ),
            });
            return checks;
        }
        Err(error) => {
            checks.push(Check::fail(
                "data_directory",
                format!("{} is unreadable: {error}", directory.display()),
                "grant the Record Store user access to the data directory",
            ));
            return checks;
        }
    };

    checks.push(match writable(directory) {
        Ok(()) => Check::pass(
            "data_directory",
            format!("{} exists and is writable", directory.display()),
        ),
        Err(error) => Check::fail(
            "data_directory",
            format!("{} is not writable: {error}", directory.display()),
            "grant the Record Store user write access, or mount the volume read-write",
        ),
    });

    if let Some(metadata) = existing {
        checks.push(permission_check(directory, &metadata));
    }
    checks
}

#[cfg(unix)]
fn permission_check(directory: &Path, metadata: &std::fs::Metadata) -> Check {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o002 != 0 {
        Check::fail(
            "data_directory_permissions",
            format!(
                "{} is world-writable (mode {mode:04o})",
                directory.display()
            ),
            "chmod 0700 the data directory: anything on this host can otherwise replace stored payloads",
        )
    } else if mode & 0o007 != 0 {
        Check::warn(
            "data_directory_permissions",
            format!(
                "{} is readable by every local user (mode {mode:04o})",
                directory.display()
            ),
            "chmod 0700 the data directory unless another local service genuinely needs to read it",
        )
    } else {
        Check::pass(
            "data_directory_permissions",
            format!("{} is mode {mode:04o}", directory.display()),
        )
    }
}

#[cfg(not(unix))]
fn permission_check(directory: &Path, _metadata: &std::fs::Metadata) -> Check {
    Check::pass(
        "data_directory_permissions",
        format!(
            "{} permissions are not checked on this platform",
            directory.display()
        ),
    )
}

/// Confirms the temporary directory can be renamed into the objects directory.
///
/// Every payload is written to a staging file and published with a single
/// rename. A rename across filesystems is not possible, so the publication
/// would fall back to failing outright — and an operator who moved the
/// temporary directory to a faster disk would have broken durability without
/// being told.
fn temporary_directory_check(config: &Config) -> Check {
    let temporary = config.storage.effective_temporary_directory().to_path_buf();
    let objects = config.storage.data_directory.join("objects");
    match same_filesystem(&temporary, &objects) {
        Ok(true) => Check::pass(
            "atomic_publication",
            format!(
                "{} and {} are on one filesystem, so payloads publish with a rename",
                temporary.display(),
                objects.display()
            ),
        ),
        Ok(false) => Check::fail(
            "atomic_publication",
            format!(
                "{} and {} are on different filesystems",
                temporary.display(),
                objects.display()
            ),
            "put storage.temporary_directory on the same filesystem as the data directory; a payload cannot be published atomically across a mount boundary",
        ),
        Err(error) => Check::warn(
            "atomic_publication",
            format!("could not compare the two filesystems: {error}"),
            "confirm storage.temporary_directory is on the same filesystem as the data directory",
        ),
    }
}

#[cfg(unix)]
fn same_filesystem(left: &Path, right: &Path) -> Result<bool, std::io::Error> {
    use std::os::unix::fs::MetadataExt;

    // Neither may exist yet on a first install, so each is resolved to the
    // nearest ancestor that does. That is the filesystem it will be created on.
    let left = existing_ancestor(left)?;
    let right = existing_ancestor(right)?;
    Ok(std::fs::metadata(left)?.dev() == std::fs::metadata(right)?.dev())
}

#[cfg(not(unix))]
fn same_filesystem(_left: &Path, _right: &Path) -> Result<bool, std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "filesystem identity is not comparable on this platform",
    ))
}

fn existing_ancestor(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return Ok(candidate.to_path_buf());
        }
        candidate = candidate.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no existing ancestor directory",
            )
        })?;
    }
}

/// Reads the on-disk storage format without opening any database.
///
/// An incompatible format has to be met with an explanation rather than a redb
/// error about a file version, because the corrective action is an upgrade path
/// rather than anything the operator can fix in place.
fn storage_format_check(config: &Config) -> Check {
    #[derive(serde::Deserialize)]
    struct Record {
        storage_format_version: u32,
    }
    let path = config
        .storage
        .data_directory
        .join("system")
        .join("storage-format.json");
    match std::fs::read(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Check::pass(
            "storage_format",
            "no storage format record yet; this is a first install",
        ),
        Err(error) => Check::fail(
            "storage_format",
            format!("{} is unreadable: {error}", path.display()),
            "grant the Record Store user access to the data directory",
        ),
        Ok(encoded) => match serde_json::from_slice::<Record>(&encoded) {
            Ok(record) if record.storage_format_version == SUPPORTED_STORAGE_FORMAT => Check::pass(
                "storage_format",
                format!("on-disk storage format {SUPPORTED_STORAGE_FORMAT}"),
            ),
            Ok(record) => Check::fail(
                "storage_format",
                format!(
                    "on-disk storage format {} is not the {SUPPORTED_STORAGE_FORMAT} this release writes",
                    record.storage_format_version
                ),
                "run the release that matches this data directory, or restore into an empty one",
            ),
            Err(error) => Check::fail(
                "storage_format",
                format!("the storage format record is unreadable: {error}"),
                "this data directory is damaged; restore it from a backup",
            ),
        },
    }
}

/// The storage format this release writes.
///
/// Kept here rather than imported because the storage crate keeps it private,
/// and a diagnostic that opens the store to ask would defeat the point of
/// checking before anything is opened. The test below pins the two together.
pub(crate) const SUPPORTED_STORAGE_FORMAT: u32 = 1;

fn restore_marker_check(config: &Config) -> Check {
    if crate::backup::restore_in_progress(&config.storage.data_directory) {
        Check::fail(
            "restore_state",
            "a restore into this data directory was interrupted and never finished",
            "run `record-store server restore` again with the same backup; the interrupted attempt is cleared automatically",
        )
    } else {
        Check::pass("restore_state", "no interrupted restore")
    }
}

fn free_space_check(config: &Config) -> Check {
    let probe = match existing_ancestor(&config.storage.data_directory) {
        Ok(path) => path,
        Err(error) => {
            return Check::warn(
                "free_space",
                format!("free space could not be measured: {error}"),
                "confirm the data directory's filesystem has room",
            );
        }
    };
    match (fs2::available_space(&probe), fs2::total_space(&probe)) {
        (Ok(available), Ok(total)) if total > 0 => {
            let percent = available.saturating_mul(100) / total;
            if percent < 5 {
                Check::fail(
                    "free_space",
                    format!("{percent}% of the data filesystem is free ({available} bytes)"),
                    "free space before starting: uploads fail and maintenance cannot reclaim on a full filesystem",
                )
            } else if percent < 15 {
                Check::warn(
                    "free_space",
                    format!("{percent}% of the data filesystem is free ({available} bytes)"),
                    "plan capacity: see the capacity planning documentation",
                )
            } else {
                Check::pass(
                    "free_space",
                    format!("{percent}% of the data filesystem is free ({available} bytes)"),
                )
            }
        }
        _ => Check::warn(
            "free_space",
            "free space could not be measured on this filesystem",
            "confirm the data directory's filesystem has room",
        ),
    }
}

fn port_checks(config: &Config) -> Vec<Check> {
    let mut checks = vec![
        port_check("s3_listener", config.server.s3_bind),
        port_check("management_listener", config.server.api_bind),
    ];
    if config.server.mode.clustered() {
        checks.push(port_check("rpc_listener", config.server.rpc_bind));
    }
    checks
}

fn port_check(name: &'static str, address: SocketAddr) -> Check {
    match TcpListener::bind(address) {
        Ok(listener) => {
            drop(listener);
            Check::pass(name, format!("{address} is free"))
        }
        Err(error) => Check::fail(
            name,
            format!("{address} cannot be bound: {error}"),
            "stop whatever already holds the address, or configure a different one",
        ),
    }
}

/// Reports on key material by presence and consistency only.
///
/// The values themselves are never read into a message. What an operator needs
/// to know is whether a key is configured and whether it is the key this data
/// directory was written with — both answerable without quoting anything.
fn secret_checks(config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();

    checks.push(if config.auth.credential_master_key.is_some() {
        Check::pass(
            "credential_master_key",
            "a credential master key is configured",
        )
    } else if config.storage.encryption_enabled {
        Check::fail(
            "credential_master_key",
            "object encryption is enabled but no credential master key is configured",
            "set RECORD_STORE_CREDENTIAL_MASTER_KEY to the key this deployment was created with",
        )
    } else {
        Check::warn(
            "credential_master_key",
            "no credential master key is configured",
            "set a stable RECORD_STORE_CREDENTIAL_MASTER_KEY: without one, service-account credentials are sealed under the root secret and rotating it invalidates them",
        )
    });

    // A data directory that already records an encryption key reference can say
    // whether the configured key is the right one, which is the difference
    // between finding out now and finding out when the first object will not
    // decrypt.
    let encryption_record = config
        .storage
        .data_directory
        .join("system")
        .join("object-encryption.json");
    if encryption_record.is_file() {
        checks.push(if config.auth.credential_master_key.is_some() {
            Check::pass(
                "object_encryption",
                "this data directory holds encrypted payloads and a master key is configured; start-up confirms it is the right one",
            )
        } else {
            Check::fail(
                "object_encryption",
                "this data directory holds encrypted payloads but no master key is configured",
                "set RECORD_STORE_CREDENTIAL_MASTER_KEY to the key the payloads were written with; there is no way to recover them without it",
            )
        });
    }

    checks.push(if config.auth.management_system_token.is_some() {
        Check::pass(
            "management_token",
            "a dedicated management token is configured",
        )
    } else {
        Check::warn(
            "management_token",
            "no dedicated management token is configured; the management API falls back to root credentials",
            "set RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN to a distinct 32-byte value",
        )
    });

    checks.push(if config.auth.metrics_scrape_token.is_some() {
        Check::pass("metrics_token", "metrics scraping is enabled")
    } else {
        Check::warn(
            "metrics_token",
            "no metrics scrape token is configured; the metrics endpoint stays closed",
            "set RECORD_STORE_METRICS_SCRAPE_TOKEN if you intend to scrape metrics",
        )
    });

    checks
}

fn writable(directory: &Path) -> Result<(), std::io::Error> {
    let probe = directory.join(format!(
        ".record-store-writable-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&probe, b"probe")?;
    std::fs::remove_file(&probe)
}

#[cfg(test)]
mod tests {
    use record_store_config::SecretValue;

    use super::*;

    fn base_config(data_directory: PathBuf) -> Config {
        let mut config = Config::default();
        config.storage.data_directory = data_directory;
        config.auth.root_access_key = Some("test-access".into());
        config.auth.root_secret_key = Some(SecretValue::new("test-secret-at-least-sixteen"));
        config
    }

    fn check<'a>(report: &'a Report, name: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("no {name} check in the report"))
    }

    /// A first install has nothing on disk yet, and that is not a problem.
    #[test]
    fn a_first_install_passes_every_structural_check() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = base_config(directory.path().join("data"));
        let report = inspect(&config, false);
        assert_eq!(
            check(&report, "data_directory").status,
            Status::Pass,
            "{:?}",
            check(&report, "data_directory")
        );
        assert_eq!(check(&report, "storage_format").status, Status::Pass);
        assert_eq!(check(&report, "restore_state").status, Status::Pass);
        assert!(!report.has_failures(), "{}", report.failure_summary());
    }

    /// The whole point of the diagnostic: a data directory nobody can write to
    /// is reported before anything tries to open a database in it.
    #[cfg(unix)]
    #[test]
    fn an_unwritable_data_directory_fails_with_a_corrective_action() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let data = directory.path().join("data");
        std::fs::create_dir(&data).expect("create the data directory");
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o500))
            .expect("make it read-only");

        let config = base_config(data.clone());
        let report = startup_checks(&config);
        let check = check(&report, "data_directory");
        assert_eq!(check.status, Status::Fail, "{check:?}");
        assert!(
            check
                .remedy
                .as_ref()
                .is_some_and(|remedy| remedy.contains("write access")),
            "a failure has to say what to do: {check:?}"
        );

        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))
            .expect("restore permissions so the directory can be removed");
    }

    /// A world-writable data directory means any local account can replace a
    /// stored payload, so it is a failure rather than a note.
    #[cfg(unix)]
    #[test]
    fn a_world_writable_data_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let data = directory.path().join("data");
        std::fs::create_dir(&data).expect("create the data directory");
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o777))
            .expect("make it world-writable");

        let config = base_config(data.clone());
        let report = startup_checks(&config);
        assert_eq!(
            check(&report, "data_directory_permissions").status,
            Status::Fail
        );

        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))
            .expect("restore permissions");
    }

    /// Publication is a rename. A temporary directory on another filesystem
    /// cannot be renamed from, and the operator has to hear that in those terms
    /// rather than as an I/O error during the first upload.
    #[cfg(unix)]
    #[test]
    fn a_temporary_directory_on_another_filesystem_is_refused() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = base_config(directory.path().join("data"));
        // /dev is a separate filesystem on both macOS and Linux.
        config.storage.temporary_directory = Some(PathBuf::from("/dev"));
        let report = startup_checks(&config);
        let check = check(&report, "atomic_publication");
        assert_eq!(check.status, Status::Fail, "{check:?}");
        assert!(
            check
                .remedy
                .as_ref()
                .is_some_and(|remedy| remedy.contains("same filesystem")),
            "{check:?}"
        );
    }

    /// The default arrangement keeps the temporary directory inside the data
    /// directory, which is what makes publication atomic.
    #[test]
    fn the_default_temporary_directory_supports_atomic_publication() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = base_config(directory.path().join("data"));
        let report = startup_checks(&config);
        assert_eq!(check(&report, "atomic_publication").status, Status::Pass);
    }

    /// An on-disk format from another release is met with an upgrade
    /// instruction rather than a database error.
    #[test]
    fn an_incompatible_storage_format_is_named_as_such() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let data = directory.path().join("data");
        std::fs::create_dir_all(data.join("system")).expect("create the system directory");
        std::fs::write(
            data.join("system").join("storage-format.json"),
            br#"{"storage_format_version":99}"#,
        )
        .expect("write a future format record");

        let config = base_config(data);
        let report = inspect(&config, false);
        let check = check(&report, "storage_format");
        assert_eq!(check.status, Status::Fail, "{check:?}");
        assert!(check.detail.contains("99"), "{check:?}");
    }

    /// Encryption without a key is unrecoverable, so it is a failure at every
    /// opportunity to notice it.
    #[test]
    fn encrypted_payloads_without_a_key_fail_the_diagnostic() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let data = directory.path().join("data");
        std::fs::create_dir_all(data.join("system")).expect("create the system directory");
        std::fs::write(
            data.join("system").join("object-encryption.json"),
            br#"{"encryption_format_version":1,"algorithm":"x","key_reference":"aa"}"#,
        )
        .expect("write an encryption record");

        let config = base_config(data);
        let report = inspect(&config, false);
        let check = check(&report, "object_encryption");
        assert_eq!(check.status, Status::Fail, "{check:?}");
        assert!(
            check
                .remedy
                .as_ref()
                .is_some_and(|remedy| { remedy.contains("RECORD_STORE_CREDENTIAL_MASTER_KEY") }),
            "{check:?}"
        );
    }

    /// An occupied port is the failure this catches earliest of all, and the
    /// one that used to surface only after every subsystem had started.
    #[test]
    fn an_occupied_port_is_reported_before_start_up() {
        let held = TcpListener::bind("127.0.0.1:0").expect("hold a port");
        let address = held.local_addr().expect("address");

        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = base_config(directory.path().join("data"));
        config.server.s3_bind = address;

        let report = inspect(&config, true);
        let check = check(&report, "s3_listener");
        assert_eq!(check.status, Status::Fail, "{check:?}");
        assert!(check.detail.contains(&address.to_string()), "{check:?}");
    }

    /// A diagnostic that leaked a key would be worse than no diagnostic. The
    /// report is rendered in full and searched for every configured secret.
    #[test]
    fn no_check_ever_repeats_a_secret_value() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut config = base_config(directory.path().join("data"));
        let secrets = [
            "test-secret-at-least-sixteen",
            "master-key-value-at-least-thirty-two-bytes",
            "management-token-value-at-least-thirty-two",
            "metrics-token-value-at-least-thirty-two-by",
        ];
        config.auth.credential_master_key = Some(SecretValue::new(secrets[1]));
        config.auth.management_system_token = Some(SecretValue::new(secrets[2]));
        config.auth.metrics_scrape_token = Some(SecretValue::new(secrets[3]));

        let rendered =
            serde_json::to_string(&inspect(&config, true)).expect("the report serializes");
        for secret in secrets {
            assert!(
                !rendered.contains(secret),
                "the diagnostic leaked a secret value"
            );
        }
    }

    /// The constant this module checks against has to stay equal to the one the
    /// storage crate writes, or the diagnostic would confidently approve a
    /// format the server then refuses.
    #[tokio::test]
    async fn the_checked_storage_format_matches_what_the_store_writes() {
        use record_store_metadata::RedbMetadataRepository;
        use std::sync::Arc;

        let directory = tempfile::tempdir().expect("temporary directory");
        let data = directory.path().join("data");
        let metadata = Arc::new(
            RedbMetadataRepository::open(data.join("catalog.redb"))
                .await
                .expect("catalog"),
        );
        record_store_storage::LocalFilesystemStore::open(&data, data.join("tmp"), metadata)
            .await
            .expect("store");

        #[derive(serde::Deserialize)]
        struct Record {
            storage_format_version: u32,
        }
        let encoded = std::fs::read(data.join("system").join("storage-format.json"))
            .expect("the store writes a format record");
        let record: Record = serde_json::from_slice(&encoded).expect("format record");
        assert_eq!(
            record.storage_format_version, SUPPORTED_STORAGE_FORMAT,
            "the diagnostic checks a storage format the store no longer writes"
        );
    }
}
