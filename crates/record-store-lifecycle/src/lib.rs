//! Incremental, restart-safe lifecycle expiration worker.
//!
//! The worker keeps durable per-rule cursors so a restart resumes a scan instead
//! of repeating it. In a cluster an activation gate restricts scanning to one
//! node at a time, because expiring the same object from several nodes would
//! create duplicate delete markers.

use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use record_store_audit::{AuditEvent, AuditRepository, AuditResult};
use record_store_core::{AuditEventId, LifecycleRule, LifecycleRuleId, open_database};
use record_store_metadata::{
    ListObjectVersionsRequest, ListObjectsRequest, MetadataError, MetadataRepository,
};
use record_store_service::{LockContext, ServiceError, Services};
use redb::{Database, ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

const CURSORS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("lifecycle_cursors_v1");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RuleCursor {
    current_key: Option<String>,
    version_key: Option<String>,
    version_id: Option<record_store_core::VersionId>,
}

/// Observable outcome of one bounded metadata scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LifecycleRunResult {
    pub scanned: u64,
    pub expired: u64,
    /// Versions Object Lock held back. Counted apart from failures because
    /// nothing went wrong: the rule was simply outranked.
    pub skipped: u64,
    pub failures: u64,
}

/// Decides whether this process may currently run lifecycle scans.
#[async_trait]
pub trait LifecycleGate: Send + Sync {
    /// Returns whether scanning is permitted right now.
    async fn active(&self) -> bool;
}

/// Supervised lifecycle engine using durable per-rule cursors.
#[derive(Clone)]
pub struct LifecycleWorker {
    database: Arc<Database>,
    metadata: Arc<dyn MetadataRepository>,
    services: Services,
    audit: Arc<dyn AuditRepository>,
    interval: Duration,
    batch_size: usize,
    gate: Option<Arc<dyn LifecycleGate>>,
}

impl LifecycleWorker {
    pub async fn open(
        path: impl AsRef<Path>,
        metadata: Arc<dyn MetadataRepository>,
        services: Services,
        audit: Arc<dyn AuditRepository>,
        interval: Duration,
        batch_size: usize,
    ) -> Result<Self, LifecycleError> {
        if batch_size == 0 || batch_size > 1_000 {
            return Err(LifecycleError::InvalidBatchSize);
        }
        if let Some(parent) = path.as_ref().parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(LifecycleError::Directory)?;
        }
        let path = path.as_ref().to_owned();
        let database = tokio::task::spawn_blocking(move || {
            let database = open_database(path).map_err(database_error)?;
            let write = database.begin_write().map_err(database_error)?;
            {
                write.open_table(CURSORS).map_err(database_error)?;
            }
            write.commit().map_err(database_error)?;
            Ok::<_, LifecycleError>(database)
        })
        .await??;
        Ok(Self {
            database: Arc::new(database),
            metadata,
            services,
            audit,
            interval,
            batch_size,
            gate: None,
        })
    }

    /// Runs one bounded pass over every enabled rule.
    pub async fn run_once(&self) -> Result<LifecycleRunResult, LifecycleError> {
        let rules = self.metadata.list_lifecycle_rules(None).await?;
        let mut total = LifecycleRunResult::default();
        for rule in rules.into_iter().filter(|rule| rule.enabled) {
            let result = self.run_rule(&rule).await?;
            total.scanned = total.scanned.saturating_add(result.scanned);
            total.expired = total.expired.saturating_add(result.expired);
            total.skipped = total.skipped.saturating_add(result.skipped);
            total.failures = total.failures.saturating_add(result.failures);
        }
        Ok(total)
    }

    /// Runs until cancellation; individual scan failures remain visible and retry later.
    /// Restricts scanning to when the gate allows it.
    ///
    /// A cluster runs one lifecycle scanner at a time: expiring the same object
    /// from several nodes would create duplicate delete markers and waste work.
    #[must_use]
    pub fn with_activation_gate(mut self, gate: Arc<dyn LifecycleGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    pub async fn run(self, cancellation: CancellationToken) -> Result<(), LifecycleError> {
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        info!("lifecycle worker started");
        loop {
            tokio::select! {
                () = cancellation.cancelled() => {
                    info!("lifecycle worker stopped");
                    return Ok(());
                }
                _ = interval.tick() => {
                    if let Some(gate) = &self.gate
                        && !gate.active().await
                    {
                        continue;
                    }
                    match self.run_once().await {
                    Ok(result) if result.scanned > 0 => info!(scanned = result.scanned, expired = result.expired, skipped = result.skipped, failures = result.failures, "lifecycle scan completed"),
                    Ok(_) => {},
                    Err(error) => error!(%error, "lifecycle scan failed"),
                    }
                }
            }
        }
    }

    async fn run_rule(&self, rule: &LifecycleRule) -> Result<LifecycleRunResult, LifecycleError> {
        let bucket = self
            .metadata
            .get_bucket(rule.bucket_id)
            .await?
            .ok_or(LifecycleError::BucketMissing)?;
        let mut cursor = self.read_cursor(rule.id).await?;
        let mut result = LifecycleRunResult::default();
        if let Some(days) = rule.expiration {
            let page = self
                .metadata
                .list_objects(ListObjectsRequest {
                    bucket_id: rule.bucket_id,
                    prefix: rule.prefix.clone(),
                    start_after: cursor.current_key.clone(),
                    limit: self.batch_size,
                })
                .await?;
            let cutoff = Utc::now() - chrono::Duration::days(i64::from(days.get()));
            for object in page.objects {
                result.scanned = result.scanned.saturating_add(1);
                if object.modified_at <= cutoff {
                    // Announced before the delete, like every other mutating
                    // path: a crash between the delete and its record would
                    // otherwise leave an expiry nobody can account for.
                    let intent = match self
                        .announce(
                            "lifecycle.expire-object",
                            &bucket.name.to_string(),
                            object.key.as_str(),
                            None,
                        )
                        .await
                    {
                        Ok(intent) => intent,
                        Err(error) => {
                            result.failures = result.failures.saturating_add(1);
                            error!(rule_id = %rule.id, key = %object.key, %error, "lifecycle expiry skipped: it could not be announced");
                            continue;
                        }
                    };
                    match self
                        .services
                        .objects
                        .delete(&bucket.name, object.key.clone())
                        .await
                    {
                        Ok(true) => {
                            result.expired = result.expired.saturating_add(1);
                            self.resolve(intent, AuditResult::Success).await;
                        }
                        Ok(false) => {
                            // Nothing was there to expire. The announcement
                            // stands, resolved as a no-op rather than left
                            // dangling.
                            self.resolve(intent, AuditResult::Failure).await;
                        }
                        Err(error) => {
                            result.failures = result.failures.saturating_add(1);
                            self.resolve(intent, AuditResult::Failure).await;
                            error!(rule_id = %rule.id, key = %object.key, %error, "lifecycle object expiration failed");
                        }
                    }
                }
            }
            cursor.current_key = page.next_key;
        }
        if let Some(days) = rule.noncurrent_version_expiration {
            let page = self
                .metadata
                .list_object_versions(ListObjectVersionsRequest {
                    bucket_id: rule.bucket_id,
                    prefix: rule.prefix.clone(),
                    key_marker: cursor.version_key.clone(),
                    version_id_marker: cursor.version_id,
                    limit: self.batch_size,
                })
                .await?;
            let cutoff = Utc::now() - chrono::Duration::days(i64::from(days.get()));
            for version in page.versions {
                result.scanned = result.scanned.saturating_add(1);
                if !version.is_latest && version.record.created_at() <= cutoff {
                    let key = version.record.key().clone();
                    let version_id = version.record.version_id();
                    let intent = match self
                        .announce(
                            "lifecycle.expire-noncurrent-version",
                            &bucket.name.to_string(),
                            key.as_str(),
                            Some(version_id),
                        )
                        .await
                    {
                        Ok(intent) => intent,
                        Err(error) => {
                            result.failures = result.failures.saturating_add(1);
                            error!(rule_id = %rule.id, key = %key, %error, "lifecycle version expiry skipped: it could not be announced");
                            continue;
                        }
                    };
                    match self
                        .services
                        .objects
                        .delete_version(
                            &bucket.name,
                            key.clone(),
                            version_id,
                            // A background scan is not a person exercising a
                            // permission, so it never carries a governance
                            // bypass. Expiry gives way to retention, not the
                            // other way around.
                            &LockContext::system("lifecycle"),
                        )
                        .await
                    {
                        Ok(()) => {
                            result.expired = result.expired.saturating_add(1);
                            self.resolve(intent, AuditResult::Success).await;
                        }
                        // A retained or held version is not a failure: the rule
                        // and the lock disagree, and the lock wins. The scan
                        // records why it stepped over this one and carries on,
                        // because aborting here would stop every later key in
                        // the bucket from ever expiring.
                        Err(ServiceError::ObjectLocked(block)) => {
                            result.skipped = result.skipped.saturating_add(1);
                            self.resolve(intent, AuditResult::Denied).await;
                            self.audit_lock_skip(
                                rule,
                                &bucket.name.to_string(),
                                key.as_str(),
                                version_id,
                                block.label(),
                            )
                            .await?;
                        }
                        Err(ServiceError::RetentionClockUnavailable) => {
                            result.skipped = result.skipped.saturating_add(1);
                            self.resolve(intent, AuditResult::Denied).await;
                            self.audit_lock_skip(
                                rule,
                                &bucket.name.to_string(),
                                key.as_str(),
                                version_id,
                                "clock_unavailable",
                            )
                            .await?;
                        }
                        Err(error) => {
                            result.failures = result.failures.saturating_add(1);
                            self.resolve(intent, AuditResult::Failure).await;
                            error!(rule_id = %rule.id, key = %key, %error, "lifecycle version expiration failed");
                        }
                    }
                }
            }
            cursor.version_key = page.next_key_marker;
            cursor.version_id = page.next_version_id_marker;
        }
        self.write_cursor(rule.id, &cursor).await?;
        Ok(result)
    }

    /// Records that Object Lock held a version back, naming the rule and why.
    ///
    /// An operator looking at a rule that is not expiring anything needs this
    /// to be a durable record rather than a log line that has rotated away.
    async fn audit_lock_skip(
        &self,
        rule: &LifecycleRule,
        bucket: &str,
        key: &str,
        version_id: record_store_core::VersionId,
        reason: &str,
    ) -> Result<(), LifecycleError> {
        let mut metadata = std::collections::BTreeMap::new();
        metadata.insert("version_id".into(), version_id.to_string());
        metadata.insert("rule_id".into(), rule.id.to_string());
        metadata.insert("rule_prefix".into(), rule.prefix.clone());
        metadata.insert("reason".into(), reason.to_owned());
        self.audit
            .append(&AuditEvent {
                event_id: AuditEventId::new(),
                timestamp: Utc::now(),
                request_id: None,
                principal: "system:lifecycle".into(),
                credential_id: None,
                source_ip: None,
                operation: "lifecycle.skip-locked-version".into(),
                resource: format!("bucket:{bucket}/{key}"),
                result: AuditResult::Denied,
                metadata,
            })
            .await?;
        Ok(())
    }

    /// Announces an expiry before it happens.
    ///
    /// The lifecycle worker mutates durable state without a request behind it,
    /// so it owes the same two records a request does: the announcement is
    /// durable before the version can be gone, and an announcement with no
    /// outcome is an expiry this server cannot account for.
    async fn announce(
        &self,
        operation: &str,
        bucket: &str,
        key: &str,
        version_id: Option<record_store_core::VersionId>,
    ) -> Result<AuditEvent, LifecycleError> {
        let mut metadata = std::collections::BTreeMap::new();
        if let Some(version_id) = version_id {
            metadata.insert("version_id".into(), version_id.to_string());
        }
        let event = AuditEvent {
            event_id: AuditEventId::new(),
            timestamp: Utc::now(),
            request_id: None,
            principal: "system:lifecycle".into(),
            credential_id: None,
            source_ip: None,
            operation: operation.into(),
            resource: format!("bucket:{bucket}/{key}"),
            result: AuditResult::Attempted,
            metadata,
        };
        self.audit.append(&event).await?;
        Ok(event)
    }

    /// Records what an announced expiry went on to do.
    ///
    /// A failure here leaves the announcement standing alone rather than
    /// aborting the scan: the pass has already changed durable state, and
    /// stopping now would strand every later key in the bucket.
    async fn resolve(&self, intent: AuditEvent, result: AuditResult) {
        let outcome = record_store_audit::intent::outcome_event(&intent, result);
        if let Err(error) = self.audit.append(&outcome).await {
            error!(
                %error,
                operation = %intent.operation,
                "the lifecycle expiry record stands without its outcome"
            );
        }
    }

    async fn read_cursor(&self, id: LifecycleRuleId) -> Result<RuleCursor, LifecycleError> {
        let db = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = db.begin_read().map_err(database_error)?;
            let table = read.open_table(CURSORS).map_err(database_error)?;
            table
                .get(id.as_uuid().as_bytes().as_slice())
                .map_err(database_error)?
                .map(|value| serde_json::from_slice(value.value()).map_err(LifecycleError::from))
                .transpose()
                .map(Option::unwrap_or_default)
        })
        .await?
    }

    async fn write_cursor(
        &self,
        id: LifecycleRuleId,
        cursor: &RuleCursor,
    ) -> Result<(), LifecycleError> {
        let db = Arc::clone(&self.database);
        let bytes = serde_json::to_vec(cursor)?;
        tokio::task::spawn_blocking(move || {
            let write = db.begin_write().map_err(database_error)?;
            {
                let mut table = write.open_table(CURSORS).map_err(database_error)?;
                table
                    .insert(id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
                    .map_err(database_error)?;
            }
            write.commit().map_err(database_error)
        })
        .await?
    }
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("failed to prepare lifecycle state: {0}")]
    Directory(#[source] std::io::Error),
    #[error("lifecycle state database failed: {0}")]
    Database(String),
    #[error("lifecycle state encoding failed: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("lifecycle task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("lifecycle metadata failed: {0}")]
    Metadata(#[from] MetadataError),
    #[error("lifecycle object action failed: {0}")]
    Service(#[from] ServiceError),
    #[error("lifecycle audit append failed: {0}")]
    Audit(#[from] record_store_audit::AuditError),
    #[error("lifecycle batch size must be between 1 and 1000")]
    InvalidBatchSize,
    #[error("lifecycle rule refers to a missing bucket")]
    BucketMissing,
}

fn database_error(error: impl std::fmt::Display) -> LifecycleError {
    LifecycleError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    use record_store_audit::{AuditQuery, RedbAuditRepository};
    use record_store_core::{
        Bucket, BucketId, BucketName, BucketQuota, Checksum, ETag, ExpirationDays, ObjectId,
        ObjectKey, ObjectMetadata, OrganizationId, VersionId, VersioningState, WriteOrigin,
    };
    use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
    use record_store_service::{ObjectLockLimits, ServiceLimits};
    use record_store_storage::{LocalFilesystemStore, ObjectStore};
    use tempfile::tempdir;

    #[tokio::test]
    async fn expired_objects_are_removed_and_audited_with_restart_safe_state() {
        let directory = tempdir().expect("temporary directory");
        let metadata = Arc::new(
            RedbMetadataRepository::open(directory.path().join("catalog.redb"))
                .await
                .expect("metadata"),
        );
        let metadata_dependency: Arc<dyn MetadataRepository> = metadata.clone();
        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new("lifecycle-test").expect("bucket name"),
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
            storage_class: None,
            durability_policy: None,
            object_lock: None,
            cors: None,
        };
        metadata.create_bucket(&bucket).await.expect("bucket");
        let key = ObjectKey::new("expired.txt").expect("key");
        metadata
            .put_object(
                &ObjectMetadata {
                    id: ObjectId::new(),
                    bucket_id: bucket.id,
                    key: key.clone(),
                    version_id: VersionId::new(),
                    size: 0,
                    checksum: Checksum::sha256([0_u8; 32]),
                    payload_format: record_store_core::PayloadFormat::Plaintext,
                    durability: record_store_core::DurabilityProfile::Single,
                    etag: ETag::from_md5([0_u8; 16]),
                    content_type: None,
                    custom_metadata: BTreeMap::new(),
                    created_at: Utc::now() - chrono::Duration::days(3),
                    modified_at: Utc::now() - chrono::Duration::days(3),
                },
                None,
                WriteOrigin::Direct,
            )
            .await
            .expect("object metadata");
        metadata
            .put_lifecycle_rule(&LifecycleRule {
                id: LifecycleRuleId::new(),
                bucket_id: bucket.id,
                prefix: String::new(),
                enabled: true,
                expiration: Some(ExpirationDays::new(1).expect("days")),
                noncurrent_version_expiration: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await
            .expect("rule");
        let storage = Arc::new(
            LocalFilesystemStore::open(
                directory.path().join("data"),
                directory.path().join("tmp"),
                metadata_dependency.clone(),
            )
            .await
            .expect("storage"),
        );
        let storage_dependency: Arc<dyn ObjectStore> = storage;
        let services = Services::new(
            storage_dependency,
            metadata_dependency.clone(),
            bucket.organization_id,
            ServiceLimits {
                maximum_concurrent_operations: 4,
                admission_wait_limit_seconds: 5,
                maximum_custom_metadata_entries: 8,
                maximum_custom_metadata_bytes: 1024,
                object_lock: ObjectLockLimits::default(),
            },
        );
        let audit = Arc::new(
            RedbAuditRepository::open(directory.path().join("audit.redb"))
                .await
                .expect("audit"),
        );
        let audit_dependency: Arc<dyn AuditRepository> = audit.clone();
        let worker = LifecycleWorker::open(
            directory.path().join("lifecycle.redb"),
            metadata_dependency,
            services,
            audit_dependency,
            Duration::from_secs(60),
            10,
        )
        .await
        .expect("worker");
        let result = worker.run_once().await.expect("run lifecycle");
        assert_eq!(result.expired, 1);
        assert!(
            metadata
                .get_object(bucket.id, &key)
                .await
                .expect("get")
                .is_none()
        );
        // An expiry mutates durable state, so it leaves the same pair every
        // other mutation does: announced before it happened, resolved after.
        let events = audit
            .query(AuditQuery {
                limit: 10,
                ..AuditQuery::default()
            })
            .await
            .expect("audit query")
            .events;
        assert_eq!(events.len(), 2, "{events:?}");
        let intent = events
            .iter()
            .find(|event| event.result == record_store_audit::AuditResult::Attempted)
            .expect("the expiry was announced before it happened");
        let outcome = events
            .iter()
            .find(|event| event.result == record_store_audit::AuditResult::Success)
            .expect("and reported afterwards");
        assert_eq!(intent.operation, "lifecycle.expire-object");
        assert_eq!(intent.principal, "system:lifecycle");
        assert_eq!(
            outcome
                .metadata
                .get(record_store_audit::intent::INTENT_EVENT_ID),
            Some(&intent.event_id.to_string())
        );
        assert!(intent.timestamp <= outcome.timestamp);
    }

    /// The batch size bounds how much one pass may delete. Zero would make the
    /// service inert and an unbounded value would let one pass stall the node,
    /// so both ends are refused at construction rather than at run time.
    #[tokio::test]
    async fn an_unusable_batch_size_is_refused_at_construction() {
        let directory = tempdir().expect("temporary directory");
        let metadata: Arc<dyn MetadataRepository> = Arc::new(
            RedbMetadataRepository::open(directory.path().join("catalog.redb"))
                .await
                .expect("metadata"),
        );
        let storage: Arc<dyn ObjectStore> = Arc::new(
            LocalFilesystemStore::open(
                directory.path().join("data"),
                directory.path().join("tmp"),
                Arc::clone(&metadata),
            )
            .await
            .expect("storage"),
        );
        let audit: Arc<dyn record_store_audit::AuditRepository> = Arc::new(
            RedbAuditRepository::open(directory.path().join("audit.redb"))
                .await
                .expect("audit"),
        );
        let services = Services::new(
            storage,
            Arc::clone(&metadata),
            OrganizationId::new(),
            ServiceLimits {
                maximum_concurrent_operations: 4,
                admission_wait_limit_seconds: 5,
                maximum_custom_metadata_entries: 8,
                maximum_custom_metadata_bytes: 1024,
                object_lock: ObjectLockLimits::default(),
            },
        );

        for batch_size in [0, 1_001] {
            let result = LifecycleWorker::open(
                directory
                    .path()
                    .join(format!("lifecycle-{batch_size}.redb")),
                Arc::clone(&metadata),
                services.clone(),
                Arc::clone(&audit),
                Duration::from_secs(60),
                batch_size,
            )
            .await;
            assert!(
                matches!(result, Err(LifecycleError::InvalidBatchSize)),
                "accepted batch size {batch_size}"
            );
        }

        LifecycleWorker::open(
            directory.path().join("lifecycle-ok.redb"),
            metadata,
            services,
            audit,
            Duration::from_secs(60),
            100,
        )
        .await
        .expect("a bounded batch size is accepted");
    }

    /// A pass over a deployment with no rules must do nothing rather than
    /// treating "no rule" as "expire everything".
    #[tokio::test]
    async fn a_pass_with_no_rules_deletes_nothing() {
        let directory = tempdir().expect("temporary directory");
        let metadata: Arc<dyn MetadataRepository> = Arc::new(
            RedbMetadataRepository::open(directory.path().join("catalog.redb"))
                .await
                .expect("metadata"),
        );
        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new("kept").expect("bucket"),
            created_at: Utc::now(),
            versioning: VersioningState::Disabled,
            quota: BucketQuota::default(),
            storage_class: None,
            durability_policy: None,
            object_lock: None,
            cors: None,
        };
        metadata.create_bucket(&bucket).await.expect("bucket");
        let key = ObjectKey::new("old.txt").expect("key");
        metadata
            .put_object(
                &ObjectMetadata {
                    id: ObjectId::new(),
                    bucket_id: bucket.id,
                    key: key.clone(),
                    version_id: VersionId::new(),
                    size: 0,
                    checksum: Checksum::sha256([0_u8; 32]),
                    payload_format: record_store_core::PayloadFormat::Plaintext,
                    durability: record_store_core::DurabilityProfile::Single,
                    etag: ETag::from_md5([0_u8; 16]),
                    content_type: None,
                    custom_metadata: BTreeMap::new(),
                    created_at: Utc::now() - chrono::Duration::days(365),
                    modified_at: Utc::now() - chrono::Duration::days(365),
                },
                None,
                WriteOrigin::Direct,
            )
            .await
            .expect("object");

        let storage: Arc<dyn ObjectStore> = Arc::new(
            LocalFilesystemStore::open(
                directory.path().join("data"),
                directory.path().join("tmp"),
                Arc::clone(&metadata),
            )
            .await
            .expect("storage"),
        );
        let audit: Arc<dyn record_store_audit::AuditRepository> = Arc::new(
            RedbAuditRepository::open(directory.path().join("audit.redb"))
                .await
                .expect("audit"),
        );
        let services = Services::new(
            storage,
            Arc::clone(&metadata),
            bucket.organization_id,
            ServiceLimits {
                maximum_concurrent_operations: 4,
                admission_wait_limit_seconds: 5,
                maximum_custom_metadata_entries: 8,
                maximum_custom_metadata_bytes: 1024,
                object_lock: ObjectLockLimits::default(),
            },
        );
        let lifecycle = LifecycleWorker::open(
            directory.path().join("lifecycle.redb"),
            Arc::clone(&metadata),
            services,
            audit,
            Duration::from_secs(60),
            100,
        )
        .await
        .expect("lifecycle");

        lifecycle.run_once().await.expect("pass");
        assert!(
            metadata
                .get_object(bucket.id, &key)
                .await
                .expect("read")
                .is_some(),
            "an object nobody wrote a rule for must survive"
        );
    }

    /// Expiry and retention will eventually disagree about the same version.
    /// When they do the lock wins, the scan says so durably, and it keeps
    /// going — aborting here would stop every later key in the bucket from ever
    /// expiring, turning one retained record into a silent outage for the rule.
    #[tokio::test]
    async fn a_retained_version_is_skipped_audited_and_the_scan_continues() {
        use record_store_core::{
            ObjectLockConfiguration, ObjectLockState, Retention, RetentionMode,
        };

        let directory = tempdir().expect("temporary directory");
        let metadata: Arc<dyn MetadataRepository> = Arc::new(
            RedbMetadataRepository::open(directory.path().join("catalog.redb"))
                .await
                .expect("metadata"),
        );
        let bucket = Bucket {
            id: BucketId::new(),
            organization_id: OrganizationId::new(),
            name: BucketName::new("records").expect("bucket"),
            created_at: Utc::now(),
            versioning: VersioningState::Enabled,
            quota: BucketQuota::default(),
            storage_class: None,
            durability_policy: None,
            object_lock: Some(ObjectLockConfiguration::default()),
            cors: None,
        };
        metadata.create_bucket(&bucket).await.expect("bucket");

        let old = Utc::now() - chrono::Duration::days(30);
        let stored = |key: &str| ObjectMetadata {
            id: ObjectId::new(),
            bucket_id: bucket.id,
            key: ObjectKey::new(key).expect("key"),
            version_id: VersionId::new(),
            size: 0,
            checksum: Checksum::sha256([0_u8; 32]),
            payload_format: record_store_core::PayloadFormat::Plaintext,
            durability: record_store_core::DurabilityProfile::Single,
            etag: ETag::from_md5([0_u8; 16]),
            content_type: None,
            custom_metadata: BTreeMap::new(),
            created_at: old,
            modified_at: old,
        };

        // One retained version and one ordinary version, both long past the
        // expiration age, and both made non-current so the rule considers them.
        let retained = stored("a-retained.txt");
        metadata
            .put_object(
                &retained,
                Some(ObjectLockState {
                    retention: Some(Retention {
                        mode: RetentionMode::Compliance,
                        retain_until: Utc::now() + chrono::Duration::days(3_650),
                    }),
                    legal_hold: false,
                }),
                WriteOrigin::Direct,
            )
            .await
            .expect("retained object");
        metadata
            .put_object(&stored("a-retained.txt"), None, WriteOrigin::Direct)
            .await
            .expect("supersede the retained version");

        let expirable = stored("b-expirable.txt");
        metadata
            .put_object(&expirable, None, WriteOrigin::Direct)
            .await
            .expect("expirable object");
        metadata
            .put_object(&stored("b-expirable.txt"), None, WriteOrigin::Direct)
            .await
            .expect("supersede the expirable version");

        metadata
            .put_lifecycle_rule(&LifecycleRule {
                id: LifecycleRuleId::new(),
                bucket_id: bucket.id,
                prefix: String::new(),
                enabled: true,
                expiration: None,
                noncurrent_version_expiration: Some(ExpirationDays::new(1).expect("days")),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await
            .expect("rule");

        let storage: Arc<dyn ObjectStore> = Arc::new(
            LocalFilesystemStore::open(
                directory.path().join("data"),
                directory.path().join("tmp"),
                Arc::clone(&metadata),
            )
            .await
            .expect("storage"),
        );
        let audit = Arc::new(
            RedbAuditRepository::open(directory.path().join("audit.redb"))
                .await
                .expect("audit"),
        );
        let audit_dependency: Arc<dyn record_store_audit::AuditRepository> = audit.clone();
        let services = Services::new(
            storage,
            Arc::clone(&metadata),
            bucket.organization_id,
            ServiceLimits {
                maximum_concurrent_operations: 4,
                admission_wait_limit_seconds: 5,
                maximum_custom_metadata_entries: 8,
                maximum_custom_metadata_bytes: 1024,
                object_lock: ObjectLockLimits::default(),
            },
        );
        let lifecycle = LifecycleWorker::open(
            directory.path().join("lifecycle.redb"),
            Arc::clone(&metadata),
            services,
            audit_dependency,
            Duration::from_secs(60),
            100,
        )
        .await
        .expect("lifecycle");

        let result = lifecycle.run_once().await.expect("the scan completes");
        assert_eq!(result.skipped, 1, "the retained version is skipped");
        assert_eq!(result.failures, 0, "a lock is not a failure");
        assert_eq!(
            result.expired, 1,
            "the scan continued past the retained version and expired the next one"
        );

        // The retained version is still there; the unretained one is gone.
        assert!(
            metadata
                .get_object_version(bucket.id, &retained.key, retained.version_id)
                .await
                .expect("read retained")
                .is_some(),
            "a retained version outranks a lifecycle rule"
        );
        assert!(
            metadata
                .get_object_version(bucket.id, &expirable.key, expirable.version_id)
                .await
                .expect("read expirable")
                .is_none(),
            "an unretained version still expires"
        );

        // The skip is durable, and names both the rule and the reason.
        let page = audit
            .query(AuditQuery {
                operation: Some("lifecycle.skip-locked-version".into()),
                limit: 50,
                ..AuditQuery::default()
            })
            .await
            .expect("audit query");
        assert_eq!(page.events.len(), 1, "one skip, one record");
        let event = &page.events[0];
        assert_eq!(event.principal, "system:lifecycle");
        assert_eq!(
            event.metadata.get("reason").map(String::as_str),
            Some("compliance_retention"),
            "the record names why the version was left alone"
        );
        assert!(
            event.metadata.contains_key("rule_id"),
            "the record names the rule that stepped over it"
        );
        assert_eq!(
            event.metadata.get("version_id").map(String::as_str),
            Some(retained.version_id.to_string()).as_deref(),
            "the record names the version by its stable identifier"
        );
    }
}
