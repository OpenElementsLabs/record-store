//! The replicated cluster state machine.
#![expect(
    clippy::result_large_err,
    reason = "the consensus storage trait fixes the error type"
)]

//!
//! The state machine is the existing durable OES catalogs, applied through
//! deterministic commands. Reusing the catalogs rather than reimplementing them
//! is what keeps versioning, multipart, and quota semantics identical between a
//! standalone node and a cluster.
//!
//! Each entry is applied in a single database transaction that also advances the
//! applied log position, so a crash can neither apply an entry twice nor lose
//! one.

use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

use oes_cluster::{ClusterCatalog, ClusterCommand, apply_command_tx as apply_cluster_tx};
use oes_metadata::{
    MetadataCommand, RedbMetadataRepository, apply_command_tx as apply_metadata_tx,
};
use openraft::{
    EntryPayload, LogId, OptionalSend, RaftSnapshotBuilder, Snapshot, SnapshotMeta, StorageError,
    StorageIOError, storage::RaftStateMachine,
};
use redb::{Database, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{
    command::{
        ClusterWrite, ClusterWriteResponse, classify_cluster_error, classify_metadata_error,
        is_durable_cluster_failure, is_durable_metadata_failure,
    },
    types::{ConsensusEntry, ConsensusMembership, MemberId, MemberNode, OesTypeConfig},
};

const APPLIED: TableDefinition<&str, &[u8]> = TableDefinition::new("raft.applied.v1");
const LAST_APPLIED: &str = "last_applied";
const LAST_MEMBERSHIP: &str = "last_membership";

/// Durable snapshot layout version.
const SNAPSHOT_FORMAT_VERSION: u32 = 1;

fn io<E: std::fmt::Display>(error: E) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

/// A complete replicated state snapshot.
#[derive(Debug, Serialize, Deserialize)]
struct SnapshotDocument {
    snapshot_format_version: u32,
    last_applied: Option<LogId<MemberId>>,
    last_membership: ConsensusMembership,
    metadata: Vec<oes_metadata::MetadataEntry>,
    cluster: Vec<oes_cluster::CatalogEntry>,
}

/// Failures raised while opening the replicated state machine.
#[derive(Debug, thiserror::Error)]
pub enum StateMachineError {
    /// The state directory could not be prepared.
    #[error("consensus state directory could not be prepared: {0}")]
    Directory(#[source] std::io::Error),
    /// The durable state could not be opened.
    #[error("consensus state could not be opened: {0}")]
    Open(String),
    /// The object catalog could not be opened.
    #[error("object catalog could not be opened: {0}")]
    Metadata(#[from] oes_metadata::MetadataError),
    /// The cluster catalog could not be opened.
    #[error("cluster catalog could not be opened: {0}")]
    Cluster(#[from] oes_cluster::ClusterCatalogError),
    /// A blocking state task could not finish.
    #[error("consensus state task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// The durable replicated state shared by consensus and local readers.
pub struct ReplicatedState {
    database: Arc<Database>,
    metadata: RedbMetadataRepository,
    cluster: ClusterCatalog,
    snapshot_directory: PathBuf,
    snapshot_sequence: Mutex<u64>,
}

impl ReplicatedState {
    /// Opens the replicated state in one shared database.
    pub async fn open(
        state_path: impl AsRef<Path>,
        snapshot_directory: impl AsRef<Path>,
    ) -> Result<Arc<Self>, StateMachineError> {
        let state_path = state_path.as_ref().to_path_buf();
        let snapshot_directory = snapshot_directory.as_ref().to_path_buf();
        let database = tokio::task::spawn_blocking({
            let state_path = state_path.clone();
            let snapshot_directory = snapshot_directory.clone();
            move || -> Result<Arc<Database>, StateMachineError> {
                if let Some(parent) = state_path.parent() {
                    std::fs::create_dir_all(parent).map_err(StateMachineError::Directory)?;
                }
                std::fs::create_dir_all(&snapshot_directory)
                    .map_err(StateMachineError::Directory)?;
                let database = Database::create(state_path)
                    .map_err(|error| StateMachineError::Open(error.to_string()))?;
                let write = database
                    .begin_write()
                    .map_err(|error| StateMachineError::Open(error.to_string()))?;
                write
                    .open_table(APPLIED)
                    .map_err(|error| StateMachineError::Open(error.to_string()))?;
                write
                    .commit()
                    .map_err(|error| StateMachineError::Open(error.to_string()))?;
                Ok(Arc::new(database))
            }
        })
        .await??;
        let metadata = RedbMetadataRepository::from_database(Arc::clone(&database))?;
        let cluster = ClusterCatalog::from_database(Arc::clone(&database))?;
        Ok(Arc::new(Self {
            database,
            metadata,
            cluster,
            snapshot_directory,
            snapshot_sequence: Mutex::new(0),
        }))
    }

    /// Returns the replicated object catalog for local reads.
    #[must_use]
    pub const fn metadata(&self) -> &RedbMetadataRepository {
        &self.metadata
    }

    /// Returns the replicated cluster catalog for local reads.
    #[must_use]
    pub const fn cluster(&self) -> &ClusterCatalog {
        &self.cluster
    }

    /// Returns the shared database handle.
    #[must_use]
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.database)
    }

    /// Returns the applied log position and membership recorded durably.
    pub async fn applied_state(
        &self,
    ) -> Result<(Option<LogId<MemberId>>, ConsensusMembership), StateMachineError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| StateMachineError::Open(error.to_string()))?;
            read_applied_state(&read).map_err(|error| StateMachineError::Open(error.to_string()))
        })
        .await?
    }
}

fn read_applied_state(
    read: &redb::ReadTransaction,
) -> Result<(Option<LogId<MemberId>>, ConsensusMembership), std::io::Error> {
    let table = read.open_table(APPLIED).map_err(io)?;
    let last_applied = table
        .get(LAST_APPLIED)
        .map_err(io)?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()
        .map_err(io)?
        .flatten();
    let last_membership = table
        .get(LAST_MEMBERSHIP)
        .map_err(io)?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()
        .map_err(io)?
        .unwrap_or_default();
    Ok((last_applied, last_membership))
}

fn record_applied(write: &WriteTransaction, log_id: LogId<MemberId>) -> Result<(), std::io::Error> {
    let encoded = serde_json::to_vec(&Some(log_id)).map_err(io)?;
    let mut table = write.open_table(APPLIED).map_err(io)?;
    table.insert(LAST_APPLIED, encoded.as_slice()).map_err(io)?;
    Ok(())
}

fn record_membership(
    write: &WriteTransaction,
    membership: &ConsensusMembership,
) -> Result<(), std::io::Error> {
    let encoded = serde_json::to_vec(membership).map_err(io)?;
    let mut table = write.open_table(APPLIED).map_err(io)?;
    table
        .insert(LAST_MEMBERSHIP, encoded.as_slice())
        .map_err(io)?;
    Ok(())
}

/// Applies one replicated write inside a transaction.
///
/// Application-level rejections are returned as values, not errors, so that
/// every member reaches the same conclusion and consensus keeps running.
fn apply_write(
    write: &WriteTransaction,
    command: ClusterWrite,
) -> Result<ClusterWriteResponse, DurableFailure> {
    match command {
        ClusterWrite::Noop => Ok(ClusterWriteResponse::Noop),
        ClusterWrite::Metadata(command) => apply_metadata_command(write, *command),
        ClusterWrite::Cluster(command) => apply_cluster_command(write, *command),
        ClusterWrite::Batch(commands) => {
            let mut responses = Vec::with_capacity(commands.len());
            for command in commands {
                let response = apply_write(write, command)?;
                if let ClusterWriteResponse::Rejected(rejection) = response {
                    // The whole batch must be atomic, so one rejection rejects
                    // the entire write and the transaction is rolled back.
                    return Ok(ClusterWriteResponse::Rejected(rejection));
                }
                responses.push(response);
            }
            Ok(ClusterWriteResponse::Batch(responses))
        }
    }
}

fn apply_metadata_command(
    write: &WriteTransaction,
    command: MetadataCommand,
) -> Result<ClusterWriteResponse, DurableFailure> {
    match apply_metadata_tx(write, command) {
        Ok(outcome) => Ok(ClusterWriteResponse::Metadata(Box::new(outcome))),
        Err(error) if is_durable_metadata_failure(&error) => Err(DurableFailure(error.to_string())),
        Err(error) => Ok(ClusterWriteResponse::Rejected(classify_metadata_error(
            &error,
        ))),
    }
}

fn apply_cluster_command(
    write: &WriteTransaction,
    command: ClusterCommand,
) -> Result<ClusterWriteResponse, DurableFailure> {
    match apply_cluster_tx(write, command) {
        Ok(outcome) => Ok(ClusterWriteResponse::Cluster(Box::new(outcome))),
        Err(error) if is_durable_cluster_failure(&error) => Err(DurableFailure(error.to_string())),
        Err(error) => Ok(ClusterWriteResponse::Rejected(classify_cluster_error(
            &error,
        ))),
    }
}

/// A failure of durable storage itself, which must stop this member.
#[derive(Debug)]
struct DurableFailure(String);

/// The consensus state machine handle.
#[derive(Clone)]
pub struct StateMachineStore {
    state: Arc<ReplicatedState>,
}

impl StateMachineStore {
    /// Wraps shared replicated state as a consensus state machine.
    #[must_use]
    pub const fn new(state: Arc<ReplicatedState>) -> Self {
        Self { state }
    }

    /// Returns the shared replicated state.
    #[must_use]
    pub fn state(&self) -> Arc<ReplicatedState> {
        Arc::clone(&self.state)
    }

    fn snapshot_path(&self, snapshot_id: &str) -> PathBuf {
        self.state
            .snapshot_directory
            .join(format!("{snapshot_id}.snapshot"))
    }

    fn current_snapshot_pointer(&self) -> PathBuf {
        self.state.snapshot_directory.join("current.json")
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotPointer {
    snapshot_id: String,
    last_applied: Option<LogId<MemberId>>,
    last_membership: ConsensusMembership,
}

impl RaftSnapshotBuilder<OesTypeConfig> for StateMachineStore {
    async fn build_snapshot(&mut self) -> Result<Snapshot<OesTypeConfig>, StorageError<MemberId>> {
        let sequence = {
            let mut guard = self.state.snapshot_sequence.lock().await;
            *guard = guard.saturating_add(1);
            *guard
        };
        let database = self.state.database();
        let document =
            tokio::task::spawn_blocking(move || -> Result<SnapshotDocument, std::io::Error> {
                // A read transaction gives a consistent point-in-time view without
                // blocking command application.
                let read = database.begin_read().map_err(io)?;
                let (last_applied, last_membership) = read_applied_state(&read)?;
                let metadata = oes_metadata::export_tx(&read).map_err(io)?;
                let cluster = oes_cluster::export_tx(&read).map_err(io)?;
                Ok(SnapshotDocument {
                    snapshot_format_version: SNAPSHOT_FORMAT_VERSION,
                    last_applied,
                    last_membership,
                    metadata,
                    cluster,
                })
            })
            .await
            .map_err(|error| StorageIOError::read_state_machine(&io(error)))?
            .map_err(|error| StorageIOError::read_state_machine(&io(error)))?;

        let snapshot_id = match document.last_applied {
            Some(log_id) => format!("{}-{}-{sequence}", log_id.leader_id, log_id.index),
            None => format!("empty-{sequence}"),
        };
        let meta = SnapshotMeta {
            last_log_id: document.last_applied,
            last_membership: document.last_membership.clone(),
            snapshot_id: snapshot_id.clone(),
        };
        let encoded = serde_json::to_vec(&document)
            .map_err(|error| StorageIOError::read_state_machine(&io(error)))?;
        let path = self.snapshot_path(&snapshot_id);
        let pointer_path = self.current_snapshot_pointer();
        let pointer = SnapshotPointer {
            snapshot_id,
            last_applied: document.last_applied,
            last_membership: document.last_membership,
        };
        let directory = self.state.snapshot_directory.clone();
        let payload = encoded.clone();
        tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
            persist_snapshot(&directory, &path, &pointer_path, &pointer, &payload)
        })
        .await
        .map_err(|error| StorageIOError::write_snapshot(None, &io(error)))?
        .map_err(|error| StorageIOError::write_snapshot(None, &io(error)))?;

        info!(bytes = encoded.len(), "built consensus metadata snapshot");
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(encoded)),
        })
    }
}

fn persist_snapshot(
    directory: &Path,
    path: &Path,
    pointer_path: &Path,
    pointer: &SnapshotPointer,
    payload: &[u8],
) -> Result<(), std::io::Error> {
    use std::io::Write;

    std::fs::create_dir_all(directory)?;
    let temporary = path.with_extension("snapshot.tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(payload)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;

    let encoded = serde_json::to_vec(pointer).map_err(io)?;
    let pointer_temporary = pointer_path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&pointer_temporary)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&pointer_temporary, pointer_path)?;

    // Older snapshots are no longer referenced once the pointer is published.
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let candidate = entry.path();
            if candidate == path || candidate == pointer_path {
                continue;
            }
            let is_snapshot = candidate
                .extension()
                .is_some_and(|extension| extension == "snapshot");
            if is_snapshot {
                let _ = std::fs::remove_file(candidate);
            }
        }
    }
    Ok(())
}

impl RaftStateMachine<OesTypeConfig> for StateMachineStore {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<MemberId>>, ConsensusMembership), StorageError<MemberId>> {
        self.state
            .applied_state()
            .await
            .map_err(|error| StorageIOError::read_state_machine(&io(error)).into())
    }

    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> Result<Vec<ClusterWriteResponse>, StorageError<MemberId>>
    where
        I: IntoIterator<Item = ConsensusEntry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let entries: Vec<ConsensusEntry> = entries.into_iter().collect();
        let database = self.state.database();
        tokio::task::spawn_blocking(move || {
            let mut responses = Vec::with_capacity(entries.len());
            for entry in entries {
                let log_id = entry.log_id;
                match entry.payload {
                    EntryPayload::Blank => {
                        commit_applied_only(&database, log_id).map_err(|error| {
                            StorageError::from(StorageIOError::<MemberId>::apply(
                                log_id,
                                &io(error),
                            ))
                        })?;
                        responses.push(ClusterWriteResponse::Noop);
                    }
                    EntryPayload::Membership(membership) => {
                        let stored = ConsensusMembership::new(Some(log_id), membership);
                        commit_membership(&database, log_id, &stored).map_err(|error| {
                            StorageError::from(StorageIOError::<MemberId>::apply(
                                log_id,
                                &io(error),
                            ))
                        })?;
                        responses.push(ClusterWriteResponse::Noop);
                    }
                    EntryPayload::Normal(command) => {
                        let name = command.name();
                        let write = database.begin_write().map_err(|error| {
                            StorageError::from(StorageIOError::<MemberId>::apply(
                                log_id,
                                &io(error),
                            ))
                        })?;
                        record_applied(&write, log_id).map_err(|error| {
                            StorageError::from(StorageIOError::<MemberId>::apply(
                                log_id,
                                &io(error),
                            ))
                        })?;
                        match apply_write(&write, command) {
                            Ok(ClusterWriteResponse::Rejected(rejection)) => {
                                // Roll the whole entry back, then advance the
                                // applied position so the entry is not retried.
                                drop(write);
                                commit_applied_only(&database, log_id).map_err(|error| {
                                    StorageError::from(StorageIOError::<MemberId>::apply(
                                        log_id,
                                        &io(error),
                                    ))
                                })?;
                                warn!(
                                    command = name,
                                    kind = ?rejection.kind,
                                    message = %rejection.message,
                                    "replicated command rejected"
                                );
                                responses.push(ClusterWriteResponse::Rejected(rejection));
                            }
                            Ok(response) => {
                                write.commit().map_err(|error| {
                                    StorageError::from(StorageIOError::<MemberId>::apply(
                                        log_id,
                                        &io(error),
                                    ))
                                })?;
                                responses.push(response);
                            }
                            Err(DurableFailure(reason)) => {
                                drop(write);
                                return Err(
                                    StorageIOError::<MemberId>::apply(log_id, &io(reason)).into()
                                );
                            }
                        }
                    }
                }
            }
            Ok(responses)
        })
        .await
        .map_err(|error| StorageIOError::write_state_machine(&io(error)))?
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<MemberId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<MemberId, MemberNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<MemberId>> {
        let payload = snapshot.into_inner();
        let document: SnapshotDocument = serde_json::from_slice(&payload)
            .map_err(|error| StorageIOError::read_snapshot(Some(meta.signature()), &io(error)))?;
        if document.snapshot_format_version != SNAPSHOT_FORMAT_VERSION {
            return Err(StorageIOError::read_snapshot(
                Some(meta.signature()),
                &io(format!(
                    "snapshot format version {} is not supported (expected {SNAPSHOT_FORMAT_VERSION})",
                    document.snapshot_format_version
                )),
            )
            .into());
        }
        let database = self.state.database();
        let membership = meta.last_membership.clone();
        let last_log_id = meta.last_log_id;
        tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
            let write = database.begin_write().map_err(io)?;
            oes_metadata::import_tx(&write, &document.metadata).map_err(io)?;
            oes_cluster::import_tx(&write, &document.cluster).map_err(io)?;
            if let Some(log_id) = last_log_id {
                record_applied(&write, log_id)?;
            }
            record_membership(&write, &membership)?;
            write.commit().map_err(io)
        })
        .await
        .map_err(|error| StorageIOError::write_snapshot(Some(meta.signature()), &io(error)))?
        .map_err(|error| StorageIOError::write_snapshot(Some(meta.signature()), &io(error)))?;

        let path = self.snapshot_path(&meta.snapshot_id);
        let pointer_path = self.current_snapshot_pointer();
        let pointer = SnapshotPointer {
            snapshot_id: meta.snapshot_id.clone(),
            last_applied: meta.last_log_id,
            last_membership: meta.last_membership.clone(),
        };
        let directory = self.state.snapshot_directory.clone();
        tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
            persist_snapshot(&directory, &path, &pointer_path, &pointer, &payload)
        })
        .await
        .map_err(|error| StorageIOError::write_snapshot(Some(meta.signature()), &io(error)))?
        .map_err(|error| StorageIOError::write_snapshot(Some(meta.signature()), &io(error)))?;
        info!(snapshot = %meta.snapshot_id, "installed consensus metadata snapshot");
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<OesTypeConfig>>, StorageError<MemberId>> {
        let pointer_path = self.current_snapshot_pointer();
        let directory = self.state.snapshot_directory.clone();
        let loaded = tokio::task::spawn_blocking(
            move || -> Result<Option<(SnapshotPointer, Vec<u8>)>, std::io::Error> {
                let encoded = match std::fs::read(&pointer_path) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error),
                };
                let pointer: SnapshotPointer = serde_json::from_slice(&encoded).map_err(io)?;
                let path = directory.join(format!("{}.snapshot", pointer.snapshot_id));
                match std::fs::read(path) {
                    Ok(payload) => Ok(Some((pointer, payload))),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error),
                }
            },
        )
        .await
        .map_err(|error| StorageIOError::read_snapshot(None, &io(error)))?
        .map_err(|error| StorageIOError::read_snapshot(None, &io(error)))?;

        Ok(loaded.map(|(pointer, payload)| Snapshot {
            meta: SnapshotMeta {
                last_log_id: pointer.last_applied,
                last_membership: pointer.last_membership,
                snapshot_id: pointer.snapshot_id,
            },
            snapshot: Box::new(Cursor::new(payload)),
        }))
    }
}

fn commit_applied_only(
    database: &Arc<Database>,
    log_id: LogId<MemberId>,
) -> Result<(), std::io::Error> {
    let write = database.begin_write().map_err(io)?;
    record_applied(&write, log_id)?;
    write.commit().map_err(io)
}

fn commit_membership(
    database: &Arc<Database>,
    log_id: LogId<MemberId>,
    membership: &ConsensusMembership,
) -> Result<(), std::io::Error> {
    let write = database.begin_write().map_err(io)?;
    record_applied(&write, log_id)?;
    record_membership(&write, membership)?;
    write.commit().map_err(io)
}
