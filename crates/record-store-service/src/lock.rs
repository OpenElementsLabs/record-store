//! Object Lock policy: who may release a retained version, and on what clock.
//!
//! The catalog enforces the invariant — it refuses, inside the transaction, to
//! remove a version a retention still holds. This module decides the policy
//! around that: it resolves what lock a new version is born with, applies the
//! S3 mode rules to a requested change, and records every governance bypass in
//! the durable audit trail.

use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};

use chrono::Utc;
use record_store_audit::{AuditEvent, AuditRepository, AuditResult};
use record_store_core::{
    AuditEventId, Bucket, BucketName, ObjectKey, ObjectLockConfiguration, ObjectLockState,
    VersionId,
};
use record_store_metadata::{LockRelease, MetadataRepository};
use tracing::warn;

use crate::error::map_metadata;
use crate::services::BucketCoordinator;
use crate::*;

/// Who is asking, and whether they carry an authorized governance bypass.
///
/// The bypass flag means the permission was already checked. Presenting
/// `x-amz-bypass-governance-retention` requires `s3:BypassGovernanceRetention`,
/// so a caller without that permission is refused before reaching this layer
/// and can never set this to true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockContext {
    /// Stable non-secret principal name, as it appears in audit records.
    pub principal: String,
    /// Whether an authorized governance bypass was presented.
    pub bypass_governance: bool,
}

impl LockContext {
    /// Builds a context for an ordinary caller presenting no bypass.
    #[must_use]
    pub fn principal(principal: impl Into<String>) -> Self {
        Self {
            principal: principal.into(),
            bypass_governance: false,
        }
    }

    /// Builds a context for an internal component, which never bypasses.
    ///
    /// A background worker is not a person exercising a permission, so it gets
    /// no bypass regardless of what it is scanning.
    #[must_use]
    pub fn system(component: &str) -> Self {
        Self::principal(format!("system:{component}"))
    }

    /// Returns the same context with an authorized bypass applied.
    #[must_use]
    pub const fn with_governance_bypass(mut self, bypass: bool) -> Self {
        self.bypass_governance = bypass;
        self
    }
}

/// Deployment-wide Object Lock settings shared by the services that need them.
pub(crate) struct LockPolicy {
    pub(crate) clock_tolerance_seconds: u32,
    pub(crate) audit: Option<Arc<dyn AuditRepository>>,
}

impl LockPolicy {
    /// Builds the clock and bypass inputs for one operation.
    pub(crate) fn release(&self, context: &LockContext) -> LockRelease {
        LockRelease::new(Utc::now(), self.clock_tolerance_seconds)
            .with_governance_bypass(context.bypass_governance)
    }

    /// Announces a governance bypass before the operation that uses it runs.
    ///
    /// A bypass is the one way a retained version leaves before its date, so
    /// the evidence has to be durable *before* the version can be gone. A
    /// record written afterwards is lost by exactly the crash that would make
    /// it matter most.
    ///
    /// Two failures refuse the operation outright rather than proceeding
    /// unrecorded, because a bypass nobody can account for afterwards is worse
    /// than a bypass that did not happen:
    ///
    /// - no audit trail is configured at all, so no evidence is possible;
    /// - the trail is configured and cannot be written.
    pub(crate) async fn begin_bypass(
        &self,
        context: &LockContext,
        operation: &str,
        bucket: &BucketName,
        key: &ObjectKey,
        version_id: VersionId,
    ) -> Result<Option<AuditEvent>, ServiceError> {
        if !context.bypass_governance {
            return Ok(None);
        }
        let Some(audit) = &self.audit else {
            warn!(
                operation,
                principal = %context.principal,
                "refusing a governance bypass: no durable audit trail is configured"
            );
            return Err(ServiceError::BypassNotRecordable);
        };
        let mut metadata = BTreeMap::new();
        metadata.insert("version_id".into(), version_id.to_string());
        metadata.insert("bypass".into(), "governance".into());
        let event = AuditEvent {
            event_id: AuditEventId::new(),
            timestamp: Utc::now(),
            request_id: None,
            principal: context.principal.clone(),
            credential_id: None,
            source_ip: None,
            operation: operation.to_owned(),
            resource: format!("bucket:{bucket}/{key}"),
            result: AuditResult::Attempted,
            metadata,
        };
        match audit.append(&event).await {
            Ok(()) => Ok(Some(event)),
            Err(error) => {
                warn!(%error, operation, "refusing a governance bypass: its record could not be made durable");
                Err(ServiceError::BypassNotRecordable)
            }
        }
    }

    /// Records what the announced bypass went on to do.
    ///
    /// A failure here leaves the intent standing alone, which reads as "a
    /// bypass was authorized and this server cannot say what came of it" —
    /// the honest state, and one an operator can investigate.
    pub(crate) async fn complete_bypass(&self, intent: Option<AuditEvent>, result: AuditResult) {
        let (Some(audit), Some(intent)) = (&self.audit, intent) else {
            return;
        };
        let outcome = record_store_audit::intent::outcome_event(&intent, result);
        if let Err(error) = audit.append(&outcome).await {
            warn!(
                %error,
                operation = %intent.operation,
                "the governance bypass record stands without its outcome"
            );
        }
    }
}

/// The Object Lock state of one named version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionLock {
    /// Version the state belongs to.
    pub version_id: VersionId,
    /// Retention and legal hold currently in force.
    pub state: ObjectLockState,
}

/// Object Lock service shared by every protocol.
pub struct ObjectLockService {
    pub(crate) metadata: Arc<dyn MetadataRepository>,
    pub(crate) coordinator: Arc<BucketCoordinator>,
    pub(crate) admission: Arc<crate::admission::Admission>,
    pub(crate) metrics: Arc<ServiceMetrics>,
    pub(crate) policy: Arc<LockPolicy>,
}

impl ObjectLockService {
    /// Returns a bucket's Object Lock configuration.
    ///
    /// A bucket without Object Lock is reported as a missing configuration
    /// rather than an empty one, because "never enabled" and "enabled with no
    /// default rule" are different answers and clients branch on them.
    pub async fn bucket_configuration(
        &self,
        bucket_name: &BucketName,
    ) -> Result<ObjectLockConfiguration, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        self.resolve_bucket(bucket_name)
            .await?
            .object_lock
            .ok_or(ServiceError::ObjectLockConfigurationNotFound)
    }

    /// Replaces a bucket's default retention.
    ///
    /// Object Lock itself cannot be turned on here; that only happens when the
    /// bucket is created.
    pub async fn set_bucket_configuration(
        &self,
        bucket_name: &BucketName,
        configuration: ObjectLockConfiguration,
    ) -> Result<Bucket, ServiceError> {
        configuration.validate()?;
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.write().await;
        self.metadata
            .set_bucket_object_lock(bucket.id, configuration)
            .await
            .map_err(map_metadata)
    }

    /// Returns the Object Lock state of one version, current by default.
    pub async fn get(
        &self,
        bucket_name: &BucketName,
        key: &ObjectKey,
        version_id: Option<VersionId>,
    ) -> Result<VersionLock, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        if bucket.object_lock.is_none() {
            return Err(ServiceError::ObjectLockNotEnabled);
        }
        let version_id = self.resolve_version(&bucket, key, version_id).await?;
        let state = self
            .metadata
            .get_object_lock(version_id)
            .await
            .map_err(map_metadata)?;
        Ok(VersionLock { version_id, state })
    }

    /// Returns the Object Lock state of a version already resolved elsewhere.
    ///
    /// Used on the read path, where the version is known and only the response
    /// headers are still missing.
    pub async fn state_of(&self, version_id: VersionId) -> Result<ObjectLockState, ServiceError> {
        self.metadata
            .get_object_lock(version_id)
            .await
            .map_err(map_metadata)
    }

    /// Replaces the retention of one version.
    ///
    /// Extending is always allowed. Shortening or removing is refused outright
    /// in compliance mode, and in governance mode requires an authorized
    /// bypass, which is audited whether it succeeds or not.
    pub async fn put_retention(
        &self,
        bucket_name: &BucketName,
        key: &ObjectKey,
        version_id: Option<VersionId>,
        retention: Option<record_store_core::Retention>,
        context: &LockContext,
    ) -> Result<VersionLock, ServiceError> {
        self.change(
            bucket_name,
            key,
            version_id,
            context,
            "object-lock.put-retention",
            move |current| ObjectLockState {
                retention,
                ..current
            },
        )
        .await
    }

    /// Places or removes the legal hold on one version.
    ///
    /// A hold is independent of retention: it blocks deletion on its own, in
    /// either mode, and no bypass applies to it. Removing one is a release, so
    /// it is judged against the observed-time high-water mark.
    pub async fn put_legal_hold(
        &self,
        bucket_name: &BucketName,
        key: &ObjectKey,
        version_id: Option<VersionId>,
        legal_hold: bool,
        context: &LockContext,
    ) -> Result<VersionLock, ServiceError> {
        self.change(
            bucket_name,
            key,
            version_id,
            context,
            "object-lock.put-legal-hold",
            move |current| current.with_legal_hold(legal_hold),
        )
        .await
    }

    /// Resolves the Object Lock state a version written now is born with.
    ///
    /// An explicit request wins over the bucket default, and a bucket without
    /// Object Lock refuses an explicit request rather than dropping it.
    pub(crate) fn initial_state(
        bucket: &Bucket,
        requested: Option<ObjectLockState>,
    ) -> Result<Option<ObjectLockState>, ServiceError> {
        match (bucket.object_lock, requested) {
            (None, Some(state)) if !state.is_unlocked() => Err(ServiceError::ObjectLockNotEnabled),
            (None, _) => Ok(None),
            (Some(_), Some(state)) => Ok(Some(state)),
            (Some(configuration), None) => Ok(Some(configuration.initial_state_at(Utc::now())?)),
        }
    }

    /// Advances the observed-time high-water mark retention is judged against.
    ///
    /// Called on a timer as well as by lock operations, so that a deployment
    /// that does nothing lock-related for a month still notices a clock that
    /// went backwards while it was idle.
    pub async fn observe_clock(&self) -> Result<(), ServiceError> {
        let release = self.policy.release(&LockContext::system("clock"));
        match self.metadata.observe_clock(release).await {
            Ok(()) => Ok(()),
            Err(record_store_metadata::MetadataError::ClockWentBackwards) => {
                warn!(
                    "system clock is behind the recorded high-water mark; \
                     object lock will refuse to release retained versions until it catches up"
                );
                Err(ServiceError::RetentionClockUnavailable)
            }
            Err(error) => Err(map_metadata(error)),
        }
    }

    async fn change<F>(
        &self,
        bucket_name: &BucketName,
        key: &ObjectKey,
        version_id: Option<VersionId>,
        context: &LockContext,
        operation: &str,
        apply: F,
    ) -> Result<VersionLock, ServiceError>
    where
        F: FnOnce(ObjectLockState) -> ObjectLockState + Send,
    {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        if bucket.object_lock.is_none() {
            return Err(ServiceError::ObjectLockNotEnabled);
        }
        let version_id = self.resolve_version(&bucket, key, version_id).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        let current = self
            .metadata
            .get_object_lock(version_id)
            .await
            .map_err(map_metadata)?;
        let requested = apply(current);
        let release = self.policy.release(context);
        // Announced before the change is attempted, and refused outright when
        // it cannot be announced.
        let intent = self
            .policy
            .begin_bypass(context, operation, bucket_name, key, version_id)
            .await?;
        let result = self
            .metadata
            .put_object_lock(bucket.id, key, version_id, requested, release)
            .await;
        self.policy
            .complete_bypass(
                intent,
                if result.is_ok() {
                    AuditResult::Success
                } else {
                    AuditResult::Denied
                },
            )
            .await;
        let state = result.map_err(map_metadata)?;
        Ok(VersionLock { version_id, state })
    }

    async fn resolve_version(
        &self,
        bucket: &Bucket,
        key: &ObjectKey,
        version_id: Option<VersionId>,
    ) -> Result<VersionId, ServiceError> {
        if let Some(version_id) = version_id {
            return Ok(version_id);
        }
        match self
            .metadata
            .get_object(bucket.id, key)
            .await
            .map_err(map_metadata)?
        {
            Some(metadata) => Ok(metadata.version_id),
            None => Err(ServiceError::ObjectNotFound),
        }
    }

    async fn resolve_bucket(&self, name: &BucketName) -> Result<Bucket, ServiceError> {
        self.metadata
            .get_bucket_by_name(name)
            .await
            .map_err(ServiceError::Metadata)?
            .ok_or(ServiceError::BucketNotFound)
    }

    async fn acquire(&self) -> Result<crate::admission::OperationPermit, ServiceError> {
        self.admission.acquire().await
    }
}

/// How much of the lock table one report reads.
///
/// A report is meant to be read, so it is bounded rather than complete on a
/// deployment with an enormous number of locked versions. Truncation is
/// reported rather than silent: a report that quietly stopped would understate
/// what is retained, which is the one direction that matters.
const RETENTION_REPORT_LIMIT: usize = 10_000;
const RETENTION_REPORT_PAGE: usize = 500;

/// Whether a lock record still holds its version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionStatus {
    /// A retention period has not elapsed, a legal hold is on, or both.
    Held,
    /// A lock record exists but nothing it describes still holds the version.
    Elapsed,
}

/// One bucket with Object Lock enabled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LockedBucket {
    /// Bucket name.
    pub bucket: String,
    /// The default retention new versions are born under, when one is set.
    pub default_retention: Option<record_store_core::DefaultRetention>,
}

/// One version a lock record names.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetainedVersion {
    pub bucket: String,
    pub key: String,
    pub version_id: VersionId,
    /// Retention mode, when a retention was applied.
    pub retention_mode: Option<String>,
    /// When the retention expires, when there is one.
    pub retain_until: Option<chrono::DateTime<Utc>>,
    /// Whether a legal hold is on.
    pub legal_hold: bool,
    /// Whether anything still holds this version.
    pub status: RetentionStatus,
}

/// Which buckets have Object Lock, and what is currently held.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetentionReport {
    /// When the report was produced. Retention is time-relative, so a report
    /// without its own timestamp cannot be interpreted later.
    pub generated_at: chrono::DateTime<Utc>,
    /// Every bucket with Object Lock enabled.
    pub buckets: Vec<LockedBucket>,
    /// Versions with a lock record, held and elapsed alike.
    pub versions: Vec<RetainedVersion>,
    /// Versions still held at `generated_at`.
    pub held_count: u64,
    /// Whether the scan stopped before the end of the lock table.
    pub truncated: bool,
}

impl ObjectLockService {
    /// Reports which buckets have Object Lock and what it currently holds.
    ///
    /// The scan walks the lock table, which contains only locked versions, so a
    /// deployment with a million objects and ten locks pays for ten.
    ///
    /// A lock record outlives the retention it describes, so each version is
    /// classified against the report's own timestamp rather than being reported
    /// as held merely because a record exists.
    pub async fn retention_report(&self) -> Result<RetentionReport, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let generated_at = Utc::now();

        let mut buckets: Vec<LockedBucket> = self
            .metadata
            .list_buckets()
            .await
            .map_err(map_metadata)?
            .into_iter()
            .filter_map(|bucket| {
                bucket.object_lock.map(|configuration| LockedBucket {
                    bucket: bucket.name.to_string(),
                    default_retention: configuration.default_retention,
                })
            })
            .collect();
        buckets.sort_by(|left, right| left.bucket.cmp(&right.bucket));
        let names: BTreeMap<_, _> = self
            .metadata
            .list_buckets()
            .await
            .map_err(map_metadata)?
            .into_iter()
            .map(|bucket| (bucket.id, bucket.name.to_string()))
            .collect();

        let mut versions = Vec::new();
        let mut held_count = 0_u64;
        let mut cursor = None;
        let mut truncated = false;
        loop {
            let page = self
                .metadata
                .list_object_locks(cursor, RETENTION_REPORT_PAGE)
                .await
                .map_err(map_metadata)?;
            for locked in page.versions {
                if versions.len() >= RETENTION_REPORT_LIMIT {
                    truncated = true;
                    break;
                }
                let status = if locked.state.deletion_block_at(generated_at).is_some() {
                    held_count += 1;
                    RetentionStatus::Held
                } else {
                    RetentionStatus::Elapsed
                };
                versions.push(RetainedVersion {
                    bucket: names
                        .get(&locked.bucket_id)
                        .cloned()
                        .unwrap_or_else(|| locked.bucket_id.to_string()),
                    key: locked.key.to_string(),
                    version_id: locked.version_id,
                    retention_mode: locked
                        .state
                        .retention
                        .map(|retention| retention.mode.as_str().to_owned()),
                    retain_until: locked
                        .state
                        .retention
                        .map(|retention| retention.retain_until),
                    legal_hold: locked.state.legal_hold,
                    status,
                });
            }
            match page.next {
                Some(next) if !truncated => cursor = Some(next),
                _ => break,
            }
        }
        versions.sort_by(|left, right| {
            (&left.bucket, &left.key, left.version_id.to_string()).cmp(&(
                &right.bucket,
                &right.key,
                right.version_id.to_string(),
            ))
        });

        Ok(RetentionReport {
            generated_at,
            buckets,
            versions,
            held_count,
            truncated,
        })
    }
}
