//! Coordinated offline backup of a whole standalone deployment.
//!
//! A deployment is recoverable only if three things travel together: the
//! catalog that names objects, the payloads it names, and the system records
//! that say which storage format and which master key those payloads were
//! written under. Backing up one without the others produces something that
//! looks like a backup and restores into dangling references or payloads
//! nobody can decrypt, so this module treats all three as one unit and refuses
//! to publish a backup that is missing any of them.
//!
//! The consistency guarantee is stated rather than implied. The whole copy runs
//! while this process holds the data directory's exclusive lock, which the
//! server also takes, so no Record Store process can be writing. That is a
//! genuine point-in-time copy of a stopped deployment — not a snapshot of a
//! live one — and the manifest records it as such.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use record_store_config::Config;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{MetadataBackupError, acquire_data_lock};

/// On-disk layout version of a coordinated backup directory.
///
/// Version 1 was the metadata-only backup this replaces. A version 1 directory
/// is still readable by [`restore`], because refusing to restore backups an
/// operator already holds would be the worst possible moment to discover a
/// format change.
pub const BACKUP_FORMAT_VERSION: u32 = 2;

/// Name of the manifest inside a backup directory.
pub const MANIFEST_NAME: &str = "backup-manifest.json";

/// Marker present only while a backup is being written.
///
/// Created before the first byte is copied and removed after the manifest is
/// durable, so "complete" is a state on disk rather than an assumption about
/// how the last run ended.
pub const INCOMPLETE_MARKER: &str = "INCOMPLETE";

/// Marker written into a data directory for the duration of a restore.
///
/// A restore renames three directories into place, which cannot be one atomic
/// step. The marker is what stops the window between them from looking like a
/// deployment that is ready to serve: the server refuses to start while it
/// exists, and a repeated restore knows it may clear what the interrupted one
/// left behind.
pub const RESTORE_MARKER: &str = ".record-store-restore-in-progress";

/// Largest manifest this reads, so a hostile or corrupt file cannot be used to
/// exhaust memory before it is rejected.
const MAXIMUM_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// Fraction of the measured source size kept free at the destination on top of
/// the copy itself, absorbing filesystem overhead and rounding.
const DESTINATION_MARGIN_PERCENT: u64 = 5;

/// Components a backup is made of, in the order they are copied.
///
/// `objects` may legitimately be empty — a deployment that has stored nothing
/// still has a valid backup — but the component is always present in the
/// manifest so a reader can tell "no objects" from "objects were not copied".
const COMPONENTS: [&str; 3] = ["metadata", "system", "objects"];

/// Files without which a restored deployment is not the deployment that was
/// backed up.
///
/// `catalog.redb` names every object; `credentials.redb` holds every service
/// account and policy; `storage-format.json` says which on-disk format the
/// payloads use. A backup missing any of these used to restore without
/// complaint and silently produce an emptier deployment than the one it came
/// from.
const REQUIRED_FILES: [&str; 3] = [
    "metadata/catalog.redb",
    "metadata/credentials.redb",
    "system/storage-format.json",
];

/// What a completed backup says about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Layout version of the backup directory.
    pub backup_format_version: u32,
    /// Release that produced the backup, for support and for upgrade ordering.
    pub record_store_version: String,
    /// Commit of the build that produced the backup. Absent from backups made
    /// before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_store_commit: Option<String>,
    /// Catalog schema the metadata files were written by.
    pub metadata_schema_version: u64,
    /// Payload layout the objects were written under.
    pub storage_format_version: u32,
    /// When the copy finished.
    pub created_unix_seconds: u64,
    /// How the copy was taken, spelled out rather than implied.
    pub consistency: String,
    /// Always false. Credentials and keys are recovered separately; see the
    /// backup documentation.
    pub secrets_included: bool,
    /// Whether payloads in this backup are encrypted at rest.
    pub objects_encrypted: bool,
    /// A one-way reference to the key material that seals this deployment's
    /// credentials, share links and webhook secrets: the credential master
    /// key, or the root secret where none is set. A restore under other
    /// material is refused before it writes anything, because those secrets
    /// would never unseal. Absent from backups made before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealing_key_reference: Option<String>,
    /// Per-component inventory.
    pub components: Vec<BackupComponent>,
    /// Every file, with the checksum that proves it arrived intact.
    pub files: Vec<BackupFile>,
    /// Sum of every file's size.
    pub total_bytes: u64,
}

/// One component's contribution to a backup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupComponent {
    /// Component name, one of `metadata`, `system`, `objects`.
    pub name: String,
    /// Whether a restore refuses to proceed without it.
    pub required: bool,
    /// Files the component contributed.
    pub file_count: u64,
    /// Bytes the component contributed.
    pub bytes: u64,
}

/// One copied file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupFile {
    /// Path relative to the backup directory, always with forward slashes.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the contents, hex encoded.
    pub sha256: String,
}

/// What a backup run did, for automation and for the operator's log.
#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    /// Where the backup was written.
    pub destination: PathBuf,
    /// The manifest that was published.
    pub manifest: BackupManifest,
}

/// How thoroughly a backup was checked.
///
/// The levels are named separately because "the manifest parses" and "every
/// payload the catalog names is present" are very different assurances, and
/// calling the first one a verification is how operators end up trusting a
/// backup that cannot restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationLevel {
    /// Structure only: the manifest parses, the format is supported, the
    /// required components are listed, and the backup is marked complete.
    /// Reads no payload bytes and proves nothing about file contents.
    Manifest,
    /// Everything above, plus every listed file's size and SHA-256 recomputed
    /// from the bytes on disk.
    Checksums,
    /// Everything above, plus a cross-check that every payload the catalog
    /// references is present in the backup and every payload in the backup is
    /// referenced.
    Full,
}

impl VerificationLevel {
    /// Parses the level an operator typed.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "manifest" => Some(Self::Manifest),
            "checksums" => Some(Self::Checksums),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The name used in reports, so a report never overstates what ran.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Checksums => "checksums",
            Self::Full => "full",
        }
    }
}

/// Outcome of verifying a backup, including what was *not* checked.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    /// The backup that was checked.
    pub backup: PathBuf,
    /// The level that actually ran.
    pub level: String,
    /// Whether the backup is usable at that level.
    pub usable: bool,
    /// Files whose bytes were recomputed. Zero at the `manifest` level.
    pub files_checksummed: u64,
    /// Bytes read while checksumming.
    pub bytes_read: u64,
    /// Payload references resolved. `None` unless the `full` level ran.
    pub payload_references_checked: Option<u64>,
    /// Referenced payloads that are absent from the backup.
    pub missing_payloads: Option<u64>,
    /// Payloads present in the backup that nothing references.
    pub unreferenced_payloads: Option<u64>,
    /// Whether the supplied master key is the one these payloads were written
    /// under. `None` when the backup is unencrypted or no key was supplied, so
    /// "not checked" is never mistaken for "checked and fine".
    pub encryption_key_matches: Option<bool>,
    /// Everything wrong with the backup, in the order it was found.
    pub problems: Vec<String>,
    /// The manifest, when one could be read.
    pub manifest: Option<BackupManifest>,
}

/// What a restore put in place, and what it proved about it.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    /// Backup that was restored.
    pub source: PathBuf,
    /// Data directory it was restored into.
    pub data_directory: PathBuf,
    /// Verification level that ran before anything was written.
    pub verified_at_level: String,
    /// Files restored per component.
    pub components: Vec<BackupComponent>,
    /// Total bytes restored.
    pub total_bytes: u64,
    /// Whether an interrupted earlier restore was cleared first.
    pub cleared_interrupted_restore: bool,
    /// Anything the operator still has to resolve, such as key material.
    pub outstanding: Vec<String>,
}

/// Writes a coordinated backup of a stopped deployment.
///
/// Holds the data directory's exclusive lock for the whole copy, so the server
/// cannot be running and the result is a single point in time.
pub fn backup(
    config: &Config,
    destination: &Path,
    replace_incomplete: bool,
) -> Result<BackupReport, BackupError> {
    config.validate().map_err(BackupError::Configuration)?;
    let data_directory = &config.storage.data_directory;
    if !data_directory.exists() {
        return Err(BackupError::NotInitialized(data_directory.clone()));
    }
    let _lock = acquire_data_lock(data_directory).map_err(BackupError::DataDirectoryInUse)?;
    // A restore that never finished may have moved some components into place
    // and not others. Copying that would publish a complete-looking backup of
    // a deployment that never existed.
    if restore_in_progress(data_directory) {
        return Err(BackupError::RestoreInProgress(data_directory.clone()));
    }
    // A data directory with no storage format record was never opened by a
    // server. Backing it up would produce a manifest promising components that
    // do not exist, so the refusal names what is actually wrong.
    if !data_directory
        .join("system")
        .join("storage-format.json")
        .is_file()
    {
        return Err(BackupError::NotInitialized(data_directory.clone()));
    }

    prepare_destination(destination, replace_incomplete)?;

    let sources: Vec<(String, PathBuf)> = COMPONENTS
        .iter()
        .map(|name| ((*name).to_owned(), data_directory.join(name)))
        .collect();
    let required_bytes = sources.iter().try_fold(0_u64, |total, (_, path)| {
        measure_directory(path).map(|bytes| total.saturating_add(bytes))
    })?;
    check_destination_space(destination, required_bytes)?;

    // The marker goes down before the first byte, so an interruption at any
    // point after this leaves something that says plainly it is not a backup.
    write_incomplete_marker(destination)?;

    let mut files = Vec::new();
    let mut components = Vec::new();
    for (name, source) in &sources {
        let target = destination.join(name);
        std::fs::create_dir_all(&target).map_err(BackupError::Io)?;
        let mut component = BackupComponent {
            name: name.clone(),
            required: name != "objects",
            file_count: 0,
            bytes: 0,
        };
        copy_tree(source, &target, name, &mut files, &mut component)?;
        sync_directory(&target)?;
        components.push(component);
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));

    let present: std::collections::BTreeSet<&str> =
        files.iter().map(|file| file.path.as_str()).collect();
    for required in REQUIRED_FILES {
        if !present.contains(required) {
            return Err(BackupError::MissingComponent((*required).to_owned()));
        }
    }

    let manifest = BackupManifest {
        backup_format_version: BACKUP_FORMAT_VERSION,
        record_store_version: env!("CARGO_PKG_VERSION").to_owned(),
        record_store_commit: Some(crate::BUILD_COMMIT.to_owned()),
        metadata_schema_version: copied_schema_version(destination),
        storage_format_version: read_storage_format_version(destination)?,
        created_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        consistency: "offline-exclusive-lock".to_owned(),
        secrets_included: false,
        objects_encrypted: destination
            .join("system")
            .join("object-encryption.json")
            .is_file(),
        sealing_key_reference: Some(sealing_key_reference(config)?),
        total_bytes: files.iter().map(|file| file.size).sum(),
        components,
        files,
    };

    let encoded = serde_json::to_vec_pretty(&manifest)?;
    write_durable(&destination.join(MANIFEST_NAME), &encoded)?;
    sync_directory(destination)?;
    // Only now is the backup a backup. Removing the marker last is what makes
    // completeness observable rather than assumed.
    std::fs::remove_file(destination.join(INCOMPLETE_MARKER)).map_err(BackupError::Io)?;
    sync_directory(destination)?;

    Ok(BackupReport {
        destination: destination.to_path_buf(),
        manifest,
    })
}

/// Checks a backup without writing anything, at an explicitly named level.
pub fn verify(
    backup: &Path,
    level: VerificationLevel,
    master_key: Option<&[u8]>,
) -> Result<VerificationReport, BackupError> {
    let mut report = VerificationReport {
        backup: backup.to_path_buf(),
        level: level.as_str().to_owned(),
        usable: false,
        files_checksummed: 0,
        bytes_read: 0,
        payload_references_checked: None,
        missing_payloads: None,
        unreferenced_payloads: None,
        encryption_key_matches: None,
        problems: Vec::new(),
        manifest: None,
    };

    if backup.join(INCOMPLETE_MARKER).exists() {
        report.problems.push(
            "the backup is marked INCOMPLETE: it was interrupted while being written and must not be restored"
                .to_owned(),
        );
        return Ok(report);
    }
    let manifest = match read_manifest(backup) {
        Ok(manifest) => manifest,
        Err(error) => {
            report.problems.push(error.to_string());
            return Ok(report);
        }
    };
    if manifest.backup_format_version > BACKUP_FORMAT_VERSION {
        report.problems.push(format!(
            "backup format {} is newer than this release understands ({BACKUP_FORMAT_VERSION}); upgrade Record Store before restoring",
            manifest.backup_format_version
        ));
    }
    if manifest.storage_format_version > crate::preflight::SUPPORTED_STORAGE_FORMAT {
        report.problems.push(format!(
            "storage format {} is newer than this release understands ({}); upgrade Record Store before restoring",
            manifest.storage_format_version,
            crate::preflight::SUPPORTED_STORAGE_FORMAT
        ));
    }
    if manifest.metadata_schema_version > record_store_metadata::METADATA_SCHEMA_VERSION {
        report.problems.push(format!(
            "catalog schema {} is newer than this release understands ({}); upgrade Record Store before restoring",
            manifest.metadata_schema_version,
            record_store_metadata::METADATA_SCHEMA_VERSION
        ));
    }
    let listed: std::collections::BTreeSet<&str> = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    for required in required_files_for(&manifest) {
        if !listed.contains(required) {
            report.problems.push(format!(
                "required component {required} is not in the backup"
            ));
        }
    }
    for file in &manifest.files {
        if !safe_relative_path(&file.path) {
            report
                .problems
                .push(format!("manifest lists an unsafe path: {}", file.path));
        }
    }
    report.manifest = Some(manifest.clone());

    if level != VerificationLevel::Manifest && report.problems.is_empty() {
        for file in &manifest.files {
            let path = source_path(backup, &manifest, file);
            match checksum_file(&path) {
                Ok((size, sha256)) => {
                    report.files_checksummed = report.files_checksummed.saturating_add(1);
                    report.bytes_read = report.bytes_read.saturating_add(size);
                    if size != file.size {
                        report.problems.push(format!(
                            "{} is {size} bytes, the manifest says {}",
                            file.path, file.size
                        ));
                    } else if sha256 != file.sha256 {
                        report.problems.push(format!(
                            "{} does not match its recorded checksum",
                            file.path
                        ));
                    }
                }
                Err(error) => report
                    .problems
                    .push(format!("{} could not be read: {error}", file.path)),
            }
        }
        // A file nobody listed is not necessarily an attack, but it is not part
        // of what will be restored, and an operator who put something there
        // deserves to be told it is being ignored.
        for stray in unlisted_files(backup, &listed)? {
            report
                .problems
                .push(format!("{stray} is present but not listed in the manifest"));
        }
    }

    if level == VerificationLevel::Full && report.problems.is_empty() {
        match cross_check_payloads(backup) {
            Ok(cross) => {
                report.payload_references_checked = Some(cross.references);
                report.missing_payloads = Some(cross.missing);
                report.unreferenced_payloads = Some(cross.unreferenced);
                if cross.missing > 0 {
                    report.problems.push(format!(
                        "{} object versions reference payloads this backup does not contain",
                        cross.missing
                    ));
                }
                // Unreferenced payloads waste space but lose nothing, so they
                // are reported without making the backup unusable.
            }
            Err(error) => report
                .problems
                .push(format!("payload cross-check failed: {error}")),
        }
    }

    // Checked at every level, because a backup whose key nobody has is
    // unrestorable no matter how intact its bytes are, and that is worth
    // knowing before the restore rather than after it.
    if manifest.objects_encrypted {
        match master_key {
            Some(key) => match encryption_key_matches(backup, key) {
                Ok(matches) => {
                    report.encryption_key_matches = Some(matches);
                    if !matches {
                        report.problems.push(
                            "the supplied credential master key is not the one these payloads were written under"
                                .to_owned(),
                        );
                    }
                }
                Err(error) => report
                    .problems
                    .push(format!("the encryption record could not be read: {error}")),
            },
            None => report.problems.push(
                "this backup holds encrypted payloads and no credential master key was supplied to check against"
                    .to_owned(),
            ),
        }
    }

    report.usable = report.problems.is_empty();
    Ok(report)
}

/// A reference to the material credentials are sealed under, safe to store
/// and print: a domain-separated digest, from which the material cannot be
/// recovered. The material is what the credential store derives its key from.
fn sealing_key_reference(config: &Config) -> Result<String, BackupError> {
    let material = match &config.auth.credential_master_key {
        Some(key) => key.expose().to_owned(),
        None => config
            .root_credentials()
            .map_err(BackupError::Configuration)?
            .1
            .expose()
            .to_owned(),
    };
    let mut digest = sha2::Sha256::new();
    sha2::Digest::update(
        &mut digest,
        b"record-store/backup-sealing-key-reference/v1\0",
    );
    sha2::Digest::update(&mut digest, material.as_bytes());
    Ok(hex::encode(&sha2::Digest::finalize(digest)[..16]))
}

/// Compares a master key against the reference the backup carries.
///
/// Only the derived reference is compared; the key itself is never written
/// anywhere, and the reference reveals nothing about it.
fn encryption_key_matches(backup: &Path, master_key: &[u8]) -> Result<bool, BackupError> {
    #[derive(Deserialize)]
    struct Record {
        key_reference: String,
    }
    let encoded = std::fs::read(backup.join("system").join("object-encryption.json"))
        .map_err(BackupError::Io)?;
    let record: Record = serde_json::from_slice(&encoded)?;
    let expected = record_store_storage::object_key_reference(master_key)
        .map_err(|error| BackupError::Catalog(error.to_string()))?;
    Ok(record.key_reference == expected)
}

/// Restores a verified backup into an empty or interrupted data directory.
pub fn restore(
    config: &Config,
    source: &Path,
    level: VerificationLevel,
) -> Result<RestoreReport, BackupError> {
    config.validate().map_err(BackupError::Configuration)?;
    let master_key = config
        .auth
        .credential_master_key
        .as_ref()
        .map(|key| key.expose().as_bytes().to_vec());
    let verification = verify(source, level, master_key.as_deref())?;
    if !verification.usable {
        return Err(BackupError::Unusable(verification.problems));
    }
    let manifest = verification
        .manifest
        .clone()
        .ok_or_else(|| BackupError::Unusable(vec!["the backup has no manifest".to_owned()]))?;
    if manifest
        .sealing_key_reference
        .as_ref()
        .is_some_and(|recorded| *recorded != sealing_key_reference(config).unwrap_or_default())
    {
        return Err(BackupError::Unusable(vec![
            "the configured credential master key (or, without one, the root secret) is not the \
             one this deployment's credentials, share links and webhook secrets were sealed under"
                .to_owned(),
        ]));
    }

    let data_directory = &config.storage.data_directory;
    std::fs::create_dir_all(data_directory).map_err(BackupError::Io)?;
    let _lock = acquire_data_lock(data_directory).map_err(BackupError::DataDirectoryInUse)?;

    // A marker means the last restore never finished, so nothing here was ever
    // served and clearing it is safe. Without one, a populated directory is a
    // real deployment and must never be merged into.
    let interrupted = data_directory.join(RESTORE_MARKER).exists();
    if interrupted {
        for component in COMPONENTS {
            remove_directory_if_present(&data_directory.join(component))?;
        }
        remove_stale_staging(data_directory)?;
    } else {
        for component in COMPONENTS {
            let path = data_directory.join(component);
            if directory_has_entries(&path)? {
                return Err(BackupError::DestinationNotEmpty(path));
            }
        }
    }

    write_durable(
        &data_directory.join(RESTORE_MARKER),
        format!("restore from {} in progress\n", source.display()).as_bytes(),
    )?;
    sync_directory(data_directory)?;

    let staging = data_directory.join(format!(".restore-{}", Uuid::new_v4().simple()));
    std::fs::create_dir(&staging).map_err(BackupError::Io)?;
    for file in &manifest.files {
        if !safe_relative_path(&file.path) {
            return Err(BackupError::UnsafePath(file.path.clone()));
        }
        let target = staging.join(&file.path);
        let parent = target
            .parent()
            .ok_or_else(|| BackupError::UnsafePath(file.path.clone()))?;
        std::fs::create_dir_all(parent).map_err(BackupError::Io)?;
        let (size, sha256) = copy_with_checksum(&source_path(source, &manifest, file), &target)?;
        if size != file.size || sha256 != file.sha256 {
            return Err(BackupError::ChecksumMismatch(file.path.clone()));
        }
    }
    sync_tree(&staging)?;

    for component in COMPONENTS {
        let staged = staging.join(component);
        if !staged.exists() {
            std::fs::create_dir_all(&staged).map_err(BackupError::Io)?;
        }
        let target = data_directory.join(component);
        remove_directory_if_present(&target)?;
        std::fs::rename(&staged, &target).map_err(BackupError::Io)?;
    }
    sync_directory(data_directory)?;
    remove_directory_if_present(&staging)?;
    std::fs::remove_file(data_directory.join(RESTORE_MARKER)).map_err(BackupError::Io)?;
    sync_directory(data_directory)?;

    let mut outstanding = Vec::new();
    if manifest.objects_encrypted {
        outstanding.push(if verification.encryption_key_matches == Some(true) {
            "payloads are encrypted and the configured credential master key was confirmed to be the right one; it is not in the backup, so keep recovering it separately"
                .to_owned()
        } else {
            "payloads are encrypted: start this deployment with the same RECORD_STORE_CREDENTIAL_MASTER_KEY, which is not in the backup"
                .to_owned()
        });
    }
    outstanding.push(
        "root credentials and management tokens come from the environment, not the backup"
            .to_owned(),
    );

    Ok(RestoreReport {
        source: source.to_path_buf(),
        data_directory: data_directory.clone(),
        verified_at_level: level.as_str().to_owned(),
        components: manifest.components.clone(),
        total_bytes: manifest.total_bytes,
        cleared_interrupted_restore: interrupted,
        outstanding,
    })
}

/// Reports whether a data directory holds an unfinished restore.
///
/// Start-up consults this so a half-restored deployment cannot come up looking
/// ready.
#[must_use]
pub fn restore_in_progress(data_directory: &Path) -> bool {
    data_directory.join(RESTORE_MARKER).exists()
}

struct PayloadCrossCheck {
    references: u64,
    missing: u64,
    unreferenced: u64,
}

/// Cross-checks the backup's catalog against the payloads it carries.
///
/// The catalog is copied into a scratch directory first: opening a redb file
/// can write to it, and a verification that modifies the thing it is verifying
/// is not a verification.
fn cross_check_payloads(backup: &Path) -> Result<PayloadCrossCheck, BackupError> {
    let scratch =
        std::env::temp_dir().join(format!("record-store-verify-{}", Uuid::new_v4().simple()));
    std::fs::create_dir(&scratch).map_err(BackupError::Io)?;
    let guard = ScratchDirectory(scratch.clone());
    let catalog = scratch.join("catalog.redb");
    copy_with_checksum(&backup.join("metadata").join("catalog.redb"), &catalog)?;

    // On its own thread, because the caller may already be inside a runtime —
    // the CLI is — and a nested `block_on` panics rather than returning an
    // error an operator could act on.
    let backup = backup.to_path_buf();
    let result = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(BackupError::Io)?;
                cross_check_in(&runtime, &backup, &catalog)
            })
            .join()
            .map_err(|_| BackupError::Catalog("the cross-check thread panicked".to_owned()))?
    });
    drop(guard);
    result
}

fn cross_check_in(
    runtime: &tokio::runtime::Runtime,
    backup: &Path,
    catalog: &Path,
) -> Result<PayloadCrossCheck, BackupError> {
    runtime.block_on(async {
        use record_store_metadata::MetadataRepository;

        let repository = record_store_metadata::RedbMetadataRepository::open(&catalog)
            .await
            .map_err(|error| BackupError::Catalog(error.to_string()))?;
        let objects = backup.join("objects");
        let mut referenced = std::collections::BTreeSet::new();
        let mut missing = 0_u64;
        let mut references = 0_u64;
        let mut cursor = None;
        loop {
            let page = repository
                .list_payload_references(cursor, 1_000)
                .await
                .map_err(|error| BackupError::Catalog(error.to_string()))?;
            for object_id in page.object_ids {
                references = references.saturating_add(1);
                let encoded = object_id.as_uuid().simple().to_string();
                let path = objects
                    .join(&encoded[0..2])
                    .join(&encoded[2..4])
                    .join(&encoded);
                if !path.is_file() {
                    missing = missing.saturating_add(1);
                }
                referenced.insert(encoded);
            }
            cursor = page.next_object_id;
            if cursor.is_none() {
                break;
            }
        }
        let mut unreferenced = 0_u64;
        for entry in walk_files(&objects)? {
            let name = entry
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            if !referenced.contains(&name) {
                unreferenced = unreferenced.saturating_add(1);
            }
        }
        Ok::<_, BackupError>(PayloadCrossCheck {
            references,
            missing,
            unreferenced,
        })
    })
}

/// Removes a scratch directory even when the work inside it fails.
struct ScratchDirectory(PathBuf);

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn prepare_destination(destination: &Path, replace_incomplete: bool) -> Result<(), BackupError> {
    match std::fs::read_dir(destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(destination).map_err(BackupError::Io)?;
            return Ok(());
        }
        Err(error) => return Err(BackupError::Io(error)),
        Ok(mut entries) => {
            if entries.next().is_none() {
                return Ok(());
            }
        }
    }
    let incomplete = destination.join(INCOMPLETE_MARKER).exists();
    let complete = destination.join(MANIFEST_NAME).is_file() && !incomplete;
    if complete {
        // The one thing a backup command must never do is destroy the backup an
        // operator already has.
        return Err(BackupError::DestinationHoldsBackup(
            destination.to_path_buf(),
        ));
    }
    if incomplete && replace_incomplete {
        std::fs::remove_dir_all(destination).map_err(BackupError::Io)?;
        std::fs::create_dir_all(destination).map_err(BackupError::Io)?;
        return Ok(());
    }
    Err(BackupError::DestinationNotEmpty(destination.to_path_buf()))
}

/// Compares the copy against the destination's free space.
///
/// This is advisory: nothing stops another process from consuming the space
/// between the check and the copy, and a full destination is still handled
/// safely because the backup is only published once the manifest is durable.
/// The check exists so the common case fails in a second rather than an hour.
fn check_destination_space(destination: &Path, required_bytes: u64) -> Result<(), BackupError> {
    let probe = if destination.exists() {
        destination.to_path_buf()
    } else {
        destination.parent().unwrap_or(Path::new(".")).to_path_buf()
    };
    let available = fs2::available_space(&probe).map_err(BackupError::Io)?;
    let margin = required_bytes / 100 * DESTINATION_MARGIN_PERCENT;
    let needed = required_bytes.saturating_add(margin);
    if available < needed {
        return Err(BackupError::InsufficientSpace {
            required_bytes: needed,
            available_bytes: available,
        });
    }
    Ok(())
}

fn write_incomplete_marker(destination: &Path) -> Result<(), BackupError> {
    write_durable(
        &destination.join(INCOMPLETE_MARKER),
        b"This backup is being written and is not usable.\n\
          It is removed only once every component is durable and the manifest is published.\n\
          A backup directory containing this file must not be restored.\n",
    )?;
    sync_directory(destination)
}

/// The schema the copied catalog is actually at. A pre-upgrade backup taken
/// with a newer binary holds the older release's catalog, and says so. A
/// catalog that cannot be read without repair -- one a crash left behind --
/// is labelled with this binary's schema, the most it can be.
fn copied_schema_version(backup: &Path) -> u64 {
    record_store_metadata::stored_schema_version(&backup.join("metadata").join("catalog.redb"))
        .ok()
        .flatten()
        .unwrap_or(record_store_metadata::METADATA_SCHEMA_VERSION)
}

fn read_storage_format_version(backup: &Path) -> Result<u32, BackupError> {
    #[derive(Deserialize)]
    struct Record {
        storage_format_version: u32,
    }
    let encoded = std::fs::read(backup.join("system").join("storage-format.json"))
        .map_err(BackupError::Io)?;
    let record: Record = serde_json::from_slice(&encoded)?;
    Ok(record.storage_format_version)
}

fn read_manifest(backup: &Path) -> Result<BackupManifest, BackupError> {
    let path = backup.join(MANIFEST_NAME);
    let path = if path.is_file() {
        path
    } else {
        // A version 1 directory names its manifest differently. Reading it here
        // means an operator's existing backups keep working.
        backup.join("manifest.json")
    };
    let metadata = std::fs::metadata(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BackupError::NoManifest(backup.to_path_buf())
        } else {
            BackupError::Io(error)
        }
    })?;
    if metadata.len() > MAXIMUM_MANIFEST_BYTES {
        return Err(BackupError::InvalidManifest);
    }
    let encoded = std::fs::read(&path).map_err(BackupError::Io)?;
    if let Ok(manifest) = serde_json::from_slice::<BackupManifest>(&encoded) {
        return Ok(manifest);
    }
    legacy_manifest(&encoded)
}

/// Reads a version 1, metadata-only manifest as if it were a modern one.
///
/// Version 1 backups carry no payloads and no system records, so they are
/// presented with exactly those components and nothing more. A restore of one
/// therefore reports honestly what it did and did not put back, rather than
/// pretending the deployment is whole.
fn legacy_manifest(encoded: &[u8]) -> Result<BackupManifest, BackupError> {
    #[derive(Deserialize)]
    struct LegacyFile {
        name: String,
        size: u64,
        sha256: String,
    }
    #[derive(Deserialize)]
    struct Legacy {
        backup_format_version: u32,
        metadata_schema_version: u64,
        created_unix_seconds: u64,
        files: Vec<LegacyFile>,
    }
    let legacy: Legacy =
        serde_json::from_slice(encoded).map_err(|_| BackupError::InvalidManifest)?;
    if legacy.backup_format_version != 1 {
        return Err(BackupError::InvalidManifest);
    }
    let files: Vec<BackupFile> = legacy
        .files
        .into_iter()
        .map(|file| BackupFile {
            path: format!("metadata/{}", file.name),
            size: file.size,
            sha256: file.sha256,
        })
        .collect();
    Ok(BackupManifest {
        backup_format_version: 1,
        record_store_version: "unknown".to_owned(),
        record_store_commit: None,
        metadata_schema_version: legacy.metadata_schema_version,
        storage_format_version: 1,
        created_unix_seconds: legacy.created_unix_seconds,
        consistency: "offline-exclusive-lock".to_owned(),
        secrets_included: false,
        objects_encrypted: false,
        sealing_key_reference: None,
        components: vec![BackupComponent {
            name: "metadata".to_owned(),
            required: true,
            file_count: files.len() as u64,
            bytes: files.iter().map(|file| file.size).sum(),
        }],
        total_bytes: files.iter().map(|file| file.size).sum(),
        files,
    })
}

/// A version 1 backup carries only metadata, so its own required set is smaller.
///
/// Restoring one leaves payloads and system records for the operator, which the
/// restore report says out loud.
fn required_files_for(manifest: &BackupManifest) -> &'static [&'static str] {
    if manifest.backup_format_version == 1 {
        &["metadata/catalog.redb", "metadata/credentials.redb"]
    } else {
        &REQUIRED_FILES
    }
}

/// Where a manifest entry's bytes actually live inside the backup directory.
///
/// A version 1 backup keeps its metadata files at the top level. Its entries
/// are presented under `metadata/` so that everything downstream can reason in
/// one vocabulary, which means the one place that opens the file has to undo
/// that.
fn source_path(backup: &Path, manifest: &BackupManifest, file: &BackupFile) -> PathBuf {
    if manifest.backup_format_version == 1 {
        let name = file.path.strip_prefix("metadata/").unwrap_or(&file.path);
        backup.join(name)
    } else {
        backup.join(&file.path)
    }
}

fn unlisted_files(
    backup: &Path,
    listed: &std::collections::BTreeSet<&str>,
) -> Result<Vec<String>, BackupError> {
    let mut stray = Vec::new();
    for component in COMPONENTS {
        for path in walk_files(&backup.join(component))? {
            let relative = path
                .strip_prefix(backup)
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if !listed.contains(relative.as_str()) {
                stray.push(relative);
            }
            if stray.len() >= 100 {
                return Ok(stray);
            }
        }
    }
    Ok(stray)
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>, BackupError> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(BackupError::Io(error)),
        };
        for entry in entries {
            let entry = entry.map_err(BackupError::Io)?;
            let file_type = entry.file_type().map_err(BackupError::Io)?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                found.push(entry.path());
            }
        }
    }
    Ok(found)
}

fn measure_directory(path: &Path) -> Result<u64, BackupError> {
    let mut total = 0_u64;
    for file in walk_files(path)? {
        total = total.saturating_add(std::fs::metadata(&file).map_err(BackupError::Io)?.len());
    }
    Ok(total)
}

fn copy_tree(
    source: &Path,
    target: &Path,
    component: &str,
    files: &mut Vec<BackupFile>,
    summary: &mut BackupComponent,
) -> Result<(), BackupError> {
    for path in walk_files(source)? {
        let relative = path
            .strip_prefix(source)
            .map_err(|_| BackupError::UnsafePath(path.display().to_string()))?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        // Incomplete uploads are staging files that were never visible to any
        // client. Copying them would inflate the backup with bytes a restore
        // must then discard.
        if component == "objects" && relative.starts_with("tmp/") {
            continue;
        }
        let logical = format!("{component}/{relative}");
        if !safe_relative_path(&logical) {
            return Err(BackupError::UnsafePath(logical));
        }
        let destination = target.join(&relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(BackupError::Io)?;
        }
        let (size, sha256) = copy_with_checksum(&path, &destination)?;
        summary.file_count = summary.file_count.saturating_add(1);
        summary.bytes = summary.bytes.saturating_add(size);
        files.push(BackupFile {
            path: logical,
            size,
            sha256,
        });
    }
    Ok(())
}

/// Rejects anything that is not a plain relative path inside the backup.
///
/// A manifest is data from a file an operator may have received from elsewhere,
/// so a path in it is never allowed to escape the directory it describes.
fn safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 1024 || path.starts_with('/') {
        return false;
    }
    let mut components = path.split('/');
    let Some(first) = components.next() else {
        return false;
    };
    if !COMPONENTS.contains(&first) {
        return false;
    }
    let mut segments = 0;
    for segment in components {
        segments += 1;
        if segment.is_empty() || segment == "." || segment == ".." {
            return false;
        }
        if segment.contains('\\') || segment.contains('\0') {
            return false;
        }
    }
    segments > 0
}

fn copy_with_checksum(source: &Path, destination: &Path) -> Result<(u64, String), BackupError> {
    let mut source = File::open(source).map_err(BackupError::Io)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(BackupError::Io)?;
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    loop {
        let read = source.read(&mut buffer).map_err(BackupError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        target.write_all(&buffer[..read]).map_err(BackupError::Io)?;
        size = size.saturating_add(read as u64);
    }
    target.sync_all().map_err(BackupError::Io)?;
    Ok((size, hex::encode(hasher.finalize())))
}

fn checksum_file(path: &Path) -> Result<(u64, String), std::io::Error> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as u64);
    }
    Ok((size, hex::encode(hasher.finalize())))
}

fn write_durable(path: &Path, contents: &[u8]) -> Result<(), BackupError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(BackupError::Io)?;
    file.write_all(contents).map_err(BackupError::Io)?;
    file.sync_all().map_err(BackupError::Io)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), BackupError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(BackupError::Io)
}

fn sync_tree(root: &Path) -> Result<(), BackupError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(BackupError::Io)? {
            let entry = entry.map_err(BackupError::Io)?;
            if entry.file_type().map_err(BackupError::Io)?.is_dir() {
                pending.push(entry.path());
            }
        }
        sync_directory(&directory)?;
    }
    Ok(())
}

fn directory_has_entries(path: &Path) -> Result<bool, BackupError> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(BackupError::Io(error)),
    }
}

fn remove_directory_if_present(path: &Path) -> Result<(), BackupError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BackupError::Io(error)),
    }
}

/// Clears staging directories a previous interrupted restore left behind.
fn remove_stale_staging(data_directory: &Path) -> Result<(), BackupError> {
    for entry in std::fs::read_dir(data_directory).map_err(BackupError::Io)? {
        let entry = entry.map_err(BackupError::Io)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(".restore-") || name.starts_with("metadata.restore-") {
            remove_directory_if_present(&entry.path())?;
        }
    }
    Ok(())
}

/// Coordinated backup, verification, and restoration failures.
#[derive(Debug, Error)]
pub enum BackupError {
    /// Resolved configuration was invalid.
    #[error("invalid configuration: {0}")]
    Configuration(record_store_config::ConfigError),
    /// The data directory was never initialized by a server.
    #[error(
        "{} is not an initialized Record Store data directory; start the server once before backing it up",
        .0.display()
    )]
    NotInitialized(PathBuf),
    /// A server, or another maintenance command, holds the data directory.
    #[error("the data directory is in use by another Record Store process; stop the server first")]
    DataDirectoryInUse(#[source] std::io::Error),
    /// A restore into the data directory never finished.
    #[error(
        "{} holds a restore that never finished; run the restore again to completion before backing it up",
        .0.display()
    )]
    RestoreInProgress(PathBuf),
    /// The destination already holds a completed backup.
    #[error(
        "{} already holds a completed backup; choose a new destination rather than overwriting it",
        .0.display()
    )]
    DestinationHoldsBackup(PathBuf),
    /// The destination has content that is neither empty nor a failed attempt.
    #[error("{} is not empty", .0.display())]
    DestinationNotEmpty(PathBuf),
    /// The destination filesystem cannot hold the copy.
    #[error(
        "the destination has {available_bytes} bytes free but the backup needs about {required_bytes}"
    )]
    InsufficientSpace {
        /// Measured source size plus margin.
        required_bytes: u64,
        /// Free space at the destination.
        available_bytes: u64,
    },
    /// A component a restore cannot do without was not copied.
    #[error("the backup would be missing {0}, which a restore cannot do without")]
    MissingComponent(String),
    /// No manifest was found where one was expected.
    #[error(
        "{} does not contain a backup manifest; it is not a Record Store backup, or it never completed",
        .0.display()
    )]
    NoManifest(PathBuf),
    /// The manifest could not be understood.
    #[error("the backup manifest is invalid")]
    InvalidManifest,
    /// The backup failed verification.
    #[error("the backup cannot be restored: {}", .0.join("; "))]
    Unusable(Vec<String>),
    /// A file did not match its recorded checksum while being restored.
    #[error("{0} did not match its recorded checksum")]
    ChecksumMismatch(String),
    /// A manifest path pointed outside the backup.
    #[error("the backup manifest contains an unsafe path: {0}")]
    UnsafePath(String),
    /// The backup's catalog could not be read.
    #[error("the backup catalog could not be read: {0}")]
    Catalog(String),
    /// Filesystem failure.
    #[error("backup I/O failed: {0}")]
    Io(#[source] std::io::Error),
    /// Manifest encoding or decoding failed.
    #[error("backup manifest encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
}

impl From<MetadataBackupError> for BackupError {
    fn from(error: MetadataBackupError) -> Self {
        Self::Catalog(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest is a file that may have travelled. A path in it must never be
    /// able to write outside the directory it describes.
    #[test]
    fn manifest_paths_cannot_escape_the_backup() {
        for accepted in [
            "metadata/catalog.redb",
            "system/storage-format.json",
            "objects/ab/cd/abcdef",
        ] {
            assert!(safe_relative_path(accepted), "rejected {accepted}");
        }
        for refused in [
            "",
            "/etc/passwd",
            "metadata",
            "metadata/../../etc/passwd",
            "metadata/./catalog.redb",
            "../metadata/catalog.redb",
            "secrets/key.pem",
            "metadata/sub\\dir",
            "metadata/",
        ] {
            assert!(!safe_relative_path(refused), "accepted {refused}");
        }
    }

    /// The space check is what turns "an hour of copying, then ENOSPC" into a
    /// refusal that names both numbers.
    #[test]
    fn a_destination_too_small_for_the_copy_is_refused_with_both_numbers() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let error = check_destination_space(directory.path(), u64::MAX / 2)
            .expect_err("no filesystem holds this");
        let BackupError::InsufficientSpace {
            required_bytes,
            available_bytes,
        } = error
        else {
            panic!("the refusal has to be about space, not I/O: {error}");
        };
        assert!(required_bytes > available_bytes);
        assert!(
            error_message_names_both(required_bytes, available_bytes),
            "an operator needs both numbers to act"
        );

        // A copy that fits is not refused.
        check_destination_space(directory.path(), 1).expect("a small copy fits");
    }

    fn error_message_names_both(required: u64, available: u64) -> bool {
        let message = BackupError::InsufficientSpace {
            required_bytes: required,
            available_bytes: available,
        }
        .to_string();
        message.contains(&required.to_string()) && message.contains(&available.to_string())
    }

    /// The margin must not overflow on a source large enough to matter.
    #[test]
    fn the_space_margin_does_not_overflow_on_a_huge_source() {
        let directory = tempfile::tempdir().expect("temporary directory");
        assert!(check_destination_space(directory.path(), u64::MAX).is_err());
    }
}
