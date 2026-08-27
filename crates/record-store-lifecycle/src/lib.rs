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
use record_store_core::{AuditEventId, LifecycleRule, LifecycleRuleId};
use record_store_metadata::{
    ListObjectVersionsRequest, ListObjectsRequest, MetadataError, MetadataRepository,
};
use record_store_service::{ServiceError, Services};
use redb::{Database, TableDefinition};
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
            let database = Database::create(path).map_err(database_error)?;
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
                    Ok(result) if result.scanned > 0 => info!(scanned = result.scanned, expired = result.expired, failures = result.failures, "lifecycle scan completed"),
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
                    match self
                        .services
                        .objects
                        .delete(&bucket.name, object.key.clone())
                        .await
                    {
                        Ok(true) => {
                            result.expired = result.expired.saturating_add(1);
                            self.audit_expiration(
                                "lifecycle.expire-object",
                                &bucket.name.to_string(),
                                object.key.as_str(),
                                None,
                            )
                            .await?;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            result.failures = result.failures.saturating_add(1);
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
                    match self
                        .services
                        .objects
                        .delete_version(&bucket.name, key.clone(), version_id)
                        .await
                    {
                        Ok(()) => {
                            result.expired = result.expired.saturating_add(1);
                            self.audit_expiration(
                                "lifecycle.expire-noncurrent-version",
                                &bucket.name.to_string(),
                                key.as_str(),
                                Some(version_id),
                            )
                            .await?;
                        }
                        Err(error) => {
                            result.failures = result.failures.saturating_add(1);
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

    async fn audit_expiration(
        &self,
        operation: &str,
        bucket: &str,
        key: &str,
        version_id: Option<record_store_core::VersionId>,
    ) -> Result<(), LifecycleError> {
        let mut metadata = std::collections::BTreeMap::new();
        if let Some(version_id) = version_id {
            metadata.insert("version_id".into(), version_id.to_string());
        }
        self.audit
            .append(&AuditEvent {
                event_id: AuditEventId::new(),
                timestamp: Utc::now(),
                request_id: None,
                principal: "system:lifecycle".into(),
                credential_id: None,
                source_ip: None,
                operation: operation.into(),
                resource: format!("bucket:{bucket}/{key}"),
                result: AuditResult::Success,
                metadata,
            })
            .await?;
        Ok(())
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
        ObjectKey, ObjectMetadata, OrganizationId, VersionId, VersioningState,
    };
    use record_store_metadata::RedbMetadataRepository;
    use record_store_service::ServiceLimits;
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
            durability_policy: None,
            cors: None,
        };
        metadata.create_bucket(&bucket).await.expect("bucket");
        let key = ObjectKey::new("expired.txt").expect("key");
        metadata
            .put_object(&ObjectMetadata {
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
            })
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
                maximum_custom_metadata_entries: 8,
                maximum_custom_metadata_bytes: 1024,
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
        assert_eq!(
            audit
                .query(AuditQuery {
                    limit: 10,
                    ..AuditQuery::default()
                })
                .await
                .expect("audit query")
                .events
                .len(),
            1
        );
    }
}
