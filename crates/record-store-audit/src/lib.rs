//! Durable security audit records, intentionally separate from tracing logs.

use std::{collections::BTreeMap, fmt::Display, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use record_store_core::{AuditEventId, DEFAULT_CACHE_BYTES, open_database_with_cache};
use redb::{Database, ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod canonical;
pub mod chain;
pub mod checkpoint;
pub mod export;
pub mod intent;
pub mod merkle;

const EVENTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("audit_events.v1");
/// Insertion order, which is what the hash chain is built over.
///
/// The primary table is keyed by timestamp, and a timestamp is not an order:
/// two records can share one, a caller can supply a backdated one, and a clock
/// can move. The chain has to be walked in the order records were appended, so
/// that order is indexed explicitly rather than inferred.
const SEQUENCE_INDEX: TableDefinition<u64, &[u8]> = TableDefinition::new("audit_sequence.v1");
/// The chain's head: the next sequence to assign and the last record's hash.
const CHAIN_STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("audit_chain_state.v1");
const NEXT_SEQUENCE: &str = "next_sequence";
const HEAD_HASH: &str = "head_hash";

/// Entries one query may walk before it gives up and hands back a cursor.
///
/// A filter is applied per scanned record rather than through an index, so a
/// sparse filter over a long range can scan a great deal to return very little.
/// The cap bounds that work; reporting it is what stops a truncated scan from
/// reading as "there is nothing more".
const MAXIMUM_SCANNED_ENTRIES: usize = 100_000;

/// Stable audit result category.
///
/// [`AuditResult::Attempted`] is not an outcome but the absence of one. It is
/// written *before* a mutation is attempted, so that a crash between the
/// mutation committing and its outcome being recorded leaves a durable record
/// naming who asked for what, rather than nothing at all. A completion record
/// carrying the same request identifier supersedes it; an intent with no
/// completion is an operation whose outcome this server cannot account for,
/// which is a fact an operator needs rather than one to hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    Success,
    Denied,
    Failure,
    Attempted,
}

/// A secret-free durable security event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub event_id: AuditEventId,
    pub timestamp: DateTime<Utc>,
    pub request_id: Option<String>,
    pub principal: String,
    pub credential_id: Option<uuid::Uuid>,
    pub source_ip: Option<String>,
    pub operation: String,
    pub resource: String,
    pub result: AuditResult,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Bounded audit query. All filters are exact except resource prefix.
///
/// Filtering narrows a bounded scan over the time range rather than consulting
/// an index, so every filter costs the same: a comparison per scanned event.
/// The scan is capped, and a caller that needs fewer results should narrow the
/// time range rather than relying on a filter to make the scan cheaper.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub principal: Option<String>,
    pub operation: Option<String>,
    pub resource_prefix: Option<String>,
    pub result: Option<AuditResult>,
    /// Exact client address, as recorded on the event.
    pub source_ip: Option<String>,
    /// Exact request identifier, for tracing one operation end to end.
    pub request_id: Option<String>,
    pub after: Option<(DateTime<Utc>, AuditEventId)>,
    pub limit: usize,
}

/// Bounded audit page.
#[derive(Debug, Clone)]
pub struct AuditPage {
    pub events: Vec<AuditEvent>,
    /// Where to resume. Present whenever the range was not walked to its end,
    /// including when the scan budget ran out before the page filled.
    pub next: Option<(DateTime<Utc>, AuditEventId)>,
    /// Whether the scan stopped on its own budget rather than on the page
    /// limit or the end of the range.
    ///
    /// A caller that ignores this and sees fewer results than it asked for
    /// would conclude the range holds nothing more. With a sparse filter over a
    /// long range that conclusion is wrong, and wrong in the direction that
    /// hides activity, so the fact is reported rather than inferred.
    pub scan_truncated: bool,
}

/// What walking a span of the hash chain established.
///
/// Verification of a span starting after the beginning takes the stored hash of
/// the preceding record as its starting point. That record's own integrity is
/// established by the span before it, not by this one — which is why a full
/// verification starts at sequence zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerification {
    /// First sequence this walk covered.
    pub from_sequence: u64,
    /// Records examined.
    pub checked: u64,
    /// Records that hash to what they claim and link to their predecessor.
    pub intact: u64,
    /// Records written before this deployment maintained a chain.
    pub unchained: u64,
    /// Where to resume, when the walk stopped on its limit.
    pub next_sequence: Option<u64>,
    /// Highest sequence the log holds, or `None` when nothing is chained yet.
    pub head_sequence: Option<u64>,
    /// Every record that failed, up to a bound.
    pub problems: Vec<ChainProblem>,
}

impl ChainVerification {
    /// Whether every examined record verified.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        self.problems.is_empty()
    }
}

/// One record that did not verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainProblem {
    /// Position of the failing record.
    pub sequence: u64,
    /// What recomputing it established.
    pub verdict: &'static str,
}

/// Problems one verification reports before it stops collecting them.
const MAXIMUM_CHAIN_PROBLEMS: usize = 100;

#[async_trait]
pub trait AuditRepository: Send + Sync {
    /// Appends one record durably, linking it to the record before it.
    ///
    /// The call returns only once the record is committed, which is what lets a
    /// caller write an intent *before* a mutation and rely on it surviving a
    /// crash during that mutation.
    async fn append(&self, event: &AuditEvent) -> Result<(), AuditError>;
    async fn query(&self, query: AuditQuery) -> Result<AuditPage, AuditError>;
    /// Walks the hash chain from `from_sequence`, examining at most `limit`.
    async fn verify_chain(
        &self,
        from_sequence: u64,
        limit: usize,
    ) -> Result<ChainVerification, AuditError>;
    async fn check_ready(&self) -> Result<(), AuditError>;
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("failed to prepare audit directory: {0}")]
    Directory(#[source] std::io::Error),
    #[error("audit encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("audit operation '{operation}' failed: {reason}")]
    Database {
        operation: &'static str,
        reason: String,
    },
    #[error("audit task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("audit query limit must be between 1 and 1000")]
    InvalidLimit,
    #[error("a digest must be 32 bytes of hex")]
    InvalidDigest,
    #[error("an export format must be json or csv")]
    InvalidExportFormat,
    #[error("an export range must end after it begins")]
    InvalidExportRange,
    #[error("a checkpoint must cover at least one record")]
    EmptyCheckpoint,
    #[error(
        "a checkpoint over sequences {from}..={to} covers {covered} records, not the \
         {leaf_count} its leaf count claims"
    )]
    CheckpointLeafCount {
        from: u64,
        to: u64,
        covered: u64,
        leaf_count: u64,
    },
}

#[derive(Clone)]
pub struct RedbAuditRepository {
    database: Arc<Database>,
    /// The one thread that writes the chain, shared by every clone.
    writer: Arc<Writer>,
}

/// Owns the writer thread. Dropping the last repository handle closes the
/// queue and waits for the thread, so the database is closed -- and can be
/// reopened -- as soon as the repository is gone.
struct Writer {
    queue: Option<std::sync::mpsc::Sender<PendingAppend>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.queue.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// An event waiting to be appended, and where to report the durable outcome.
type PendingAppend = (
    AuditEvent,
    tokio::sync::oneshot::Sender<Result<(), AuditError>>,
);

/// Appends committed by one transaction at most.
const MAXIMUM_BATCH: usize = 512;

/// Starts the thread that owns every write to the chain.
///
/// Every request is audited and every change twice (`attempted`, then its
/// outcome), and each append used to be its own durable commit through redb's
/// single writer, so under write load a read's audit record queued behind every
/// change's two fsyncs. The writer takes whatever has queued, appends it in
/// arrival order inside one transaction -- sequence numbers and hash links are
/// assigned exactly as before -- commits once, and only then answers each
/// caller. A caller is still told its record is durable only after it is.
fn start_writer(database: Arc<Database>) -> Result<Writer, AuditError> {
    let (sender, receiver) = std::sync::mpsc::channel::<PendingAppend>();
    let thread = std::thread::Builder::new()
        .name("audit-writer".into())
        .spawn(move || {
            while let Ok(first) = receiver.recv() {
                let mut batch = vec![first];
                while batch.len() < MAXIMUM_BATCH {
                    match receiver.try_recv() {
                        Ok(pending) => batch.push(pending),
                        Err(_) => break,
                    }
                }
                commit_batch(&database, batch);
            }
        })
        .map_err(|error| backend("start audit writer", error))?;
    Ok(Writer {
        queue: Some(sender),
        thread: Some(thread),
    })
}

/// Commits a batch in one transaction. If any append in it fails, the batch is
/// abandoned whole and every append retried in a transaction of its own, so
/// one bad record never fails its neighbours and never lands half a batch.
fn commit_batch(database: &Database, batch: Vec<PendingAppend>) {
    let together = (|| {
        let write = database
            .begin_write()
            .map_err(|error| backend("begin append", error))?;
        for (event, _) in &batch {
            append_in(&write, event)?;
        }
        write
            .commit()
            .map_err(|error| backend("commit event", error))
    })();
    match together {
        Ok(()) => {
            for (_, reply) in batch {
                let _ = reply.send(Ok(()));
            }
        }
        Err(error) if batch.len() == 1 => {
            if let Some((_, reply)) = batch.into_iter().next() {
                let _ = reply.send(Err(error));
            }
        }
        Err(_) => {
            for (event, reply) in batch {
                let alone = (|| {
                    let write = database
                        .begin_write()
                        .map_err(|error| backend("begin append", error))?;
                    append_in(&write, &event)?;
                    write
                        .commit()
                        .map_err(|error| backend("commit event", error))
                })();
                let _ = reply.send(alone);
            }
        }
    }
}

impl RedbAuditRepository {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AuditError> {
        Self::open_with_cache(path, DEFAULT_CACHE_BYTES).await
    }

    /// Opens the trail with at most `cache_bytes` of page cache. The trail is
    /// append-mostly and grows with every request, so the pages worth keeping
    /// are the few near its head, not the whole file.
    pub async fn open_with_cache(
        path: impl AsRef<Path>,
        cache_bytes: usize,
    ) -> Result<Self, AuditError> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(AuditError::Directory)?;
            }
            let database = open_database_with_cache(path, cache_bytes)
                .map_err(|error| backend("open", error))?;
            let write = database
                .begin_write()
                .map_err(|error| backend("initialize", error))?;
            {
                write
                    .open_table(EVENTS)
                    .map_err(|error| backend("initialize events", error))?;
                write
                    .open_table(SEQUENCE_INDEX)
                    .map_err(|error| backend("initialize sequence index", error))?;
                write
                    .open_table(CHAIN_STATE)
                    .map_err(|error| backend("initialize chain state", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit initialization", error))?;
            let database = Arc::new(database);
            let writer = Arc::new(start_writer(Arc::clone(&database))?);
            Ok(Self { database, writer })
        })
        .await?
    }
}

/// Appends one event inside `write`: one sequence number, one hash link to its
/// predecessor, one index entry, and the new head. redb allows one writer at a
/// time and every append goes through the writer thread, so appends cannot
/// interleave into a chain with a gap, a repeat, or a hash over the wrong
/// predecessor.
fn append_in(write: &redb::WriteTransaction, event: &AuditEvent) -> Result<(), AuditError> {
    let key = event_key(event);
    {
        let mut state = write
            .open_table(CHAIN_STATE)
            .map_err(|error| backend("open chain state", error))?;
        let (sequence, previous) = read_chain_head(&state)?;
        let record = chain::AuditRecord {
            sequence,
            chain: Some(chain::ChainLinks {
                previous_hash: previous,
                record_hash: chain::hash_event(sequence, &previous, event),
            }),
            event: event.clone(),
        };
        let bytes = serde_json::to_vec(&record)?;
        {
            let mut events = write
                .open_table(EVENTS)
                .map_err(|error| backend("open events", error))?;
            let replaced = events
                .insert(key.as_slice(), bytes.as_slice())
                .map_err(|error| backend("append event", error))?;
            // Unreachable by construction — every caller allocates a
            // fresh identifier — and refused rather than assumed,
            // because appending the same event twice would leave two
            // sequence positions pointing at one stored record and a
            // chain that no longer verifies for reasons nobody could
            // trace back to here.
            if replaced.is_some() {
                return Err(backend(
                    "append event",
                    "an audit record with this identifier and timestamp already exists",
                ));
            }
        }
        {
            let mut index = write
                .open_table(SEQUENCE_INDEX)
                .map_err(|error| backend("open sequence index", error))?;
            index
                .insert(sequence, key.as_slice())
                .map_err(|error| backend("index event", error))?;
        }
        let head = record
            .chain
            .ok_or_else(|| backend("append event", "a new record is always chained"))?
            .record_hash;
        state
            .insert(
                NEXT_SEQUENCE,
                sequence.saturating_add(1).to_be_bytes().as_slice(),
            )
            .map_err(|error| backend("advance audit sequence", error))?;
        state
            .insert(HEAD_HASH, head.as_slice())
            .map_err(|error| backend("advance audit chain head", error))?;
    }
    Ok(())
}

/// Decodes a stored value, accepting records written before the chain existed.
///
/// A catalog upgraded from an earlier release holds bare events. They stay
/// queryable and they are never presented as chained: rewriting them to look
/// chained would be manufacturing evidence they never had.
fn decode_stored(value: &[u8]) -> Result<chain::AuditRecord, AuditError> {
    if let Ok(record) = serde_json::from_slice::<chain::AuditRecord>(value) {
        return Ok(record);
    }
    let event: AuditEvent = serde_json::from_slice(value)?;
    Ok(chain::AuditRecord {
        sequence: 0,
        chain: None,
        event,
    })
}

/// Reads the chain head: the next sequence to assign and the last hash.
fn read_chain_head(
    table: &impl redb::ReadableTable<&'static str, &'static [u8]>,
) -> Result<(u64, chain::Digest32), AuditError> {
    let next = table
        .get(NEXT_SEQUENCE)
        .map_err(|error| backend("read audit sequence", error))?
        .map(|value| {
            <[u8; 8]>::try_from(value.value())
                .map(u64::from_be_bytes)
                .map_err(|_| backend("decode audit sequence", "sequence is not eight bytes"))
        })
        .transpose()?
        .unwrap_or(0);
    let head = table
        .get(HEAD_HASH)
        .map_err(|error| backend("read audit chain head", error))?
        .map(|value| {
            chain::Digest32::try_from(value.value())
                .map_err(|_| backend("decode audit chain head", "head is not thirty-two bytes"))
        })
        .transpose()?
        .unwrap_or_else(chain::genesis_hash);
    Ok((next, head))
}

#[async_trait]
impl AuditRepository for RedbAuditRepository {
    async fn append(&self, event: &AuditEvent) -> Result<(), AuditError> {
        let (reply, outcome) = tokio::sync::oneshot::channel();
        self.writer
            .queue
            .as_ref()
            .ok_or_else(|| backend("append event", "the audit writer has stopped"))?
            .send((event.clone(), reply))
            .map_err(|_| backend("append event", "the audit writer has stopped"))?;
        outcome
            .await
            .map_err(|_| backend("append event", "the audit writer stopped before committing"))?
    }

    async fn query(&self, query: AuditQuery) -> Result<AuditPage, AuditError> {
        if !(1..=1_000).contains(&query.limit) {
            return Err(AuditError::InvalidLimit);
        }
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| backend("begin query", error))?;
            let table = read
                .open_table(EVENTS)
                .map_err(|error| backend("open events", error))?;
            let mut start = query.after.map_or_else(
                || query.since.map_or_else(|| vec![0; 24], time_prefix),
                |(time, id)| {
                    let mut key = event_key_parts(time, id);
                    key.push(0);
                    key
                },
            );
            if start.is_empty() {
                start = vec![0; 24];
            }
            let end = query.until.map_or_else(
                || vec![u8::MAX; 25],
                |time| {
                    let mut key = time_prefix(time);
                    key.extend_from_slice(&[u8::MAX; 16]);
                    key
                },
            );
            let mut events = Vec::with_capacity(query.limit + 1);
            let mut scan_truncated = false;
            // Where the scan actually reached, which is not where the page
            // ends: a filter can reject every record the budget was spent on.
            let mut last_scanned: Option<(DateTime<Utc>, AuditEventId)> = None;
            for (scanned, entry) in table
                .range(start.as_slice()..end.as_slice())
                .map_err(|error| backend("range events", error))?
                .enumerate()
            {
                if events.len() > query.limit {
                    break;
                }
                if scanned >= MAXIMUM_SCANNED_ENTRIES {
                    scan_truncated = true;
                    break;
                }
                let (_, value) = entry.map_err(|error| backend("read event", error))?;
                let event = decode_stored(value.value())?.event;
                last_scanned = Some((event.timestamp, event.event_id));
                if query
                    .principal
                    .as_ref()
                    .is_some_and(|value| value != &event.principal)
                    || query
                        .operation
                        .as_ref()
                        .is_some_and(|value| value != &event.operation)
                    || query
                        .resource_prefix
                        .as_ref()
                        .is_some_and(|value| !event.resource.starts_with(value))
                    || query.result.is_some_and(|value| value != event.result)
                    // Both of these are exact matches: an address or a request
                    // id is looked up because it is already known, not browsed.
                    || query
                        .source_ip
                        .as_ref()
                        .is_some_and(|value| Some(value) != event.source_ip.as_ref())
                    || query
                        .request_id
                        .as_ref()
                        .is_some_and(|value| Some(value) != event.request_id.as_ref())
                {
                    continue;
                }
                events.push(event);
            }
            let next = if events.len() > query.limit {
                events.pop();
                events.last().map(|event| (event.timestamp, event.event_id))
            } else if scan_truncated {
                // The cursor is the last record the scan *looked at*, not the
                // last it returned. Resuming from the last returned record
                // would re-walk everything the filter already rejected, and
                // returning no cursor at all would tell the caller the range
                // is exhausted when it is not.
                last_scanned
            } else {
                None
            };
            Ok(AuditPage {
                events,
                next,
                scan_truncated,
            })
        })
        .await?
    }

    async fn verify_chain(
        &self,
        from_sequence: u64,
        limit: usize,
    ) -> Result<ChainVerification, AuditError> {
        if !(1..=10_000).contains(&limit) {
            return Err(AuditError::InvalidLimit);
        }
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| backend("begin chain verification", error))?;
            let index = read
                .open_table(SEQUENCE_INDEX)
                .map_err(|error| backend("open sequence index", error))?;
            let events = read
                .open_table(EVENTS)
                .map_err(|error| backend("open events", error))?;
            let state = read
                .open_table(CHAIN_STATE)
                .map_err(|error| backend("open chain state", error))?;
            let (next_to_assign, _) = read_chain_head(&state)?;
            let head_sequence = next_to_assign.checked_sub(1);

            let load = |sequence: u64| -> Result<Option<chain::AuditRecord>, AuditError> {
                let Some(key) = index
                    .get(sequence)
                    .map_err(|error| backend("read sequence index", error))?
                else {
                    return Ok(None);
                };
                let Some(value) = events
                    .get(key.value())
                    .map_err(|error| backend("read indexed event", error))?
                else {
                    return Ok(None);
                };
                decode_stored(value.value()).map(Some)
            };

            // Starting mid-chain takes the predecessor's stored hash on trust.
            // That is not a gap in the argument, it is where this span's
            // argument begins: the span before it is what establishes that
            // record. A walk from zero takes nothing on trust but the genesis
            // constant.
            let mut previous = match from_sequence.checked_sub(1) {
                None => chain::genesis_hash(),
                Some(earlier) => match load(earlier)? {
                    Some(record) => record
                        .chain
                        .map_or_else(chain::genesis_hash, |links| links.record_hash),
                    None => chain::genesis_hash(),
                },
            };

            let mut verification = ChainVerification {
                from_sequence,
                checked: 0,
                intact: 0,
                unchained: 0,
                next_sequence: None,
                head_sequence,
                problems: Vec::new(),
            };
            let mut sequence = from_sequence;
            while verification.checked < limit as u64 {
                // The sequence is gapless by construction, so the walk runs to
                // the head rather than stopping at the first empty position.
                // Stopping there is precisely how a deletion would hide: the
                // record carrying the broken link is the one that is gone.
                let Some(head) = head_sequence else { break };
                if sequence > head {
                    break;
                }
                verification.checked += 1;
                let verdict = match load(sequence)? {
                    Some(record) => {
                        let verdict = record.verify_against(&previous);
                        // The walk continues from the hash the record carries
                        // even when that record failed, so one edited record is
                        // reported once rather than invalidating the rest.
                        previous = record.chain.map_or(previous, |links| links.record_hash);
                        verdict
                    }
                    None => chain::RecordVerdict::Missing,
                };
                match verdict {
                    chain::RecordVerdict::Intact => verification.intact += 1,
                    chain::RecordVerdict::Unchained => verification.unchained += 1,
                    verdict => {
                        if verification.problems.len() < MAXIMUM_CHAIN_PROBLEMS {
                            verification.problems.push(ChainProblem {
                                sequence,
                                verdict: verdict.label(),
                            });
                        }
                    }
                }
                sequence = sequence.saturating_add(1);
            }
            if head_sequence.is_some_and(|head| sequence <= head) {
                verification.next_sequence = Some(sequence);
            }
            Ok(verification)
        })
        .await?
    }

    async fn check_ready(&self) -> Result<(), AuditError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| backend("readiness", error))?;
            read.open_table(EVENTS)
                .map_err(|error| backend("readiness table", error))?;
            Ok(())
        })
        .await?
    }
}

fn event_key(event: &AuditEvent) -> Vec<u8> {
    event_key_parts(event.timestamp, event.event_id)
}
fn event_key_parts(time: DateTime<Utc>, id: AuditEventId) -> Vec<u8> {
    let mut key = time_prefix(time);
    key.extend_from_slice(id.as_uuid().as_bytes());
    key
}
fn time_prefix(time: DateTime<Utc>) -> Vec<u8> {
    (time.timestamp_micros().max(0) as u64)
        .to_be_bytes()
        .to_vec()
}
fn backend(operation: &'static str, error: impl Display) -> AuditError {
    AuditError::Database {
        operation,
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Group commit batches concurrent appends into one transaction.
    /// However they are batched, the chain must come out exactly as if they had
    /// been appended one at a time: every event present once, sequences gapless,
    /// every link verifying -- and all of it still there after a reopen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_appends_form_one_gapless_verifying_chain() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("audit.redb");
        let repository = RedbAuditRepository::open(&path).await.expect("repository");
        let mut tasks = Vec::new();
        for index in 0..400_u32 {
            let repository = repository.clone();
            tasks.push(tokio::spawn(async move {
                let event = AuditEvent {
                    event_id: AuditEventId::new(),
                    timestamp: Utc::now(),
                    request_id: Some(format!("request-{index}")),
                    principal: "service:test".into(),
                    credential_id: None,
                    source_ip: None,
                    operation: "object.created".into(),
                    resource: format!("bucket:test/{index}"),
                    result: AuditResult::Success,
                    metadata: BTreeMap::new(),
                };
                repository.append(&event).await.expect("append");
                event.event_id
            }));
        }
        let mut appended = std::collections::BTreeSet::new();
        for task in tasks {
            appended.insert(task.await.expect("task"));
        }
        drop(repository);

        let repository = RedbAuditRepository::open(&path).await.expect("reopen");
        let verification = repository.verify_chain(0, 10_000).await.expect("verify");
        assert_eq!(verification.checked, 400);
        assert_eq!(verification.intact, 400, "{verification:?}");
        let mut stored = std::collections::BTreeSet::new();
        let mut after = None;
        loop {
            let page = repository
                .query(AuditQuery {
                    limit: 1_000,
                    after,
                    ..AuditQuery::default()
                })
                .await
                .expect("query");
            stored.extend(page.events.iter().map(|event| event.event_id));
            match page.next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        assert_eq!(
            stored, appended,
            "every appended event is stored exactly once"
        );
    }

    #[tokio::test]
    async fn events_survive_restart_and_queries_are_bounded() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("audit.redb");
        let event = AuditEvent {
            event_id: AuditEventId::new(),
            timestamp: Utc::now(),
            request_id: Some("request".into()),
            principal: "service:test".into(),
            credential_id: None,
            source_ip: None,
            operation: "object.created".into(),
            resource: "bucket:test/key".into(),
            result: AuditResult::Success,
            metadata: BTreeMap::new(),
        };
        {
            let repository = RedbAuditRepository::open(&path).await.expect("repository");
            repository.append(&event).await.expect("append");
        }
        let repository = RedbAuditRepository::open(&path).await.expect("reopen");
        let page = repository
            .query(AuditQuery {
                limit: 10,
                ..AuditQuery::default()
            })
            .await
            .expect("query");
        assert_eq!(page.events, vec![event]);
    }

    #[tokio::test]
    async fn source_ip_and_request_id_narrow_a_query() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let repository = RedbAuditRepository::open(directory.path().join("audit.redb"))
            .await
            .expect("open audit");

        let base = Utc::now();
        for (index, (ip, request)) in [
            (Some("10.0.0.1"), Some("req-a")),
            (Some("10.0.0.2"), Some("req-a")),
            (Some("10.0.0.1"), Some("req-b")),
            (None, None),
        ]
        .into_iter()
        .enumerate()
        {
            repository
                .append(&AuditEvent {
                    event_id: AuditEventId::new(),
                    timestamp: base + chrono::Duration::seconds(index as i64),
                    request_id: request.map(str::to_owned),
                    principal: "ingest".into(),
                    credential_id: None,
                    source_ip: ip.map(str::to_owned),
                    operation: "PutObject".into(),
                    resource: "uploads/a".into(),
                    result: AuditResult::Success,
                    metadata: BTreeMap::new(),
                })
                .await
                .expect("append");
        }

        let by_address = repository
            .query(AuditQuery {
                source_ip: Some("10.0.0.1".into()),
                limit: 50,
                ..AuditQuery::default()
            })
            .await
            .expect("query by address");
        assert_eq!(by_address.events.len(), 2);
        assert!(
            by_address
                .events
                .iter()
                .all(|event| event.source_ip.as_deref() == Some("10.0.0.1"))
        );

        let by_request = repository
            .query(AuditQuery {
                request_id: Some("req-a".into()),
                limit: 50,
                ..AuditQuery::default()
            })
            .await
            .expect("query by request");
        assert_eq!(by_request.events.len(), 2);

        // Both together are an intersection, not a union.
        let both = repository
            .query(AuditQuery {
                source_ip: Some("10.0.0.1".into()),
                request_id: Some("req-a".into()),
                limit: 50,
                ..AuditQuery::default()
            })
            .await
            .expect("query by both");
        assert_eq!(both.events.len(), 1);

        // An event that recorded neither field must not match a filter on it.
        let missing = repository
            .query(AuditQuery {
                source_ip: Some("10.0.0.9".into()),
                limit: 50,
                ..AuditQuery::default()
            })
            .await
            .expect("query for an absent address");
        assert!(missing.events.is_empty());
    }
}
