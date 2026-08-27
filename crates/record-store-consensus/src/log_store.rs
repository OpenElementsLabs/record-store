//! Durable consensus log.
//!
//! The error type on these signatures is the consensus library's own, so its
//! size is not ours to change; boxing it would break the trait contract.
#![expect(
    clippy::result_large_err,
    reason = "the consensus storage trait fixes the error type"
)]

//!
//! The log lives in its own database file, separate from the replicated state,
//! because the two have different durability and compaction lifecycles. Every
//! write is fsynced before it is reported as flushed: the consensus algorithm's
//! safety argument depends on that promise being real.

use std::{
    fmt::Debug,
    ops::{Bound, RangeBounds},
    path::Path,
    sync::Arc,
};

use openraft::{
    LogId, LogState, OptionalSend, RaftLogId, RaftLogReader, StorageError, StorageIOError, Vote,
    storage::{LogFlushed, RaftLogStorage},
};
use redb::{Database, ReadableTable, TableDefinition};

use crate::types::{ConsensusEntry, MemberId, OesTypeConfig};

const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("raft.entries.v1");
const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("raft.state.v1");

const VOTE: &str = "vote";
const COMMITTED: &str = "committed";
const LAST_PURGED: &str = "last_purged";

/// Failures raised while opening the durable consensus log.
#[derive(Debug, thiserror::Error)]
pub enum LogStoreError {
    /// The log directory could not be prepared.
    #[error("consensus log directory could not be prepared: {0}")]
    Directory(#[source] std::io::Error),
    /// The durable log could not be opened.
    #[error("consensus log could not be opened: {0}")]
    Open(String),
    /// A blocking log task could not finish.
    #[error("consensus log task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// A redb-backed consensus log.
#[derive(Clone)]
pub struct RedbLogStore {
    database: Arc<Database>,
}

impl Debug for RedbLogStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RedbLogStore")
    }
}

impl RedbLogStore {
    /// Opens or creates the durable log.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, LogStoreError> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(LogStoreError::Directory)?;
            }
            let database =
                Database::create(path).map_err(|error| LogStoreError::Open(error.to_string()))?;
            let write = database
                .begin_write()
                .map_err(|error| LogStoreError::Open(error.to_string()))?;
            write
                .open_table(ENTRIES)
                .map_err(|error| LogStoreError::Open(error.to_string()))?;
            write
                .open_table(STATE)
                .map_err(|error| LogStoreError::Open(error.to_string()))?;
            write
                .commit()
                .map_err(|error| LogStoreError::Open(error.to_string()))?;
            Ok(Self {
                database: Arc::new(database),
            })
        })
        .await?
    }

    fn read_state<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, StorageError<MemberId>> {
        let read = self
            .database
            .begin_read()
            .map_err(|error| StorageIOError::read(&io(error)))?;
        let table = read
            .open_table(STATE)
            .map_err(|error| StorageIOError::read(&io(error)))?;
        let encoded = table
            .get(key)
            .map_err(|error| StorageIOError::read(&io(error)))?
            .map(|value| value.value().to_vec());
        match encoded {
            Some(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).map_err(|error| StorageIOError::read(&io(error)))?,
            )),
            None => Ok(None),
        }
    }

    fn write_state<T: serde::Serialize>(
        &self,
        key: &'static str,
        value: &T,
    ) -> Result<(), StorageError<MemberId>> {
        let encoded =
            serde_json::to_vec(value).map_err(|error| StorageIOError::write(&io(error)))?;
        let write = self
            .database
            .begin_write()
            .map_err(|error| StorageIOError::write(&io(error)))?;
        {
            let mut table = write
                .open_table(STATE)
                .map_err(|error| StorageIOError::write(&io(error)))?;
            table
                .insert(key, encoded.as_slice())
                .map_err(|error| StorageIOError::write(&io(error)))?;
        }
        write
            .commit()
            .map_err(|error| StorageIOError::write(&io(error)))?;
        Ok(())
    }

    fn last_entry(&self) -> Result<Option<LogId<MemberId>>, StorageError<MemberId>> {
        let read = self
            .database
            .begin_read()
            .map_err(|error| StorageIOError::read_logs(&io(error)))?;
        let table = read
            .open_table(ENTRIES)
            .map_err(|error| StorageIOError::read_logs(&io(error)))?;
        let last = table
            .last()
            .map_err(|error| StorageIOError::read_logs(&io(error)))?;
        match last {
            Some((_, value)) => {
                let entry: ConsensusEntry = serde_json::from_slice(value.value())
                    .map_err(|error| StorageIOError::read_logs(&io(error)))?;
                Ok(Some(*entry.get_log_id()))
            }
            None => Ok(None),
        }
    }
}

fn io<E: std::fmt::Display>(error: E) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

impl RaftLogReader<OesTypeConfig> for RedbLogStore {
    async fn try_get_log_entries<RB>(
        &mut self,
        range: RB,
    ) -> Result<Vec<ConsensusEntry>, StorageError<MemberId>>
    where
        RB: RangeBounds<u64> + Clone + Debug + OptionalSend,
    {
        let start = match range.start_bound() {
            Bound::Included(index) => *index,
            Bound::Excluded(index) => index.saturating_add(1),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(index) => Some(*index),
            Bound::Excluded(index) => Some(index.saturating_sub(1)),
            Bound::Unbounded => None,
        };
        let read = self
            .database
            .begin_read()
            .map_err(|error| StorageIOError::read_logs(&io(error)))?;
        let table = read
            .open_table(ENTRIES)
            .map_err(|error| StorageIOError::read_logs(&io(error)))?;
        let iterator = match end {
            Some(end) => table.range(start..=end),
            None => table.range(start..),
        }
        .map_err(|error| StorageIOError::read_logs(&io(error)))?;
        let mut entries = Vec::new();
        for item in iterator {
            let (_, value) = item.map_err(|error| StorageIOError::read_logs(&io(error)))?;
            entries.push(
                serde_json::from_slice(value.value())
                    .map_err(|error| StorageIOError::read_logs(&io(error)))?,
            );
        }
        Ok(entries)
    }
}

impl RaftLogStorage<OesTypeConfig> for RedbLogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<OesTypeConfig>, StorageError<MemberId>> {
        let last_purged_log_id: Option<LogId<MemberId>> = self.read_state(LAST_PURGED)?;
        let last_log_id = self.last_entry()?.or(last_purged_log_id);
        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<MemberId>) -> Result<(), StorageError<MemberId>> {
        self.write_state(VOTE, vote)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<MemberId>>, StorageError<MemberId>> {
        self.read_state(VOTE)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<MemberId>>,
    ) -> Result<(), StorageError<MemberId>> {
        self.write_state(COMMITTED, &committed)
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<MemberId>>, StorageError<MemberId>> {
        Ok(self.read_state(COMMITTED)?.flatten())
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<OesTypeConfig>,
    ) -> Result<(), StorageError<MemberId>>
    where
        I: IntoIterator<Item = ConsensusEntry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let database = Arc::clone(&self.database);
        let encoded: Vec<(u64, Vec<u8>)> = entries
            .into_iter()
            .map(|entry| {
                let index = entry.log_id.index;
                serde_json::to_vec(&entry)
                    .map(|bytes| (index, bytes))
                    .map_err(|error| StorageIOError::write_logs(&io(error)))
            })
            .collect::<Result<_, _>>()?;
        let outcome = tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
            let write = database.begin_write().map_err(io)?;
            {
                let mut table = write.open_table(ENTRIES).map_err(io)?;
                for (index, bytes) in &encoded {
                    table.insert(*index, bytes.as_slice()).map_err(io)?;
                }
            }
            // redb commits durably, which is exactly the guarantee the
            // consensus algorithm needs before the flush callback fires.
            write.commit().map_err(io)
        })
        .await
        .map_err(|error| StorageIOError::write_logs(&io(error)))?;
        let failed = outcome.is_err();
        callback.log_io_completed(outcome);
        if failed {
            return Err(StorageIOError::write_logs(&io("log append failed")).into());
        }
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<MemberId>) -> Result<(), StorageError<MemberId>> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
            let write = database.begin_write().map_err(io)?;
            {
                let mut table = write.open_table(ENTRIES).map_err(io)?;
                table.retain(|index, _| index < log_id.index).map_err(io)?;
            }
            write.commit().map_err(io)
        })
        .await
        .map_err(|error| StorageIOError::write_logs(&io(error)))?
        .map_err(|error| StorageIOError::write_logs(&io(error)))?;
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<MemberId>) -> Result<(), StorageError<MemberId>> {
        self.write_state(LAST_PURGED, &Some(log_id))?;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
            let write = database.begin_write().map_err(io)?;
            {
                let mut table = write.open_table(ENTRIES).map_err(io)?;
                table.retain(|index, _| index > log_id.index).map_err(io)?;
            }
            write.commit().map_err(io)
        })
        .await
        .map_err(|error| StorageIOError::write_logs(&io(error)))?
        .map_err(|error| StorageIOError::write_logs(&io(error)))?;
        Ok(())
    }
}
