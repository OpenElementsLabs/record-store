//! Durable storage events, signed webhook subscriptions, and bounded delivery.

use std::{collections::BTreeMap, net::IpAddr, path::Path, sync::Arc, time::Duration};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, OsRng, rand_core::RngCore},
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use oes_core::{EventId, VersionId, WebhookId};
use redb::{Database, ReadableTable, TableDefinition};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use tokio::{net::lookup_host, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

const EVENTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("storage_events_v1");
const SUBSCRIPTIONS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("webhook_subscriptions_v1");
const PENDING: TableDefinition<&[u8], &[u8]> = TableDefinition::new("webhook_pending_v1");
const DELIVERY_LOGS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("webhook_delivery_logs_v1");
/// Time-ordered index over stored events.
///
/// Event identifiers are random, so the primary table cannot answer "the most
/// recent events" without a full scan. This index makes that a bounded range
/// read instead.
const EVENTS_BY_TIME: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("storage_events_by_time_v1");
const MAX_ERROR_SUMMARY: usize = 512;

/// Builds an index key that sorts by time and then by identifier.
///
/// The sign bit is flipped so byte ordering matches numeric ordering for
/// timestamps on both sides of the epoch.
fn event_time_key(time: DateTime<Utc>, id: EventId) -> Vec<u8> {
    let mut key = Vec::with_capacity(24);
    key.extend_from_slice(&((time.timestamp_millis() as u64) ^ (1_u64 << 63)).to_be_bytes());
    key.extend_from_slice(id.as_uuid().as_bytes());
    key
}

/// Builds the exclusive upper bound for a time filter.
fn upper_time_key(time: DateTime<Utc>) -> Vec<u8> {
    let mut key = event_time_key(time, EventId::from_uuid(Uuid::max()));
    key.push(0);
    key
}

/// Stable storage-event names intended for integrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageEventType {
    #[serde(rename = "bucket.created")]
    BucketCreated,
    #[serde(rename = "bucket.deleted")]
    BucketDeleted,
    #[serde(rename = "object.created")]
    ObjectCreated,
    #[serde(rename = "object.updated")]
    ObjectUpdated,
    #[serde(rename = "object.deleted")]
    ObjectDeleted,
    #[serde(rename = "object.restored")]
    ObjectRestored,
    #[serde(rename = "multipart.completed")]
    MultipartCompleted,
    #[serde(rename = "multipart.aborted")]
    MultipartAborted,
}

impl StorageEventType {
    fn header_value(self) -> &'static str {
        match self {
            Self::BucketCreated => "bucket.created",
            Self::BucketDeleted => "bucket.deleted",
            Self::ObjectCreated => "object.created",
            Self::ObjectUpdated => "object.updated",
            Self::ObjectDeleted => "object.deleted",
            Self::ObjectRestored => "object.restored",
            Self::MultipartCompleted => "multipart.completed",
            Self::MultipartAborted => "multipart.aborted",
        }
    }
}

/// Filesystem-independent event envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageEvent {
    pub id: EventId,
    #[serde(rename = "type")]
    pub event_type: StorageEventType,
    pub time: DateTime<Utc>,
    pub bucket: String,
    pub object: Option<String>,
    pub version_id: Option<VersionId>,
    pub size: Option<u64>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl StorageEvent {
    #[must_use]
    pub fn new(event_type: StorageEventType, bucket: impl Into<String>) -> Self {
        Self {
            id: EventId::new(),
            event_type,
            time: Utc::now(),
            bucket: bucket.into(),
            object: None,
            version_id: None,
            size: None,
            metadata: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn object(
        mut self,
        key: impl Into<String>,
        version_id: Option<VersionId>,
        size: Option<u64>,
    ) -> Self {
        self.object = Some(key.into());
        self.version_id = version_id;
        self.size = size;
        self
    }
}

/// Persisted webhook routing configuration. The signing secret is deliberately absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookSubscription {
    pub id: WebhookId,
    pub target_url: String,
    pub event_types: Vec<StorageEventType>,
    pub bucket_filter: Option<String>,
    pub object_prefix_filter: Option<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Subscription returned at creation time with its one-time signing secret.
#[derive(Debug, Serialize)]
pub struct CreatedWebhook {
    pub subscription: WebhookSubscription,
    pub signing_secret: String,
}

/// Bounded delivery-attempt record without arbitrary response bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDeliveryLog {
    pub id: Uuid,
    pub event_id: EventId,
    pub webhook_id: WebhookId,
    pub attempt: u32,
    pub http_status: Option<u16>,
    pub timestamp: DateTime<Utc>,
    pub success: bool,
    pub error_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedSecret {
    format_version: u8,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredWebhook {
    subscription: WebhookSubscription,
    encrypted_secret: EncryptedSecret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingDelivery {
    event_id: EventId,
    webhook_id: WebhookId,
    attempts: u32,
    next_attempt_at: DateTime<Utc>,
}

/// Validated subscription input.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateWebhookRequest {
    pub target_url: String,
    pub event_types: Vec<StorageEventType>,
    pub bucket_filter: Option<String>,
    pub object_prefix_filter: Option<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

const fn enabled_by_default() -> bool {
    true
}

/// Delivery and SSRF controls for one node.
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub allow_http: bool,
    pub allow_private_networks: bool,
    pub request_timeout: Duration,
    pub maximum_attempts: u32,
    pub poll_interval: Duration,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            allow_http: false,
            allow_private_networks: false,
            request_timeout: Duration::from_secs(10),
            maximum_attempts: 6,
            poll_interval: Duration::from_secs(2),
        }
    }
}

/// Storage-event interface used by request services and future exporters.
/// Bounded storage-event query, newest first.
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    /// Inclusive lower time bound.
    pub since: Option<DateTime<Utc>>,
    /// Inclusive upper time bound.
    pub until: Option<DateTime<Utc>>,
    /// Exact bucket name filter.
    pub bucket: Option<String>,
    /// Exact event-type filter.
    pub event_type: Option<StorageEventType>,
    /// Object-key prefix filter.
    pub object_prefix: Option<String>,
    /// Cursor from a previous page.
    pub after: Option<(DateTime<Utc>, EventId)>,
    /// Maximum events returned.
    pub limit: usize,
}

/// A bounded page of storage events.
#[derive(Debug, Clone, Default)]
pub struct EventPage {
    /// Events ordered newest first.
    pub events: Vec<StorageEvent>,
    /// Cursor for the next page, when more events match.
    pub next: Option<(DateTime<Utc>, EventId)>,
}

#[async_trait]
pub trait EventRepository: Send + Sync {
    async fn publish(&self, event: &StorageEvent) -> Result<(), EventError>;
    /// Returns recent storage events, newest first.
    ///
    /// Storage events describe what happened to data. They are deliberately kept
    /// separate from the audit trail, which describes who asked for it.
    async fn list_events(&self, query: EventQuery) -> Result<EventPage, EventError>;
    async fn create_webhook(
        &self,
        request: CreateWebhookRequest,
    ) -> Result<CreatedWebhook, EventError>;
    async fn list_webhooks(&self) -> Result<Vec<WebhookSubscription>, EventError>;
    async fn set_webhook_enabled(
        &self,
        id: WebhookId,
        enabled: bool,
    ) -> Result<WebhookSubscription, EventError>;
    async fn delete_webhook(&self, id: WebhookId) -> Result<(), EventError>;
    async fn list_delivery_logs(&self, limit: usize)
    -> Result<Vec<WebhookDeliveryLog>, EventError>;
    async fn deliver_due(&self, limit: usize) -> Result<usize, EventError>;
    async fn check_ready(&self) -> Result<(), EventError>;
}

/// Redb-backed event outbox and webhook delivery repository.
pub struct RedbEventRepository {
    database: Arc<Database>,
    cipher: Option<Aes256Gcm>,
    config: WebhookConfig,
}

struct ValidatedTarget {
    url: Url,
    host: String,
    addresses: Vec<std::net::SocketAddr>,
}

impl RedbEventRepository {
    pub async fn open(
        path: impl AsRef<Path>,
        master_key: Option<&[u8]>,
        config: WebhookConfig,
    ) -> Result<Self, EventError> {
        if let Some(parent) = path.as_ref().parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(EventError::Directory)?;
        }
        let path = path.as_ref().to_owned();
        let database =
            tokio::task::spawn_blocking(move || Database::create(path).map_err(database_error))
                .await??;
        let database = Arc::new(database);
        let db = Arc::clone(&database);
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(database_error)?;
            {
                write.open_table(EVENTS).map_err(database_error)?;
            }
            {
                write.open_table(SUBSCRIPTIONS).map_err(database_error)?;
            }
            {
                write.open_table(PENDING).map_err(database_error)?;
            }
            {
                write.open_table(DELIVERY_LOGS).map_err(database_error)?;
            }
            {
                write.open_table(EVENTS_BY_TIME).map_err(database_error)?;
            }
            write.commit().map_err(database_error)
        })
        .await??;
        if master_key.is_none() {
            let db = Arc::clone(&database);
            let has_subscriptions = tokio::task::spawn_blocking(move || {
                let read = db.begin_read().map_err(database_error)?;
                let table = read.open_table(SUBSCRIPTIONS).map_err(database_error)?;
                Ok::<_, EventError>(table.iter().map_err(database_error)?.next().is_some())
            })
            .await??;
            if has_subscriptions {
                return Err(EventError::MasterKeyRequired);
            }
        }
        let cipher = master_key.map(derive_cipher).transpose()?;
        Ok(Self {
            database,
            cipher,
            config,
        })
    }

    async fn deliver_one(&self, pending: PendingDelivery) -> Result<(), EventError> {
        let (event, subscription) = self
            .load_delivery(pending.event_id, pending.webhook_id)
            .await?;
        let Some(subscription) = subscription else {
            return self
                .remove_pending(pending.event_id, pending.webhook_id)
                .await;
        };
        if !subscription.subscription.enabled {
            return self
                .remove_pending(pending.event_id, pending.webhook_id)
                .await;
        }
        let event = event.ok_or(EventError::InconsistentState)?;
        let target = self
            .validate_target(&subscription.subscription.target_url)
            .await?;
        let client = Client::builder()
            .timeout(self.config.request_timeout)
            .redirect(Policy::none())
            .resolve_to_addrs(&target.host, &target.addresses)
            .build()
            .map_err(EventError::HttpClient)?;
        let secret = self.decrypt_secret(&subscription.encrypted_secret)?;
        let payload = serde_json::to_vec(&event)?;
        let mut signer =
            <Hmac<Sha256> as Mac>::new_from_slice(&secret).map_err(|_| EventError::Crypto)?;
        signer.update(&payload);
        let signature = format!("sha256={}", hex::encode(signer.finalize().into_bytes()));
        let attempt = pending.attempts.saturating_add(1);
        let response = client
            .post(target.url)
            .header("content-type", "application/json")
            .header("x-oes-event-id", event.id.to_string())
            .header("x-oes-event-type", event.event_type.header_value())
            .header("x-oes-event-time", event.time.to_rfc3339())
            .header("x-oes-signature", signature)
            .body(payload)
            .send()
            .await;
        let (status, success, summary) = match response {
            Ok(response) => {
                let status = response.status();
                (
                    Some(status.as_u16()),
                    status.is_success(),
                    (!status.is_success())
                        .then(|| format!("webhook returned HTTP {}", status.as_u16())),
                )
            }
            Err(error) => (None, false, Some(error.to_string())),
        };
        self.record_attempt(pending, attempt, status, success, summary)
            .await
    }

    async fn validate_target(&self, target: &str) -> Result<ValidatedTarget, EventError> {
        let url = Url::parse(target).map_err(|_| EventError::InvalidTarget)?;
        let allowed_scheme =
            url.scheme() == "https" || (self.config.allow_http && url.scheme() == "http");
        if !allowed_scheme
            || url.username() != ""
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(EventError::InvalidTarget);
        }
        let host = url.host_str().ok_or(EventError::InvalidTarget)?.to_owned();
        let port = url
            .port_or_known_default()
            .ok_or(EventError::InvalidTarget)?;
        let addresses: Vec<_> = lookup_host((host.as_str(), port))
            .await
            .map_err(EventError::ResolveTarget)?
            .collect();
        if addresses.is_empty() {
            return Err(EventError::InvalidTarget);
        }
        if !self.config.allow_private_networks {
            for address in &addresses {
                if !is_public_ip(address.ip()) {
                    return Err(EventError::PrivateTarget);
                }
            }
        }
        Ok(ValidatedTarget {
            url,
            host,
            addresses,
        })
    }

    fn encrypt_secret(&self, secret: &[u8]) -> Result<EncryptedSecret, EventError> {
        let cipher = self.cipher.as_ref().ok_or(EventError::MasterKeyRequired)?;
        let mut nonce = [0_u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), secret)
            .map_err(|_| EventError::Crypto)?;
        Ok(EncryptedSecret {
            format_version: 1,
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }

    fn decrypt_secret(&self, encrypted: &EncryptedSecret) -> Result<Vec<u8>, EventError> {
        if encrypted.format_version != 1 || encrypted.nonce.len() != 12 {
            return Err(EventError::UnsupportedSecretFormat);
        }
        let cipher = self.cipher.as_ref().ok_or(EventError::MasterKeyRequired)?;
        cipher
            .decrypt(
                Nonce::from_slice(&encrypted.nonce),
                encrypted.ciphertext.as_ref(),
            )
            .map_err(|_| EventError::Crypto)
    }

    async fn load_delivery(
        &self,
        event_id: EventId,
        webhook_id: WebhookId,
    ) -> Result<(Option<StorageEvent>, Option<StoredWebhook>), EventError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(database_error)?;
            let events = read.open_table(EVENTS).map_err(database_error)?;
            let subscriptions = read.open_table(SUBSCRIPTIONS).map_err(database_error)?;
            Ok((
                decode(
                    events
                        .get(event_id.as_uuid().as_bytes().as_slice())
                        .map_err(database_error)?,
                )?,
                decode(
                    subscriptions
                        .get(webhook_id.as_uuid().as_bytes().as_slice())
                        .map_err(database_error)?,
                )?,
            ))
        })
        .await?
    }

    async fn remove_pending(
        &self,
        event_id: EventId,
        webhook_id: WebhookId,
    ) -> Result<(), EventError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(database_error)?;
            {
                write
                    .open_table(PENDING)
                    .map_err(database_error)?
                    .remove(pending_key(event_id, webhook_id).as_slice())
                    .map_err(database_error)?;
            }
            write.commit().map_err(database_error)
        })
        .await?
    }

    async fn record_attempt(
        &self,
        pending: PendingDelivery,
        attempt: u32,
        status: Option<u16>,
        success: bool,
        summary: Option<String>,
    ) -> Result<(), EventError> {
        let db = Arc::clone(&self.database);
        let maximum_attempts = self.config.maximum_attempts;
        tokio::task::spawn_blocking(move || {
            let now = Utc::now();
            let log = WebhookDeliveryLog {
                id: Uuid::new_v4(),
                event_id: pending.event_id,
                webhook_id: pending.webhook_id,
                attempt,
                http_status: status,
                timestamp: now,
                success,
                error_summary: summary.map(|value| bounded(&value, MAX_ERROR_SUMMARY)),
            };
            let write = db.begin_write().map_err(database_error)?;
            {
                let mut logs = write.open_table(DELIVERY_LOGS).map_err(database_error)?;
                let bytes = serde_json::to_vec(&log)?;
                logs.insert(ordered_key(now, log.id).as_slice(), bytes.as_slice())
                    .map_err(database_error)?;
            }
            {
                let mut queue = write.open_table(PENDING).map_err(database_error)?;
                let key = pending_key(pending.event_id, pending.webhook_id);
                if success || attempt >= maximum_attempts {
                    queue.remove(key.as_slice()).map_err(database_error)?;
                } else {
                    let delay_seconds = 2_u64.saturating_pow(attempt.min(10));
                    let jitter = u64::from(pending.event_id.as_uuid().as_bytes()[0]) % 3;
                    let updated = PendingDelivery {
                        attempts: attempt,
                        next_attempt_at: now
                            + chrono::Duration::seconds(
                                i64::try_from(delay_seconds + jitter).unwrap_or(i64::MAX),
                            ),
                        ..pending
                    };
                    let bytes = serde_json::to_vec(&updated)?;
                    queue
                        .insert(key.as_slice(), bytes.as_slice())
                        .map_err(database_error)?;
                }
            }
            write.commit().map_err(database_error)
        })
        .await?
    }
}

#[async_trait]
impl EventRepository for RedbEventRepository {
    async fn publish(&self, event: &StorageEvent) -> Result<(), EventError> {
        let db = Arc::clone(&self.database);
        let event = event.clone();
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(database_error)?;
            let subscriptions = write.open_table(SUBSCRIPTIONS).map_err(database_error)?;
            let mut matching = Vec::new();
            for item in subscriptions.iter().map_err(database_error)? {
                let (_, value) = item.map_err(database_error)?;
                let subscription: StoredWebhook = serde_json::from_slice(value.value())?;
                let subscription = subscription.subscription;
                if subscription.enabled
                    && subscription.event_types.contains(&event.event_type)
                    && subscription
                        .bucket_filter
                        .as_ref()
                        .is_none_or(|bucket| bucket == &event.bucket)
                    && subscription
                        .object_prefix_filter
                        .as_ref()
                        .is_none_or(|prefix| {
                            event
                                .object
                                .as_ref()
                                .is_some_and(|key| key.starts_with(prefix))
                        })
                {
                    matching.push(subscription.id);
                }
            }
            drop(subscriptions);
            {
                let mut events = write.open_table(EVENTS).map_err(database_error)?;
                let bytes = serde_json::to_vec(&event)?;
                events
                    .insert(event.id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
                    .map_err(database_error)?;
            }
            {
                let mut index = write.open_table(EVENTS_BY_TIME).map_err(database_error)?;
                index
                    .insert(
                        event_time_key(event.time, event.id).as_slice(),
                        event.id.as_uuid().as_bytes().as_slice(),
                    )
                    .map_err(database_error)?;
            }
            {
                let mut queue = write.open_table(PENDING).map_err(database_error)?;
                for webhook_id in matching {
                    let pending = PendingDelivery {
                        event_id: event.id,
                        webhook_id,
                        attempts: 0,
                        next_attempt_at: Utc::now(),
                    };
                    let bytes = serde_json::to_vec(&pending)?;
                    queue
                        .insert(
                            pending_key(event.id, webhook_id).as_slice(),
                            bytes.as_slice(),
                        )
                        .map_err(database_error)?;
                }
            }
            write.commit().map_err(database_error)
        })
        .await?
    }

    async fn list_events(&self, query: EventQuery) -> Result<EventPage, EventError> {
        let limit = query.limit.clamp(1, 1_000);
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(database_error)?;
            let index = read.open_table(EVENTS_BY_TIME).map_err(database_error)?;
            let events = read.open_table(EVENTS).map_err(database_error)?;
            // The index is scanned in reverse so the newest events come first
            // without loading the whole history.
            let upper = match query.after {
                Some((time, id)) => event_time_key(time, id),
                None => query
                    .until
                    .map_or_else(|| vec![u8::MAX; 24], upper_time_key),
            };
            let lower = query.since.map_or_else(Vec::new, |since| {
                event_time_key(since, EventId::from_uuid(Uuid::nil()))
            });
            let mut page = EventPage::default();
            for item in index
                .range(lower.as_slice()..upper.as_slice())
                .map_err(database_error)?
                .rev()
            {
                let (_, value) = item.map_err(database_error)?;
                let Some(stored) = events.get(value.value()).map_err(database_error)? else {
                    continue;
                };
                let event: StorageEvent = serde_json::from_slice(stored.value())?;
                if query
                    .bucket
                    .as_ref()
                    .is_some_and(|bucket| bucket != &event.bucket)
                    || query
                        .event_type
                        .is_some_and(|kind| kind != event.event_type)
                    || query.object_prefix.as_ref().is_some_and(|prefix| {
                        event
                            .object
                            .as_ref()
                            .is_none_or(|key| !key.starts_with(prefix))
                    })
                {
                    continue;
                }
                if page.events.len() == limit {
                    page.next = Some((event.time, event.id));
                    break;
                }
                page.events.push(event);
            }
            Ok(page)
        })
        .await?
    }

    async fn create_webhook(
        &self,
        request: CreateWebhookRequest,
    ) -> Result<CreatedWebhook, EventError> {
        self.validate_target(&request.target_url).await?;
        if request.event_types.is_empty()
            || request.event_types.len() > 32
            || request
                .object_prefix_filter
                .as_ref()
                .is_some_and(|prefix| prefix.len() > 1024)
            || request
                .bucket_filter
                .as_ref()
                .is_some_and(|bucket| bucket.is_empty() || bucket.len() > 255)
        {
            return Err(EventError::InvalidSubscription);
        }
        let mut raw_secret = [0_u8; 32];
        OsRng.fill_bytes(&mut raw_secret);
        let signing_secret = URL_SAFE_NO_PAD.encode(raw_secret);
        let encrypted_secret = self.encrypt_secret(signing_secret.as_bytes())?;
        let now = Utc::now();
        let subscription = WebhookSubscription {
            id: WebhookId::new(),
            target_url: request.target_url,
            event_types: request.event_types,
            bucket_filter: request.bucket_filter,
            object_prefix_filter: request.object_prefix_filter,
            enabled: request.enabled,
            created_at: now,
            updated_at: now,
        };
        let db = Arc::clone(&self.database);
        let stored = StoredWebhook {
            subscription: subscription.clone(),
            encrypted_secret,
        };
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(database_error)?;
            {
                let mut table = write.open_table(SUBSCRIPTIONS).map_err(database_error)?;
                let bytes = serde_json::to_vec(&stored)?;
                table
                    .insert(
                        stored.subscription.id.as_uuid().as_bytes().as_slice(),
                        bytes.as_slice(),
                    )
                    .map_err(database_error)?;
            }
            write.commit().map_err(database_error)
        })
        .await??;
        Ok(CreatedWebhook {
            subscription,
            signing_secret,
        })
    }

    async fn list_webhooks(&self) -> Result<Vec<WebhookSubscription>, EventError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(database_error)?;
            let table = read.open_table(SUBSCRIPTIONS).map_err(database_error)?;
            let mut out = Vec::new();
            for item in table.iter().map_err(database_error)? {
                let (_, value) = item.map_err(database_error)?;
                out.push(serde_json::from_slice::<StoredWebhook>(value.value())?.subscription);
            }
            Ok(out)
        })
        .await?
    }

    async fn set_webhook_enabled(
        &self,
        id: WebhookId,
        enabled: bool,
    ) -> Result<WebhookSubscription, EventError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(database_error)?;
            let mut stored: StoredWebhook = {
                let table = write.open_table(SUBSCRIPTIONS).map_err(database_error)?;
                decode(
                    table
                        .get(id.as_uuid().as_bytes().as_slice())
                        .map_err(database_error)?,
                )?
                .ok_or(EventError::WebhookNotFound)?
            };
            stored.subscription.enabled = enabled;
            stored.subscription.updated_at = Utc::now();
            {
                let mut table = write.open_table(SUBSCRIPTIONS).map_err(database_error)?;
                let bytes = serde_json::to_vec(&stored)?;
                table
                    .insert(id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
                    .map_err(database_error)?;
            }
            write.commit().map_err(database_error)?;
            Ok(stored.subscription)
        })
        .await?
    }

    async fn delete_webhook(&self, id: WebhookId) -> Result<(), EventError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(database_error)?;
            {
                let mut table = write.open_table(SUBSCRIPTIONS).map_err(database_error)?;
                if table
                    .remove(id.as_uuid().as_bytes().as_slice())
                    .map_err(database_error)?
                    .is_none()
                {
                    return Err(EventError::WebhookNotFound);
                }
            }
            {
                let mut queue = write.open_table(PENDING).map_err(database_error)?;
                let mut keys = Vec::new();
                for item in queue.iter().map_err(database_error)? {
                    let (key, value) = item.map_err(database_error)?;
                    let pending: PendingDelivery = serde_json::from_slice(value.value())?;
                    if pending.webhook_id == id {
                        keys.push(key.value().to_vec());
                    }
                }
                for key in keys {
                    queue.remove(key.as_slice()).map_err(database_error)?;
                }
            }
            write.commit().map_err(database_error)
        })
        .await?
    }

    async fn list_delivery_logs(
        &self,
        limit: usize,
    ) -> Result<Vec<WebhookDeliveryLog>, EventError> {
        if limit == 0 || limit > 1_000 {
            return Err(EventError::InvalidLimit);
        }
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(database_error)?;
            let table = read.open_table(DELIVERY_LOGS).map_err(database_error)?;
            let mut out = Vec::with_capacity(limit);
            for item in table.iter().map_err(database_error)?.rev().take(limit) {
                let (_, value) = item.map_err(database_error)?;
                out.push(serde_json::from_slice(value.value())?);
            }
            Ok(out)
        })
        .await?
    }

    async fn deliver_due(&self, limit: usize) -> Result<usize, EventError> {
        let db = Arc::clone(&self.database);
        let due = tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(database_error)?;
            let table = read.open_table(PENDING).map_err(database_error)?;
            let now = Utc::now();
            let mut out = Vec::new();
            for item in table.iter().map_err(database_error)? {
                let (_, value) = item.map_err(database_error)?;
                let pending: PendingDelivery = serde_json::from_slice(value.value())?;
                if pending.next_attempt_at <= now {
                    out.push(pending);
                    if out.len() == limit.min(1_000) {
                        break;
                    }
                }
            }
            Ok::<_, EventError>(out)
        })
        .await??;
        let count = due.len();
        for pending in due {
            if let Err(error) = self.deliver_one(pending.clone()).await {
                warn!(event_id = %pending.event_id, webhook_id = %pending.webhook_id, %error, "webhook delivery attempt failed before HTTP dispatch");
                self.record_attempt(
                    pending.clone(),
                    pending.attempts.saturating_add(1),
                    None,
                    false,
                    Some(error.to_string()),
                )
                .await?;
            }
        }
        Ok(count)
    }

    async fn check_ready(&self) -> Result<(), EventError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(database_error)?;
            {
                write.open_table(EVENTS).map_err(database_error)?;
            }
            write.commit().map_err(database_error)
        })
        .await?
    }
}

/// Supervised, cancellation-aware webhook delivery loop.
#[derive(Clone)]
pub struct WebhookWorker {
    repository: Arc<dyn EventRepository>,
    poll_interval: Duration,
}

impl WebhookWorker {
    #[must_use]
    pub fn new(repository: Arc<dyn EventRepository>, poll_interval: Duration) -> Self {
        Self {
            repository,
            poll_interval,
        }
    }

    pub async fn run(self, cancellation: CancellationToken) -> Result<(), EventError> {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        info!("webhook delivery worker started");
        loop {
            tokio::select! { () = cancellation.cancelled() => { info!("webhook delivery worker stopped"); return Ok(()); }, _ = interval.tick() => { match self.repository.deliver_due(100).await { Ok(delivered) if delivered > 0 => info!(delivered, "processed webhook deliveries"), Ok(_) => {}, Err(error) => error!(%error, "webhook delivery scan failed"), } } }
        }
    }
}

#[derive(Debug, Error)]
pub enum EventError {
    #[error("failed to prepare event storage: {0}")]
    Directory(#[source] std::io::Error),
    #[error("event database operation failed: {0}")]
    Database(String),
    #[error("event encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("event storage task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("webhook target is invalid or disallowed")]
    InvalidTarget,
    #[error("webhook target resolves to a private or special-use address")]
    PrivateTarget,
    #[error("failed to resolve webhook target: {0}")]
    ResolveTarget(#[source] std::io::Error),
    #[error("webhook subscription is invalid")]
    InvalidSubscription,
    #[error("webhook was not found")]
    WebhookNotFound,
    #[error("webhook delivery limit must be between 1 and 1000")]
    InvalidLimit,
    #[error("a master key is required for webhook signing secrets")]
    MasterKeyRequired,
    #[error("webhook signing-secret encryption failed")]
    Crypto,
    #[error("webhook signing-secret format is unsupported")]
    UnsupportedSecretFormat,
    #[error("webhook database contains inconsistent delivery state")]
    InconsistentState,
    #[error("failed to build webhook HTTP client: {0}")]
    HttpClient(#[source] reqwest::Error),
}

fn derive_cipher(master_key: &[u8]) -> Result<Aes256Gcm, EventError> {
    let hkdf = Hkdf::<Sha256>::new(Some(b"oes-webhook-secrets-v1"), master_key);
    let mut key = [0_u8; 32];
    hkdf.expand(b"aes-256-gcm", &mut key)
        .map_err(|_| EventError::Crypto)?;
    Aes256Gcm::new_from_slice(&key).map_err(|_| EventError::Crypto)
}
fn database_error(error: impl std::fmt::Display) -> EventError {
    EventError::Database(error.to_string())
}
fn decode<T: for<'de> Deserialize<'de>>(
    value: Option<redb::AccessGuard<&[u8]>>,
) -> Result<Option<T>, EventError> {
    value
        .map(|value| serde_json::from_slice(value.value()).map_err(EventError::from))
        .transpose()
}
fn pending_key(event_id: EventId, webhook_id: WebhookId) -> Vec<u8> {
    let mut key = event_id.as_uuid().as_bytes().to_vec();
    key.extend_from_slice(webhook_id.as_uuid().as_bytes());
    key
}
fn ordered_key(timestamp: DateTime<Utc>, id: Uuid) -> Vec<u8> {
    let encoded = timestamp.timestamp_millis() as u64 ^ (1_u64 << 63);
    let mut key = encoded.to_be_bytes().to_vec();
    key.extend_from_slice(id.as_bytes());
    key
}
fn bounded(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.octets()[0] == 0
                || ip.octets()[0] >= 224)
        }
        IpAddr::V6(ip) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| !is_public_ip(IpAddr::V4(mapped))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn events_are_listed_newest_first_with_filters_and_a_cursor() {
        let directory = tempdir().expect("temporary directory");
        let repository = RedbEventRepository::open(
            directory.path().join("events.redb"),
            None,
            WebhookConfig::default(),
        )
        .await
        .expect("open");

        let base = Utc::now() - chrono::Duration::seconds(600);
        for index in 0..5_i64 {
            let mut event = StorageEvent::new(StorageEventType::ObjectCreated, "uploads").object(
                format!("images/photo-{index}.jpg"),
                None,
                Some(index as u64),
            );
            event.time = base + chrono::Duration::seconds(index * 10);
            repository.publish(&event).await.expect("publish");
        }
        let mut other = StorageEvent::new(StorageEventType::BucketCreated, "reports");
        other.time = base + chrono::Duration::seconds(5);
        repository.publish(&other).await.expect("publish");

        let page = repository
            .list_events(EventQuery {
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .expect("list events");
        assert_eq!(page.events.len(), 6);
        assert!(
            page.events
                .windows(2)
                .all(|pair| pair[0].time >= pair[1].time),
            "events must be ordered newest first"
        );
        assert!(page.next.is_none());

        let filtered = repository
            .list_events(EventQuery {
                bucket: Some("uploads".into()),
                event_type: Some(StorageEventType::ObjectCreated),
                object_prefix: Some("images/photo-1".into()),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .expect("list events");
        assert_eq!(filtered.events.len(), 1);
        assert_eq!(
            filtered.events[0].object.as_deref(),
            Some("images/photo-1.jpg")
        );

        // Paging with the returned cursor must continue without repeating.
        let first = repository
            .list_events(EventQuery {
                limit: 2,
                ..EventQuery::default()
            })
            .await
            .expect("first page");
        assert_eq!(first.events.len(), 2);
        let cursor = first.next.expect("a cursor when more events remain");
        let second = repository
            .list_events(EventQuery {
                after: Some(cursor),
                limit: 2,
                ..EventQuery::default()
            })
            .await
            .expect("second page");
        assert_eq!(second.events.len(), 2);
        let seen: std::collections::BTreeSet<_> = first
            .events
            .iter()
            .chain(second.events.iter())
            .map(|event| event.id)
            .collect();
        assert_eq!(seen.len(), 4, "pages must not overlap");

        let bounded = repository
            .list_events(EventQuery {
                since: Some(base + chrono::Duration::seconds(30)),
                limit: 100,
                ..EventQuery::default()
            })
            .await
            .expect("bounded");
        assert_eq!(bounded.events.len(), 2);
    }

    #[tokio::test]
    async fn events_subscriptions_and_delivery_queue_survive_restart() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.redb");
        let key = b"test webhook master key at least 32 bytes";
        let config = WebhookConfig {
            allow_http: true,
            allow_private_networks: true,
            ..WebhookConfig::default()
        };
        let repository = RedbEventRepository::open(&path, Some(key), config.clone())
            .await
            .expect("open");
        let created = repository
            .create_webhook(CreateWebhookRequest {
                target_url: "http://127.0.0.1:9/hook".into(),
                event_types: vec![StorageEventType::ObjectCreated],
                bucket_filter: Some("uploads".into()),
                object_prefix_filter: Some("images/".into()),
                enabled: true,
            })
            .await
            .expect("create webhook");
        assert!(!created.signing_secret.is_empty());
        repository
            .publish(
                &StorageEvent::new(StorageEventType::ObjectCreated, "uploads").object(
                    "images/photo.jpg",
                    None,
                    Some(10),
                ),
            )
            .await
            .expect("publish");
        drop(repository);
        let reopened = RedbEventRepository::open(&path, Some(key), config)
            .await
            .expect("reopen");
        assert_eq!(reopened.list_webhooks().await.expect("list").len(), 1);
        assert_eq!(reopened.deliver_due(10).await.expect("delivery attempt"), 1);
        let logs = reopened.list_delivery_logs(10).await.expect("logs");
        assert_eq!(logs.len(), 1);
        assert!(!logs[0].success);
    }

    #[tokio::test]
    async fn safe_defaults_reject_private_and_plain_http_targets() {
        let directory = tempdir().expect("temporary directory");
        let repository = RedbEventRepository::open(
            directory.path().join("events.redb"),
            Some(b"test webhook master key at least 32 bytes"),
            WebhookConfig::default(),
        )
        .await
        .expect("open");
        let result = repository
            .create_webhook(CreateWebhookRequest {
                target_url: "http://127.0.0.1/hook".into(),
                event_types: vec![StorageEventType::ObjectCreated],
                bucket_filter: None,
                object_prefix_filter: None,
                enabled: true,
            })
            .await;
        assert!(matches!(result, Err(EventError::InvalidTarget)));
    }
}
