//! Durable security audit records, intentionally separate from tracing logs.

use std::{collections::BTreeMap, fmt::Display, path::Path, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use record_store_core::AuditEventId;
use redb::{Database, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const EVENTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("audit_events.v1");

/// Stable audit result category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    Success,
    Denied,
    Failure,
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
    pub next: Option<(DateTime<Utc>, AuditEventId)>,
}

#[async_trait]
pub trait AuditRepository: Send + Sync {
    async fn append(&self, event: &AuditEvent) -> Result<(), AuditError>;
    async fn query(&self, query: AuditQuery) -> Result<AuditPage, AuditError>;
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
}

#[derive(Clone)]
pub struct RedbAuditRepository {
    database: Arc<Database>,
}

impl RedbAuditRepository {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AuditError> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(AuditError::Directory)?;
            }
            let database = Database::create(path).map_err(|error| backend("open", error))?;
            let write = database
                .begin_write()
                .map_err(|error| backend("initialize", error))?;
            {
                write
                    .open_table(EVENTS)
                    .map_err(|error| backend("initialize events", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit initialization", error))?;
            Ok(Self {
                database: Arc::new(database),
            })
        })
        .await?
    }
}

#[async_trait]
impl AuditRepository for RedbAuditRepository {
    async fn append(&self, event: &AuditEvent) -> Result<(), AuditError> {
        let database = Arc::clone(&self.database);
        let event = event.clone();
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin append", error))?;
            {
                let mut table = write
                    .open_table(EVENTS)
                    .map_err(|error| backend("open events", error))?;
                let bytes = serde_json::to_vec(&event)?;
                table
                    .insert(event_key(&event).as_slice(), bytes.as_slice())
                    .map_err(|error| backend("append event", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit event", error))
        })
        .await?
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
            for (scanned, entry) in table
                .range(start.as_slice()..end.as_slice())
                .map_err(|error| backend("range events", error))?
                .enumerate()
            {
                if scanned >= 100_000 || events.len() > query.limit {
                    break;
                }
                let (_, value) = entry.map_err(|error| backend("read event", error))?;
                let event: AuditEvent = serde_json::from_slice(value.value())?;
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
            } else {
                None
            };
            Ok(AuditPage { events, next })
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
