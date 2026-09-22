//! Durable single-node metadata catalog.

use chrono::{DateTime, Utc};
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, CorsConfiguration, DeleteMarker, LifecycleRule,
    LifecycleRuleId, MultipartUpload, MultipartUploadState, ObjectId, ObjectKey,
    ObjectLockConfiguration, ObjectLockState, ObjectMetadata, ObjectVersionRecord,
    StorageEventType, UploadId, UploadedPart, VersionId, VersioningState, WriteOrigin,
};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use crate::error::{backend, counter_error};
use crate::keys::{bucket_key, multipart_order_key, object_key, part_key};
use crate::schema::{
    BUCKET_COUNT, BUCKET_NAMES, BUCKET_USAGE, BUCKETS, CLEANUP, LIFECYCLE_RULES, LOGICAL_BYTES,
    MARKERS, MULTIPART, MULTIPART_BYTES, MULTIPART_ORDER, OBJECT_COUNT, OBJECTS, PARTS,
    PHYSICAL_BYTES, VERSION_BYTES, VERSION_COUNT, VERSIONS,
};
use crate::tx::{
    LockRelease, adjust_counter, as_object, clear_null, current_version, enforce_deletable,
    has_multipart, insert_version, journal_event, latest_version, list_parts_tx, observe_clock,
    prune_mutation_events_tx, publish_current, queue_cleanup, read_bucket, read_bucket_usage,
    read_object_lock, read_tx, record_matches, remove_current, remove_object_lock, remove_version,
    set_null, take_null, update_bucket_tx, write_bucket_usage, write_object_lock,
};
use crate::types::BucketUsage;
use crate::*;

/// A newly generated delete marker supplied by the caller.
///
/// Delete markers carry a fresh version identifier and timestamp. Generating
/// them outside the catalog keeps command application deterministic, which is
/// what allows the catalog to be replicated through consensus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewDeleteMarker {
    /// Version identifier the marker will be published under.
    pub version_id: VersionId,
    /// Time the marker becomes current.
    pub created_at: DateTime<Utc>,
}

impl NewDeleteMarker {
    /// Generates a marker for a delete that is about to be proposed.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            version_id: VersionId::new(),
            created_at: Utc::now(),
        }
    }
}

/// One deterministic mutation of the durable object catalog.
///
/// Commands carry every non-deterministic input so that applying the same
/// ordered sequence on any node yields identical state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum MetadataCommand {
    /// Create a bucket.
    CreateBucket {
        /// Bucket to create.
        bucket: Box<Bucket>,
    },
    /// Change a bucket's versioning state.
    SetBucketVersioning {
        /// Bucket to change.
        bucket_id: BucketId,
        /// Requested versioning state.
        state: VersioningState,
    },
    /// Change a bucket's quota.
    SetBucketQuota {
        /// Bucket to change.
        bucket_id: BucketId,
        /// Requested quota.
        quota: BucketQuota,
    },
    /// Replace or remove a bucket's browser CORS configuration.
    SetBucketCors {
        /// Bucket to change.
        bucket_id: BucketId,
        /// Complete replacement configuration, or `None` to remove it.
        configuration: Option<CorsConfiguration>,
    },
    /// Delete an empty bucket.
    DeleteBucket {
        /// Bucket name.
        name: BucketName,
        /// Time the deletion event is recorded at.
        ///
        /// Carried in the command rather than read from the clock at apply time:
        /// the event journal is replicated state, so a member that reads its own
        /// clock would write a different journal than its peers for the same log
        /// entry, and a snapshot taken on one member would not match another's
        /// replayed state. Entries written before this field existed decode as
        /// the epoch, which is visibly wrong rather than quietly divergent.
        #[serde(default)]
        at: DateTime<Utc>,
    },
    /// Replace or remove a bucket's Object Lock default retention.
    SetBucketObjectLock {
        /// Bucket to change. It must already have Object Lock enabled.
        bucket_id: BucketId,
        /// Complete replacement configuration.
        configuration: ObjectLockConfiguration,
    },
    /// Publish an immutable object version and make it current.
    PutObject {
        /// Object metadata to publish.
        metadata: Box<ObjectMetadata>,
        /// Object Lock state the new version is born with.
        ///
        /// It is applied in the same transaction that publishes the version, so
        /// a crash cannot leave a version that was written under retention
        /// durably unretained.
        object_lock: Option<ObjectLockState>,
        /// Why this version is being written.
        ///
        /// A copy, a restore, and a completed multipart upload all publish a
        /// version and are indistinguishable once they arrive, but they are
        /// different events to a subscriber. The caller knows which it is, so
        /// it says; defaulted so a command written before origins existed still
        /// decodes as the ordinary upload it was.
        #[serde(default)]
        origin: WriteOrigin,
    },
    /// Apply ordinary delete semantics for the current version of a key.
    DeleteObject {
        /// Owning bucket.
        bucket_id: BucketId,
        /// Logical key.
        key: ObjectKey,
        /// Marker values to use when the bucket keeps history.
        marker: NewDeleteMarker,
    },
    /// Permanently delete one explicit version.
    DeleteObjectVersion {
        /// Owning bucket.
        bucket_id: BucketId,
        /// Logical key.
        key: ObjectKey,
        /// Version to remove.
        version_id: VersionId,
        /// Clock and bypass inputs for the Object Lock decision.
        release: LockRelease,
    },
    /// Replace the Object Lock state of one explicit version.
    PutObjectLock {
        /// Owning bucket.
        bucket_id: BucketId,
        /// Logical key.
        key: ObjectKey,
        /// Version to change.
        version_id: VersionId,
        /// Complete replacement state.
        requested: ObjectLockState,
        /// Clock and bypass inputs for the Object Lock decision.
        release: LockRelease,
    },
    /// Advance the observed-time high-water mark.
    ObserveClock {
        /// Clock inputs. No bypass applies to an observation.
        release: LockRelease,
    },
    /// Create a multipart upload.
    CreateMultipartUpload {
        /// Upload descriptor.
        upload: Box<MultipartUpload>,
    },
    /// Publish one durable multipart part.
    PutMultipartPart {
        /// Part to publish.
        part: Box<UploadedPart>,
    },
    /// Mark a multipart upload as completing under a preallocated payload.
    BeginMultipartCompletion {
        /// Upload identifier.
        upload_id: UploadId,
        /// Preallocated payload identifier.
        object_id: ObjectId,
    },
    /// Remove multipart state after a successful completion.
    FinishMultipartUpload {
        /// Upload identifier.
        upload_id: UploadId,
    },
    /// Remove multipart state after an abort.
    AbortMultipartUpload {
        /// Upload identifier.
        upload_id: UploadId,
        /// Time the abort event is recorded at. See [`MetadataCommand::DeleteBucket`].
        #[serde(default)]
        at: DateTime<Utc>,
    },
    /// Reconcile multipart uploads interrupted mid-completion.
    RecoverMultipartCompletions,
    /// Create or replace a lifecycle rule.
    PutLifecycleRule {
        /// Rule to store.
        rule: Box<LifecycleRule>,
    },
    /// Remove a lifecycle rule.
    DeleteLifecycleRule {
        /// Rule identifier.
        rule_id: LifecycleRuleId,
    },
    /// Clear a durable payload cleanup record.
    CompleteCleanup {
        /// Payload identifier.
        object_id: ObjectId,
    },
    /// Drop journalled storage events an outbox has already taken.
    ///
    /// A catalog mutation like any other, so a replicated deployment prunes
    /// every member's journal to the same point rather than letting them drift.
    PruneMutationEvents {
        /// Highest sequence that may be removed, inclusive.
        through_sequence: u64,
    },
}

impl MetadataCommand {
    /// Returns a stable short name for tracing, metrics, and audit records.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::CreateBucket { .. } => "create_bucket",
            Self::SetBucketVersioning { .. } => "set_bucket_versioning",
            Self::SetBucketQuota { .. } => "set_bucket_quota",
            Self::SetBucketCors { .. } => "set_bucket_cors",
            Self::SetBucketObjectLock { .. } => "set_bucket_object_lock",
            Self::DeleteBucket { .. } => "delete_bucket",
            Self::PutObject { .. } => "put_object",
            Self::DeleteObject { .. } => "delete_object",
            Self::DeleteObjectVersion { .. } => "delete_object_version",
            Self::PutObjectLock { .. } => "put_object_lock",
            Self::ObserveClock { .. } => "observe_clock",
            Self::CreateMultipartUpload { .. } => "create_multipart_upload",
            Self::PutMultipartPart { .. } => "put_multipart_part",
            Self::BeginMultipartCompletion { .. } => "begin_multipart_completion",
            Self::FinishMultipartUpload { .. } => "finish_multipart_upload",
            Self::AbortMultipartUpload { .. } => "abort_multipart_upload",
            Self::RecoverMultipartCompletions => "recover_multipart_completions",
            Self::PutLifecycleRule { .. } => "put_lifecycle_rule",
            Self::DeleteLifecycleRule { .. } => "delete_lifecycle_rule",
            Self::CompleteCleanup { .. } => "complete_cleanup",
            Self::PruneMutationEvents { .. } => "prune_mutation_events",
        }
    }
}

/// Result of applying one metadata command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum MetadataOutcome {
    /// The command produced no value.
    None,
    /// A bucket record after the command was applied.
    Bucket(Box<Bucket>),
    /// The payloads an object commit made unreachable.
    ObjectCommit(ObjectCommitResult),
    /// The result of an ordinary delete.
    DeleteObject(DeleteObjectResult),
    /// The result of deleting an explicit version.
    ///
    /// Boxed because it is by far the largest outcome and would otherwise set
    /// the size of every command response.
    DeleteVersion(Option<Box<DeleteVersionResult>>),
    /// The Object Lock state of one version after the command was applied.
    ObjectLock(ObjectLockState),
    /// A multipart upload descriptor.
    MultipartUpload(Box<MultipartUpload>),
    /// The part a publish replaced, when it replaced one.
    ReplacedPart(Option<Box<UploadedPart>>),
    /// Multipart state removed by completion, abort, or recovery.
    MultipartCleanup(MultipartCleanupResult),
}

pub(crate) fn unexpected(expected: &'static str) -> MetadataError {
    MetadataError::Database {
        operation: "decode command outcome",
        reason: format!("expected a {expected} outcome"),
    }
}

impl MetadataOutcome {
    /// Returns the bucket produced by the command.
    pub fn into_bucket(self) -> Result<Bucket, MetadataError> {
        match self {
            Self::Bucket(bucket) => Ok(*bucket),
            _ => Err(unexpected("bucket")),
        }
    }

    /// Returns the object commit result produced by the command.
    pub fn into_object_commit(self) -> Result<ObjectCommitResult, MetadataError> {
        match self {
            Self::ObjectCommit(result) => Ok(result),
            _ => Err(unexpected("object commit")),
        }
    }

    /// Returns the delete result produced by the command.
    pub fn into_delete_object(self) -> Result<DeleteObjectResult, MetadataError> {
        match self {
            Self::DeleteObject(result) => Ok(result),
            _ => Err(unexpected("object delete")),
        }
    }

    /// Returns the version delete result produced by the command.
    pub fn into_delete_version(self) -> Result<Option<DeleteVersionResult>, MetadataError> {
        match self {
            Self::DeleteVersion(result) => Ok(result.map(|result| *result)),
            _ => Err(unexpected("version delete")),
        }
    }

    /// Returns the Object Lock state produced by the command.
    pub fn into_object_lock(self) -> Result<ObjectLockState, MetadataError> {
        match self {
            Self::ObjectLock(state) => Ok(state),
            _ => Err(unexpected("object lock")),
        }
    }

    /// Returns the multipart upload produced by the command.
    pub fn into_multipart_upload(self) -> Result<MultipartUpload, MetadataError> {
        match self {
            Self::MultipartUpload(upload) => Ok(*upload),
            _ => Err(unexpected("multipart upload")),
        }
    }

    /// Returns the part a publish replaced.
    pub fn into_replaced_part(self) -> Result<Option<UploadedPart>, MetadataError> {
        match self {
            Self::ReplacedPart(part) => Ok(part.map(|part| *part)),
            _ => Err(unexpected("multipart part")),
        }
    }

    /// Returns the multipart state removed by the command.
    pub fn into_multipart_cleanup(self) -> Result<MultipartCleanupResult, MetadataError> {
        match self {
            Self::MultipartCleanup(result) => Ok(result),
            _ => Err(unexpected("multipart cleanup")),
        }
    }
}

/// Applies one metadata command inside a caller-provided transaction.
///
/// Keeping the transaction external lets a consensus state machine commit the
/// state change and its applied log position atomically, so a crash can neither
/// apply an entry twice nor lose one.
pub fn apply_command_tx(
    write: &redb::WriteTransaction,
    command: MetadataCommand,
) -> Result<MetadataOutcome, MetadataError> {
    match command {
        MetadataCommand::CreateBucket { bucket } => {
            create_bucket_tx(write, &bucket)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::SetBucketVersioning { bucket_id, state } => {
            let bucket = update_bucket_tx(write, bucket_id, move |bucket| {
                if bucket.versioning == VersioningState::Enabled
                    && state == VersioningState::Disabled
                {
                    return Err(MetadataError::InvalidVersioningTransition);
                }
                // Object Lock holds immutable versions. Suspending versioning
                // would make the next write replace the null version in place,
                // which is precisely the history a retained version is meant to
                // be safe from, so the bucket keeps versioning for as long as
                // lock is enabled on it.
                if bucket.object_lock.is_some() && state != VersioningState::Enabled {
                    return Err(MetadataError::ObjectLockRequiresVersioning);
                }
                bucket.versioning = state;
                Ok(())
            })?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::SetBucketObjectLock {
            bucket_id,
            configuration,
        } => {
            let configuration = configuration
                .validate()
                .map_err(|error| MetadataError::InvalidObjectLock(error.to_string()))?;
            let bucket = update_bucket_tx(write, bucket_id, move |bucket| {
                // Enabling lock later would claim protection over versions that
                // were written without it, so the answer is the bucket's whole
                // lifetime or nothing.
                if bucket.object_lock.is_none() {
                    return Err(MetadataError::ObjectLockNotEnabled);
                }
                bucket.object_lock = Some(configuration);
                Ok(())
            })?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::SetBucketQuota { bucket_id, quota } => {
            let usage = read_bucket_usage(write, bucket_id)?;
            if !quota
                .bytes
                .allows(usage.version_bytes.saturating_add(usage.multipart_bytes))
                || !quota.objects.allows(usage.current_objects)
            {
                return Err(MetadataError::QuotaExceeded);
            }
            let bucket = update_bucket_tx(write, bucket_id, move |bucket| {
                bucket.quota = quota;
                Ok(())
            })?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::SetBucketCors {
            bucket_id,
            configuration,
        } => {
            let bucket = update_bucket_tx(write, bucket_id, move |bucket| {
                bucket.cors = configuration;
                Ok(())
            })?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::DeleteBucket { name, at } => {
            let bucket = delete_bucket_tx(write, &name, at)?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::PutObject {
            metadata,
            object_lock,
            origin,
        } => Ok(MetadataOutcome::ObjectCommit(put_object_tx(
            write,
            &metadata,
            object_lock,
            origin,
        )?)),
        MetadataCommand::DeleteObject {
            bucket_id,
            key,
            marker,
        } => Ok(MetadataOutcome::DeleteObject(delete_object_tx(
            write, bucket_id, &key, marker,
        )?)),
        MetadataCommand::DeleteObjectVersion {
            bucket_id,
            key,
            version_id,
            release,
        } => Ok(MetadataOutcome::DeleteVersion(
            delete_object_version_tx(write, bucket_id, &key, version_id, release)?.map(Box::new),
        )),
        MetadataCommand::PutObjectLock {
            bucket_id,
            key,
            version_id,
            requested,
            release,
        } => Ok(MetadataOutcome::ObjectLock(put_object_lock_tx(
            write, bucket_id, &key, version_id, requested, release,
        )?)),
        MetadataCommand::ObserveClock { release } => {
            observe_clock(write, release)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::CreateMultipartUpload { upload } => {
            create_multipart_upload_tx(write, &upload)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::PutMultipartPart { part } => Ok(MetadataOutcome::ReplacedPart(
            put_multipart_part_tx(write, &part)?.map(Box::new),
        )),
        MetadataCommand::BeginMultipartCompletion {
            upload_id,
            object_id,
        } => Ok(MetadataOutcome::MultipartUpload(Box::new(
            begin_multipart_completion_tx(write, upload_id, object_id)?,
        ))),
        MetadataCommand::FinishMultipartUpload { upload_id } => {
            Ok(MetadataOutcome::MultipartCleanup(remove_multipart_tx(
                write,
                upload_id,
                true,
                DateTime::<Utc>::default(),
            )?))
        }
        MetadataCommand::AbortMultipartUpload { upload_id, at } => Ok(
            MetadataOutcome::MultipartCleanup(remove_multipart_tx(write, upload_id, false, at)?),
        ),
        MetadataCommand::RecoverMultipartCompletions => Ok(MetadataOutcome::MultipartCleanup(
            recover_multipart_completions_tx(write)?,
        )),
        MetadataCommand::PutLifecycleRule { rule } => {
            put_lifecycle_rule_tx(write, &rule)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::DeleteLifecycleRule { rule_id } => {
            delete_lifecycle_rule_tx(write, rule_id)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::CompleteCleanup { object_id } => {
            complete_cleanup_tx(write, object_id)?;
            Ok(MetadataOutcome::None)
        }
        MetadataCommand::PruneMutationEvents { through_sequence } => {
            prune_mutation_events_tx(write, through_sequence)?;
            Ok(MetadataOutcome::None)
        }
    }
}

pub(crate) fn create_bucket_tx(
    write: &redb::WriteTransaction,
    bucket: &Bucket,
) -> Result<(), MetadataError> {
    if let Some(configuration) = bucket.object_lock {
        configuration
            .validate()
            .map_err(|error| MetadataError::InvalidObjectLock(error.to_string()))?;
        // Object Lock protects immutable versions, so there has to be version
        // history for it to protect. AWS enables versioning along with lock for
        // the same reason.
        if bucket.versioning != VersioningState::Enabled {
            return Err(MetadataError::ObjectLockRequiresVersioning);
        }
    }
    let key = bucket_key(bucket.id);
    let encoded = serde_json::to_vec(bucket)?;
    {
        let ids = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open buckets", e))?;
        let names = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        if ids
            .get(key.as_slice())
            .map_err(|e| backend("read bucket", e))?
            .is_some()
            || names
                .get(bucket.name.as_str())
                .map_err(|e| backend("read bucket name", e))?
                .is_some()
        {
            return Err(MetadataError::BucketAlreadyExists);
        }
    }
    {
        let mut table = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open buckets", e))?;
        table
            .insert(key.as_slice(), encoded.as_slice())
            .map_err(|e| backend("insert bucket", e))?;
    }
    {
        let mut table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        table
            .insert(bucket.name.as_str(), encoded.as_slice())
            .map_err(|e| backend("index bucket", e))?;
    }
    write_bucket_usage(write, bucket.id, BucketUsage::default())?;
    journal_event(
        write,
        StorageEventType::BucketCreated,
        bucket.name.as_str(),
        None,
        None,
        None,
        bucket.created_at,
    )?;
    adjust_counter(write, BUCKET_COUNT, 1)
}

pub(crate) fn delete_bucket_tx(
    write: &redb::WriteTransaction,
    name: &BucketName,
    at: DateTime<Utc>,
) -> Result<Bucket, MetadataError> {
    let bucket: Bucket = {
        let table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        let bytes = table
            .get(name.as_str())
            .map_err(|e| backend("read bucket", e))?
            .map(|v| v.value().to_vec())
            .ok_or(MetadataError::BucketNotFound)?;
        serde_json::from_slice(&bytes)?
    };
    if read_bucket_usage(write, bucket.id)?.versions != 0 || has_multipart(write, bucket.id)? {
        return Err(MetadataError::BucketNotEmpty);
    }
    {
        let mut table = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open bucket names", e))?;
        table
            .remove(name.as_str())
            .map_err(|e| backend("remove bucket name", e))?;
    }
    {
        let mut table = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open buckets", e))?;
        table
            .remove(bucket_key(bucket.id).as_slice())
            .map_err(|e| backend("remove bucket", e))?;
    }
    {
        let mut table = write
            .open_table(BUCKET_USAGE)
            .map_err(|e| backend("open bucket usage", e))?;
        table
            .remove(bucket_key(bucket.id).as_slice())
            .map_err(|e| backend("remove bucket usage", e))?;
    }
    {
        let mut table = write
            .open_table(LIFECYCLE_RULES)
            .map_err(|e| backend("open lifecycle rules", e))?;
        let mut ids = Vec::new();
        for item in table
            .iter()
            .map_err(|e| backend("iterate lifecycle rules", e))?
        {
            let (id, value) = item.map_err(|e| backend("read lifecycle rule", e))?;
            let rule: LifecycleRule = serde_json::from_slice(value.value())?;
            if rule.bucket_id == bucket.id {
                ids.push(id.value().to_vec());
            }
        }
        for id in ids {
            table
                .remove(id.as_slice())
                .map_err(|e| backend("remove lifecycle rule", e))?;
        }
    }
    adjust_counter(write, BUCKET_COUNT, -1)?;
    journal_event(
        write,
        StorageEventType::BucketDeleted,
        bucket.name.as_str(),
        None,
        None,
        None,
        at,
    )?;
    Ok(bucket)
}

pub(crate) fn put_object_tx(
    write: &redb::WriteTransaction,
    metadata: &ObjectMetadata,
    object_lock: Option<ObjectLockState>,
    origin: WriteOrigin,
) -> Result<ObjectCommitResult, MetadataError> {
    let bucket = read_bucket(write, metadata.bucket_id)?.ok_or(MetadataError::BucketNotFound)?;
    let key = object_key(metadata.bucket_id, &metadata.key);
    let previous: Option<ObjectMetadata> = read_tx(write, OBJECTS, &key, "read current object")?;
    let is_null = bucket.versioning != VersioningState::Enabled;
    let replaced = if is_null {
        take_null(write, metadata.bucket_id, &metadata.key)?
    } else {
        None
    };
    let removed_bytes = replaced.as_ref().and_then(as_object).map_or(0, |m| m.size);
    let mut usage = read_bucket_usage(write, metadata.bucket_id)?;
    let proposed_objects = usage.current_objects + u64::from(previous.is_none());
    let proposed_logical = usage
        .logical_bytes
        .checked_sub(previous.as_ref().map_or(0, |m| m.size))
        .and_then(|v| v.checked_add(metadata.size))
        .ok_or_else(counter_error)?;
    let proposed_versions = usage.versions - u64::from(replaced.is_some()) + 1;
    let proposed_version_bytes = usage
        .version_bytes
        .checked_sub(removed_bytes)
        .and_then(|v| v.checked_add(metadata.size))
        .ok_or_else(counter_error)?;
    if !bucket
        .quota
        .bytes
        .allows(proposed_version_bytes.saturating_add(usage.multipart_bytes))
        || !bucket.quota.objects.allows(proposed_objects)
    {
        return Err(MetadataError::QuotaExceeded);
    }
    if let Some(record) = &replaced {
        // Unreachable by construction: a locked version only exists in a bucket
        // with Object Lock enabled, such a bucket is always version-enabled,
        // and a version-enabled bucket never replaces a null version here. It
        // is asserted rather than assumed because the cost of the assumption
        // being wrong is a silently destroyed retained version.
        let lock = read_object_lock(write, record.version_id())?;
        if let Some(block) = lock.deletion_block_at(metadata.created_at) {
            return Err(MetadataError::VersionLocked(block));
        }
        remove_object_lock(write, record.version_id())?;
        remove_version(write, record)?;
    }
    let record = ObjectVersionRecord::Object {
        metadata: metadata.clone(),
        is_null,
    };
    insert_version(write, &record)?;
    if let Some(state) = object_lock.filter(|state| !state.is_unlocked()) {
        if bucket.object_lock.is_none() {
            return Err(MetadataError::ObjectLockNotEnabled);
        }
        write_object_lock(write, metadata.version_id, state)?;
    }
    if is_null {
        set_null(write, &key, metadata.version_id)?;
    }
    {
        let mut table = write
            .open_table(OBJECTS)
            .map_err(|e| backend("open objects", e))?;
        let bytes = serde_json::to_vec(&metadata)?;
        table
            .insert(key.as_slice(), bytes.as_slice())
            .map_err(|e| backend("publish object", e))?;
    }
    {
        let mut table = write
            .open_table(MARKERS)
            .map_err(|e| backend("open markers", e))?;
        table
            .remove(key.as_slice())
            .map_err(|e| backend("remove marker", e))?;
    }
    usage.current_objects = proposed_objects;
    usage.logical_bytes = proposed_logical;
    usage.versions = proposed_versions;
    usage.version_bytes = proposed_version_bytes;
    write_bucket_usage(write, metadata.bucket_id, usage)?;
    adjust_counter(write, OBJECT_COUNT, i128::from(previous.is_none()))?;
    adjust_counter(
        write,
        LOGICAL_BYTES,
        i128::from(metadata.size) - i128::from(previous.as_ref().map_or(0, |m| m.size)),
    )?;
    adjust_counter(write, VERSION_COUNT, 1 - i128::from(replaced.is_some()))?;
    adjust_counter(
        write,
        VERSION_BYTES,
        i128::from(metadata.size) - i128::from(removed_bytes),
    )?;
    adjust_counter(
        write,
        PHYSICAL_BYTES,
        i128::from(metadata.size) - i128::from(removed_bytes),
    )?;
    let cleanup: Vec<_> = replaced
        .as_ref()
        .and_then(as_object)
        .cloned()
        .into_iter()
        .collect();
    for old in &cleanup {
        queue_cleanup(write, old.id)?;
    }
    // Whether this is a creation or an update is known here, inside the
    // transaction, from the object that was actually replaced. Asking before
    // the write — which is what a caller would have to do — answers a question
    // about a moment that has already passed.
    let created_or_updated = if previous.is_none() {
        StorageEventType::ObjectCreated
    } else {
        StorageEventType::ObjectUpdated
    };
    let journalled = match origin {
        WriteOrigin::MultipartCompletion => vec![StorageEventType::MultipartCompleted],
        // A restore publishes a version *and* is a restore. Both events are
        // emitted because both are true and subscribers already receive both.
        WriteOrigin::Restore => vec![created_or_updated, StorageEventType::ObjectRestored],
        WriteOrigin::Direct | WriteOrigin::Copy => vec![created_or_updated],
    };
    for event_type in journalled {
        journal_event(
            write,
            event_type,
            bucket.name.as_str(),
            Some(&metadata.key),
            Some(metadata.version_id),
            Some(metadata.size),
            metadata.created_at,
        )?;
    }
    Ok(ObjectCommitResult { cleanup })
}

pub(crate) fn delete_object_tx(
    write: &redb::WriteTransaction,
    bucket: BucketId,
    object_key_value: &ObjectKey,
    marker_values: NewDeleteMarker,
) -> Result<DeleteObjectResult, MetadataError> {
    let bucket_record = read_bucket(write, bucket)?.ok_or(MetadataError::BucketNotFound)?;
    let key = object_key(bucket, object_key_value);
    let current: Option<ObjectMetadata> = read_tx(write, OBJECTS, &key, "read current object")?;
    let mut usage = read_bucket_usage(write, bucket)?;
    let mut result = DeleteObjectResult {
        previously_visible: current.is_some(),
        ..DeleteObjectResult::default()
    };
    match bucket_record.versioning {
        VersioningState::Disabled => {
            if let Some(record) = take_null(write, bucket, object_key_value)? {
                assert_unlocked(write, &record, marker_values.created_at)?;
                remove_object_lock(write, record.version_id())?;
                remove_version(write, &record)?;
                usage.versions -= 1;
                adjust_counter(write, VERSION_COUNT, -1)?;
                if let Some(metadata) = as_object(&record) {
                    usage.version_bytes -= metadata.size;
                    adjust_counter(write, VERSION_BYTES, -i128::from(metadata.size))?;
                    adjust_counter(write, PHYSICAL_BYTES, -i128::from(metadata.size))?;
                    queue_cleanup(write, metadata.id)?;
                    result.cleanup.push(metadata.clone());
                }
            }
            remove_current(write, &key)?;
        }
        VersioningState::Enabled | VersioningState::Suspended => {
            let is_null = bucket_record.versioning == VersioningState::Suspended;
            if is_null && let Some(record) = take_null(write, bucket, object_key_value)? {
                assert_unlocked(write, &record, marker_values.created_at)?;
                remove_object_lock(write, record.version_id())?;
                remove_version(write, &record)?;
                usage.versions -= 1;
                adjust_counter(write, VERSION_COUNT, -1)?;
                if let Some(metadata) = as_object(&record) {
                    usage.version_bytes -= metadata.size;
                    adjust_counter(write, VERSION_BYTES, -i128::from(metadata.size))?;
                    adjust_counter(write, PHYSICAL_BYTES, -i128::from(metadata.size))?;
                    queue_cleanup(write, metadata.id)?;
                    result.cleanup.push(metadata.clone());
                }
            }
            let marker = DeleteMarker {
                version_id: marker_values.version_id,
                bucket_id: bucket,
                key: object_key_value.clone(),
                created_at: marker_values.created_at,
            };
            let record = ObjectVersionRecord::DeleteMarker {
                marker: marker.clone(),
                is_null,
            };
            insert_version(write, &record)?;
            if is_null {
                set_null(write, &key, marker.version_id)?;
            }
            {
                let mut objects = write
                    .open_table(OBJECTS)
                    .map_err(|e| backend("open objects", e))?;
                objects
                    .remove(key.as_slice())
                    .map_err(|e| backend("hide object", e))?;
            }
            {
                let mut markers = write
                    .open_table(MARKERS)
                    .map_err(|e| backend("open markers", e))?;
                let bytes = serde_json::to_vec(&marker)?;
                markers
                    .insert(key.as_slice(), bytes.as_slice())
                    .map_err(|e| backend("publish marker", e))?;
            }
            usage.versions += 1;
            adjust_counter(write, VERSION_COUNT, 1)?;
            result.delete_marker = Some(marker);
        }
    }
    if let Some(metadata) = current {
        usage.current_objects -= 1;
        usage.logical_bytes -= metadata.size;
        adjust_counter(write, OBJECT_COUNT, -1)?;
        adjust_counter(write, LOGICAL_BYTES, -i128::from(metadata.size))?;
    }
    write_bucket_usage(write, bucket, usage)?;
    // A delete against a key that was already absent changes nothing, so it
    // owes nothing: announcing it would tell subscribers about a mutation that
    // did not happen.
    if result.previously_visible || result.delete_marker.is_some() {
        journal_event(
            write,
            StorageEventType::ObjectDeleted,
            bucket_record.name.as_str(),
            Some(object_key_value),
            result
                .delete_marker
                .as_ref()
                .map(|marker| marker.version_id),
            None,
            marker_values.created_at,
        )?;
    }
    Ok(result)
}

pub(crate) fn delete_object_version_tx(
    write: &redb::WriteTransaction,
    bucket: BucketId,
    object_key_value: &ObjectKey,
    version: VersionId,
    release: LockRelease,
) -> Result<Option<DeleteVersionResult>, MetadataError> {
    let Some(record): Option<ObjectVersionRecord> = read_tx(
        write,
        VERSIONS,
        version.as_uuid().as_bytes().as_slice(),
        "read version",
    )?
    else {
        return Ok(None);
    };
    if !record_matches(&record, bucket, object_key_value) {
        return Ok(None);
    }
    // Inside the same transaction that removes the version, so a retention
    // placed or extended concurrently cannot land between the two.
    enforce_deletable(write, version, release)?;
    let key = object_key(bucket, object_key_value);
    let was_current = current_version(write, &key)? == Some(version);
    remove_object_lock(write, version)?;
    remove_version(write, &record)?;
    if record.is_null() {
        clear_null(write, &key, version)?;
    }
    let mut usage = read_bucket_usage(write, bucket)?;
    usage.versions -= 1;
    adjust_counter(write, VERSION_COUNT, -1)?;
    let cleanup = as_object(&record).cloned();
    if let Some(metadata) = &cleanup {
        usage.version_bytes -= metadata.size;
        adjust_counter(write, VERSION_BYTES, -i128::from(metadata.size))?;
        adjust_counter(write, PHYSICAL_BYTES, -i128::from(metadata.size))?;
        queue_cleanup(write, metadata.id)?;
    }
    if was_current {
        if let Some(metadata) =
            read_tx::<ObjectMetadata>(write, OBJECTS, &key, "read current object")?
        {
            usage.current_objects -= 1;
            usage.logical_bytes -= metadata.size;
            adjust_counter(write, OBJECT_COUNT, -1)?;
            adjust_counter(write, LOGICAL_BYTES, -i128::from(metadata.size))?;
        }
        remove_current(write, &key)?;
        if let Some(next) = latest_version(write, bucket, object_key_value)? {
            publish_current(write, &key, &next)?;
            if let Some(metadata) = as_object(&next) {
                usage.current_objects += 1;
                usage.logical_bytes += metadata.size;
                adjust_counter(write, OBJECT_COUNT, 1)?;
                adjust_counter(write, LOGICAL_BYTES, i128::from(metadata.size))?;
            }
        }
    }
    write_bucket_usage(write, bucket, usage)?;
    if let Some(bucket_record) = read_bucket(write, bucket)? {
        journal_event(
            write,
            StorageEventType::ObjectDeleted,
            bucket_record.name.as_str(),
            Some(object_key_value),
            Some(version),
            None,
            release.observed_at,
        )?;
    }
    Ok(Some(DeleteVersionResult {
        removed: record,
        cleanup,
    }))
}

/// Refuses to destroy a version that Object Lock still holds.
///
/// Used on the paths that replace or remove the special null version, which a
/// bucket with Object Lock enabled can never reach, because such a bucket is
/// always version-enabled. The check makes that reasoning enforced rather than
/// merely true.
fn assert_unlocked(
    write: &redb::WriteTransaction,
    record: &ObjectVersionRecord,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), MetadataError> {
    match read_object_lock(write, record.version_id())?.deletion_block_at(now) {
        Some(block) => Err(MetadataError::VersionLocked(block)),
        None => Ok(()),
    }
}

/// Replaces the Object Lock state of one version, enforcing the mode rules.
///
/// The clock high-water mark is consulted for any change that could release
/// something. A change that only ever adds protection is applied even when the
/// clock is not trusted, because getting that wrong can only over-retain.
pub(crate) fn put_object_lock_tx(
    write: &redb::WriteTransaction,
    bucket: BucketId,
    object_key_value: &ObjectKey,
    version: VersionId,
    requested: ObjectLockState,
    release: LockRelease,
) -> Result<ObjectLockState, MetadataError> {
    let bucket_record = read_bucket(write, bucket)?.ok_or(MetadataError::BucketNotFound)?;
    if bucket_record.object_lock.is_none() {
        return Err(MetadataError::ObjectLockNotEnabled);
    }
    let Some(record): Option<ObjectVersionRecord> = read_tx(
        write,
        VERSIONS,
        version.as_uuid().as_bytes().as_slice(),
        "read version",
    )?
    else {
        return Err(MetadataError::ObjectLockVersionNotFound);
    };
    if !record_matches(&record, bucket, object_key_value) {
        return Err(MetadataError::ObjectLockVersionNotFound);
    }
    // A delete marker holds no payload, so there is nothing for a retention to
    // protect and AWS does not accept one either.
    if record.is_delete_marker() {
        return Err(MetadataError::ObjectLockVersionNotFound);
    }
    let current = read_object_lock(write, version)?;
    if !current.change_only_tightens(&requested) {
        observe_clock(write, release)?;
    }
    let updated = current
        .with_retention(
            requested.retention,
            release.observed_at,
            release.bypass_governance,
        )
        .map_err(MetadataError::ObjectLockChangeRefused)?
        .with_legal_hold(requested.legal_hold);
    write_object_lock(write, version, updated)?;
    Ok(updated)
}

pub(crate) fn create_multipart_upload_tx(
    write: &redb::WriteTransaction,
    upload: &MultipartUpload,
) -> Result<(), MetadataError> {
    if read_bucket(write, upload.bucket_id)?.is_none() {
        return Err(MetadataError::BucketNotFound);
    }
    let id = upload.id.as_uuid().as_bytes().to_vec();
    {
        let mut table = write
            .open_table(MULTIPART)
            .map_err(|e| backend("open multipart", e))?;
        if table
            .get(id.as_slice())
            .map_err(|e| backend("read multipart", e))?
            .is_some()
        {
            return Err(MetadataError::MultipartStateConflict);
        }
        let bytes = serde_json::to_vec(upload)?;
        table
            .insert(id.as_slice(), bytes.as_slice())
            .map_err(|e| backend("insert multipart", e))?;
    }
    {
        let mut table = write
            .open_table(MULTIPART_ORDER)
            .map_err(|e| backend("open multipart order", e))?;
        table
            .insert(multipart_order_key(upload).as_slice(), id.as_slice())
            .map_err(|e| backend("index multipart", e))?;
    }
    Ok(())
}

pub(crate) fn put_multipart_part_tx(
    write: &redb::WriteTransaction,
    part: &UploadedPart,
) -> Result<Option<UploadedPart>, MetadataError> {
    let upload: MultipartUpload = read_tx(
        write,
        MULTIPART,
        part.upload_id.as_uuid().as_bytes().as_slice(),
        "read multipart",
    )?
    .ok_or(MetadataError::MultipartUploadNotFound)?;
    if upload.state != MultipartUploadState::Active {
        return Err(MetadataError::MultipartStateConflict);
    }
    let bucket = read_bucket(write, upload.bucket_id)?.ok_or(MetadataError::BucketNotFound)?;
    let key = part_key(part.upload_id, part.number);
    let previous: Option<UploadedPart> = read_tx(write, PARTS, &key, "read part")?;
    let mut usage = read_bucket_usage(write, upload.bucket_id)?;
    let proposed = usage
        .multipart_bytes
        .checked_sub(previous.as_ref().map_or(0, |p| p.size))
        .and_then(|v| v.checked_add(part.size))
        .ok_or_else(counter_error)?;
    if !bucket
        .quota
        .bytes
        .allows(usage.version_bytes.saturating_add(proposed))
    {
        return Err(MetadataError::QuotaExceeded);
    }
    {
        let mut table = write
            .open_table(PARTS)
            .map_err(|e| backend("open parts", e))?;
        let bytes = serde_json::to_vec(part)?;
        table
            .insert(key.as_slice(), bytes.as_slice())
            .map_err(|e| backend("insert part", e))?;
    }
    usage.multipart_bytes = proposed;
    write_bucket_usage(write, upload.bucket_id, usage)?;
    adjust_counter(
        write,
        MULTIPART_BYTES,
        i128::from(part.size) - i128::from(previous.as_ref().map_or(0, |p| p.size)),
    )?;
    if let Some(old) = &previous {
        queue_cleanup(write, old.object_id)?;
    }
    Ok(previous)
}

pub(crate) fn begin_multipart_completion_tx(
    write: &redb::WriteTransaction,
    id: UploadId,
    object_id: ObjectId,
) -> Result<MultipartUpload, MetadataError> {
    let mut upload: MultipartUpload = read_tx(
        write,
        MULTIPART,
        id.as_uuid().as_bytes().as_slice(),
        "read multipart",
    )?
    .ok_or(MetadataError::MultipartUploadNotFound)?;
    match upload.state {
        MultipartUploadState::Active => {
            upload.state = MultipartUploadState::Completing { object_id }
        }
        MultipartUploadState::Completing {
            object_id: existing,
        } if existing == object_id => {}
        MultipartUploadState::Completing { .. } => {
            return Err(MetadataError::MultipartStateConflict);
        }
    }
    {
        let mut table = write
            .open_table(MULTIPART)
            .map_err(|e| backend("open multipart", e))?;
        let bytes = serde_json::to_vec(&upload)?;
        table
            .insert(id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
            .map_err(|e| backend("mark completion", e))?;
    }
    Ok(upload)
}

pub(crate) fn remove_multipart_tx(
    write: &redb::WriteTransaction,
    id: UploadId,
    require_completing: bool,
    at: DateTime<Utc>,
) -> Result<MultipartCleanupResult, MetadataError> {
    let upload: MultipartUpload = read_tx(
        write,
        MULTIPART,
        id.as_uuid().as_bytes().as_slice(),
        "read multipart",
    )?
    .ok_or(MetadataError::MultipartUploadNotFound)?;
    if require_completing && !matches!(upload.state, MultipartUploadState::Completing { .. }) {
        return Err(MetadataError::MultipartStateConflict);
    }
    let parts = list_parts_tx(write, id)?;
    {
        let mut table = write
            .open_table(MULTIPART)
            .map_err(|e| backend("open multipart", e))?;
        table
            .remove(id.as_uuid().as_bytes().as_slice())
            .map_err(|e| backend("remove multipart", e))?;
    }
    {
        let mut table = write
            .open_table(MULTIPART_ORDER)
            .map_err(|e| backend("open multipart order", e))?;
        table
            .remove(multipart_order_key(&upload).as_slice())
            .map_err(|e| backend("remove multipart index", e))?;
    }
    {
        let mut table = write
            .open_table(PARTS)
            .map_err(|e| backend("open parts", e))?;
        for part in &parts {
            table
                .remove(part_key(id, part.number).as_slice())
                .map_err(|e| backend("remove part", e))?;
        }
    }
    let bytes = parts.iter().try_fold(0_u64, |sum, part| {
        sum.checked_add(part.size).ok_or_else(counter_error)
    })?;
    let mut usage = read_bucket_usage(write, upload.bucket_id)?;
    usage.multipart_bytes = usage
        .multipart_bytes
        .checked_sub(bytes)
        .ok_or_else(counter_error)?;
    write_bucket_usage(write, upload.bucket_id, usage)?;
    adjust_counter(write, MULTIPART_BYTES, -i128::from(bytes))?;
    for part in &parts {
        queue_cleanup(write, part.object_id)?;
    }
    // A completion's event belongs to the version it published, which the
    // PutObject that preceded this already journalled. Only an abort has an
    // event of its own.
    if !require_completing {
        journal_event(
            write,
            StorageEventType::MultipartAborted,
            &bucket_name_of(write, upload.bucket_id)?,
            Some(&upload.key),
            None,
            None,
            at,
        )?;
    }
    Ok(MultipartCleanupResult { parts })
}

pub(crate) fn recover_multipart_completions_tx(
    write: &redb::WriteTransaction,
) -> Result<MultipartCleanupResult, MetadataError> {
    let uploads = {
        let table = write
            .open_table(MULTIPART)
            .map_err(|e| backend("open multipart", e))?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(|e| backend("iterate multipart", e))? {
            let (_, value) = entry.map_err(|e| backend("read multipart", e))?;
            let upload: MultipartUpload = serde_json::from_slice(value.value())?;
            if matches!(upload.state, MultipartUploadState::Completing { .. }) {
                out.push(upload);
            }
        }
        out
    };
    let mut cleaned = Vec::new();
    for upload in uploads {
        let MultipartUploadState::Completing { object_id } = upload.state else {
            continue;
        };
        let key = object_key(upload.bucket_id, &upload.key);
        let committed = read_tx::<ObjectMetadata>(write, OBJECTS, &key, "read current object")?
            .is_some_and(|metadata| metadata.id == object_id);
        if committed {
            // A completing upload is finished, not aborted, so this path
            // journals nothing and needs no timestamp of its own.
            cleaned.extend(
                remove_multipart_tx(write, upload.id, true, DateTime::<Utc>::default())?.parts,
            );
        } else {
            let mut reset = upload;
            reset.state = MultipartUploadState::Active;
            let mut table = write
                .open_table(MULTIPART)
                .map_err(|e| backend("open multipart", e))?;
            let bytes = serde_json::to_vec(&reset)?;
            table
                .insert(reset.id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
                .map_err(|e| backend("reset completion", e))?;
        }
    }
    Ok(MultipartCleanupResult { parts: cleaned })
}

pub(crate) fn put_lifecycle_rule_tx(
    write: &redb::WriteTransaction,
    rule: &LifecycleRule,
) -> Result<(), MetadataError> {
    rule.validate()
        .map_err(|error| MetadataError::InvalidLifecycleRule(error.to_string()))?;
    if read_bucket(write, rule.bucket_id)?.is_none() {
        return Err(MetadataError::BucketNotFound);
    }
    let mut table = write
        .open_table(LIFECYCLE_RULES)
        .map_err(|e| backend("open lifecycle rules", e))?;
    let bytes = serde_json::to_vec(rule)?;
    table
        .insert(rule.id.as_uuid().as_bytes().as_slice(), bytes.as_slice())
        .map_err(|e| backend("put lifecycle rule", e))?;
    Ok(())
}

pub(crate) fn delete_lifecycle_rule_tx(
    write: &redb::WriteTransaction,
    id: LifecycleRuleId,
) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(LIFECYCLE_RULES)
        .map_err(|e| backend("open lifecycle rules", e))?;
    if table
        .remove(id.as_uuid().as_bytes().as_slice())
        .map_err(|e| backend("delete lifecycle rule", e))?
        .is_none()
    {
        return Err(MetadataError::LifecycleRuleNotFound);
    }
    Ok(())
}

pub(crate) fn complete_cleanup_tx(
    write: &redb::WriteTransaction,
    id: ObjectId,
) -> Result<(), MetadataError> {
    let mut table = write
        .open_table(CLEANUP)
        .map_err(|e| backend("open cleanup", e))?;
    table
        .remove(id.as_uuid().as_bytes().as_slice())
        .map_err(|e| backend("complete cleanup", e))?;
    Ok(())
}

/// Returns a bucket's name, for the events a mutation of it owes.
///
/// A bucket that has already gone leaves its identifier: an event naming
/// nothing at all would be worse than one naming what the catalog still has.
fn bucket_name_of(
    write: &redb::WriteTransaction,
    bucket_id: BucketId,
) -> Result<String, MetadataError> {
    Ok(read_bucket(write, bucket_id)?
        .map_or_else(|| bucket_id.to_string(), |bucket| bucket.name.to_string()))
}

#[cfg(test)]
mod tests {
    use record_store_core::VersioningState;
    use redb::ReadableDatabase;
    use tempfile::tempdir;

    use super::*;
    use crate::RedbMetadataRepository;
    use crate::test_support::*;
    use record_store_core::{BucketId, BucketName, ObjectKey};

    #[tokio::test]
    async fn applying_the_same_commands_produces_identical_state() {
        // Determinism is what makes the catalog usable as a consensus state
        // machine: two members replaying one command sequence must agree.
        let bucket_record = bucket("deterministic");
        let first_object = object(bucket_record.id, "alpha", 10);
        let second_object = object(bucket_record.id, "beta", 20);
        let marker = NewDeleteMarker::generate();
        let commands = vec![
            MetadataCommand::CreateBucket {
                bucket: Box::new(bucket_record.clone()),
            },
            MetadataCommand::SetBucketVersioning {
                bucket_id: bucket_record.id,
                state: VersioningState::Enabled,
            },
            MetadataCommand::SetBucketCors {
                bucket_id: bucket_record.id,
                configuration: Some(cors_configuration()),
            },
            MetadataCommand::PutObject {
                metadata: Box::new(first_object.clone()),
                object_lock: None,
                origin: WriteOrigin::Direct,
            },
            MetadataCommand::PutObject {
                metadata: Box::new(second_object.clone()),
                object_lock: None,
                origin: WriteOrigin::Direct,
            },
            MetadataCommand::DeleteObject {
                bucket_id: bucket_record.id,
                key: first_object.key.clone(),
                marker,
            },
        ];

        let mut snapshots = Vec::new();
        for _ in 0..2 {
            let dir = tempdir().expect("temp");
            let repo = RedbMetadataRepository::open(dir.path().join("catalog.redb"))
                .await
                .expect("repo");
            for command in commands.clone() {
                repo.command(command).await.expect("apply command");
            }
            let database = repo.database();
            let entries = tokio::task::spawn_blocking(move || {
                let read = database.begin_read().expect("begin");
                export_tx(&read).expect("export")
            })
            .await
            .expect("join");
            snapshots.push(entries);
        }
        assert_eq!(
            snapshots[0], snapshots[1],
            "replaying identical commands must produce identical durable state"
        );
    }

    /// The commands that write to the event journal are the ones that used to
    /// read the wall clock while applying, which made two members produce
    /// different durable state for the same log entry. The determinism test
    /// above never reached them, which is precisely why it went unnoticed, so
    /// they get their own coverage here.
    ///
    /// Divergence here is not cosmetic: the journal is exported in snapshots, so
    /// a member that installs a snapshot would hold different bytes than a member
    /// that replayed the log, and the two would disagree about what happened.
    #[tokio::test]
    async fn journalling_commands_are_deterministic_across_members() {
        let bucket_record = bucket("journalled");
        let upload_record = upload(bucket_record.id, "interrupted");
        // One fixed instant, decided by the proposer and carried in the command,
        // stands in for the leader's clock at proposal time.
        let at = DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
        let commands = vec![
            MetadataCommand::CreateBucket {
                bucket: Box::new(bucket_record.clone()),
            },
            MetadataCommand::CreateMultipartUpload {
                upload: Box::new(upload_record.clone()),
            },
            MetadataCommand::AbortMultipartUpload {
                upload_id: upload_record.id,
                at,
            },
            MetadataCommand::DeleteBucket {
                name: bucket_record.name.clone(),
                at,
            },
        ];

        let mut snapshots = Vec::new();
        for _ in 0..2 {
            let dir = tempdir().expect("temp");
            let repo = RedbMetadataRepository::open(dir.path().join("catalog.redb"))
                .await
                .expect("repo");
            for command in commands.clone() {
                repo.command(command).await.expect("apply command");
            }
            let database = repo.database();
            let entries = tokio::task::spawn_blocking(move || {
                let read = database.begin_read().expect("begin");
                export_tx(&read).expect("export")
            })
            .await
            .expect("join");
            snapshots.push(entries);
            // A gap between the two replays is what a clock read would turn into
            // a difference. Applying the same commands must not care.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            snapshots[0], snapshots[1],
            "a command that journals an event must record the timestamp it carries, not the              clock of whichever member is applying it"
        );
    }

    /// Applies a command sequence to a fresh database and returns the outcomes.
    fn apply_all(
        database: &redb::Database,
        commands: Vec<MetadataCommand>,
    ) -> Vec<MetadataOutcome> {
        let write = database.begin_write().expect("write transaction");
        let outcomes = commands
            .into_iter()
            .map(|command| apply_command_tx(&write, command).expect("apply"))
            .collect();
        write.commit().expect("commit");
        outcomes
    }

    async fn database() -> (tempfile::TempDir, redb::Database) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("metadata.redb");
        RedbMetadataRepository::open(&path)
            .await
            .expect("initialise schema");
        let database = redb::Database::open(&path).expect("open");
        (directory, database)
    }

    /// Every command carries a stable short name used in tracing and metrics.
    /// A rename would silently break an operator's dashboards.
    #[test]
    fn every_command_has_a_stable_name() {
        let bucket_record = bucket("named");
        let cases = [
            (
                MetadataCommand::CreateBucket {
                    bucket: Box::new(bucket_record.clone()),
                },
                "create_bucket",
            ),
            (
                MetadataCommand::DeleteBucket {
                    name: bucket_record.name.clone(),
                    at: Utc::now(),
                },
                "delete_bucket",
            ),
            (
                MetadataCommand::PutObject {
                    metadata: Box::new(object(bucket_record.id, "a", 1)),
                    object_lock: None,
                    origin: WriteOrigin::Direct,
                },
                "put_object",
            ),
            (
                MetadataCommand::RecoverMultipartCompletions,
                "recover_multipart_completions",
            ),
        ];
        for (command, expected) in cases {
            assert_eq!(command.name(), expected);
        }
    }

    /// Commands cross the replication log, so each one has to survive the same
    /// encoding a follower decodes it with.
    #[test]
    fn commands_round_trip_through_their_encoded_form() {
        let bucket_record = bucket("encoded");
        let commands = vec![
            MetadataCommand::CreateBucket {
                bucket: Box::new(bucket_record.clone()),
            },
            MetadataCommand::SetBucketVersioning {
                bucket_id: bucket_record.id,
                state: VersioningState::Enabled,
            },
            MetadataCommand::SetBucketCors {
                bucket_id: bucket_record.id,
                configuration: Some(cors_configuration()),
            },
            MetadataCommand::PutObject {
                metadata: Box::new(object(bucket_record.id, "a", 1)),
                object_lock: None,
                origin: WriteOrigin::Direct,
            },
            MetadataCommand::DeleteObject {
                bucket_id: bucket_record.id,
                key: ObjectKey::new("a").expect("key"),
                marker: NewDeleteMarker::generate(),
            },
            MetadataCommand::RecoverMultipartCompletions,
        ];
        for command in commands {
            let name = command.name();
            let encoded = serde_json::to_vec(&command).expect("serialise");
            let decoded: MetadataCommand = serde_json::from_slice(&encoded).expect("deserialise");
            assert_eq!(decoded.name(), name);
        }
    }

    /// Bucket settings applied as commands must be visible in the stored record,
    /// because this is the only path a replicated deployment writes them by.
    #[tokio::test]
    async fn bucket_settings_applied_as_commands_are_stored() {
        let (_directory, db) = database().await;
        let bucket_record = bucket("settings");
        apply_all(
            &db,
            vec![
                MetadataCommand::CreateBucket {
                    bucket: Box::new(bucket_record.clone()),
                },
                MetadataCommand::SetBucketVersioning {
                    bucket_id: bucket_record.id,
                    state: VersioningState::Enabled,
                },
                MetadataCommand::SetBucketQuota {
                    bucket_id: bucket_record.id,
                    quota: record_store_core::BucketQuota {
                        bytes: record_store_core::ByteQuota::Limit(2_048),
                        objects: record_store_core::ObjectCountQuota::Limit(5),
                    },
                },
                MetadataCommand::SetBucketCors {
                    bucket_id: bucket_record.id,
                    configuration: Some(cors_configuration()),
                },
            ],
        );

        let write = db.begin_write().expect("write");
        let MetadataOutcome::Bucket(stored) = apply_command_tx(
            &write,
            MetadataCommand::SetBucketVersioning {
                bucket_id: bucket_record.id,
                state: VersioningState::Suspended,
            },
        )
        .expect("apply") else {
            panic!("a versioning change returns the bucket");
        };
        assert_eq!(stored.versioning, VersioningState::Suspended);
        assert_eq!(
            stored.quota.bytes,
            record_store_core::ByteQuota::Limit(2_048)
        );
        assert!(stored.cors.is_some());
    }

    /// A command naming a bucket that does not exist has to be rejected rather
    /// than creating one implicitly, or a replayed log could resurrect state an
    /// operator deleted.
    #[tokio::test]
    async fn commands_against_an_absent_bucket_are_rejected() {
        let (_directory, db) = database().await;
        let write = db.begin_write().expect("write");
        let absent = BucketId::new();

        assert!(matches!(
            apply_command_tx(
                &write,
                MetadataCommand::SetBucketVersioning {
                    bucket_id: absent,
                    state: VersioningState::Enabled,
                },
            ),
            Err(MetadataError::BucketNotFound)
        ));
        assert!(matches!(
            apply_command_tx(
                &write,
                MetadataCommand::DeleteBucket {
                    name: BucketName::new("never-created").expect("name"),
                    at: Utc::now(),
                },
            ),
            Err(MetadataError::BucketNotFound)
        ));
    }

    /// Multipart completion is two-phase so a crash between the phases can be
    /// recovered. Both phases and the recovery sweep are exercised here.
    #[tokio::test]
    async fn a_multipart_upload_completes_through_its_two_phases() {
        let (_directory, db) = database().await;
        let bucket_record = bucket("multipart");
        let upload_record = upload(bucket_record.id, "big.bin");
        let stored_part = part(upload_record.id, 1, 1_024);
        let committed = object(bucket_record.id, "big.bin", 1_024);

        let outcomes = apply_all(
            &db,
            vec![
                MetadataCommand::CreateBucket {
                    bucket: Box::new(bucket_record.clone()),
                },
                MetadataCommand::CreateMultipartUpload {
                    upload: Box::new(upload_record.clone()),
                },
                MetadataCommand::PutMultipartPart {
                    part: Box::new(stored_part.clone()),
                },
                MetadataCommand::BeginMultipartCompletion {
                    upload_id: upload_record.id,
                    object_id: committed.id,
                },
                MetadataCommand::PutObject {
                    metadata: Box::new(committed.clone()),
                    object_lock: None,
                    origin: WriteOrigin::Direct,
                },
                MetadataCommand::FinishMultipartUpload {
                    upload_id: upload_record.id,
                },
            ],
        );
        assert!(matches!(outcomes[1], MetadataOutcome::None));
        assert!(
            matches!(outcomes[2], MetadataOutcome::ReplacedPart(None)),
            "a first upload of a part number replaces nothing"
        );
        assert!(matches!(outcomes[3], MetadataOutcome::MultipartUpload(_)));

        let write = db.begin_write().expect("write");
        let recovered = apply_command_tx(&write, MetadataCommand::RecoverMultipartCompletions)
            .expect("recover");
        let MetadataOutcome::MultipartCleanup(cleanup) = recovered else {
            panic!("recovery reports what it swept up");
        };
        assert!(
            cleanup.parts.is_empty(),
            "a completed upload leaves nothing to recover: {cleanup:?}"
        );
    }

    /// Storing the same part number twice replaces it and reports what it
    /// replaced, so the caller can release the payload it no longer owns.
    #[tokio::test]
    async fn re_uploading_a_part_reports_the_payload_it_replaced() {
        let (_directory, db) = database().await;
        let bucket_record = bucket("replacement");
        let upload_record = upload(bucket_record.id, "big.bin");
        let first = part(upload_record.id, 1, 1_024);
        let mut second = part(upload_record.id, 1, 2_048);
        second.object_id = record_store_core::ObjectId::new();

        apply_all(
            &db,
            vec![
                MetadataCommand::CreateBucket {
                    bucket: Box::new(bucket_record.clone()),
                },
                MetadataCommand::CreateMultipartUpload {
                    upload: Box::new(upload_record.clone()),
                },
                MetadataCommand::PutMultipartPart {
                    part: Box::new(first.clone()),
                },
            ],
        );

        let write = db.begin_write().expect("write");
        let outcome = apply_command_tx(
            &write,
            MetadataCommand::PutMultipartPart {
                part: Box::new(second),
            },
        )
        .expect("apply");
        let MetadataOutcome::ReplacedPart(Some(replaced)) = outcome else {
            panic!("re-uploading a part must report the one it replaced");
        };
        assert_eq!(replaced.object_id, first.object_id);
    }

    #[tokio::test]
    async fn aborting_an_upload_reports_the_payloads_to_release() {
        let (_directory, db) = database().await;
        let bucket_record = bucket("aborted");
        let upload_record = upload(bucket_record.id, "big.bin");
        let stored_part = part(upload_record.id, 1, 1_024);

        apply_all(
            &db,
            vec![
                MetadataCommand::CreateBucket {
                    bucket: Box::new(bucket_record.clone()),
                },
                MetadataCommand::CreateMultipartUpload {
                    upload: Box::new(upload_record.clone()),
                },
                MetadataCommand::PutMultipartPart {
                    part: Box::new(stored_part.clone()),
                },
            ],
        );

        let write = db.begin_write().expect("write");
        let outcome = apply_command_tx(
            &write,
            MetadataCommand::AbortMultipartUpload {
                upload_id: upload_record.id,
                at: Utc::now(),
            },
        )
        .expect("apply");
        let MetadataOutcome::MultipartCleanup(cleanup) = outcome else {
            panic!("aborting must report what to clean up");
        };
        assert!(
            cleanup
                .parts
                .iter()
                .any(|part| part.object_id == stored_part.object_id),
            "{cleanup:?}"
        );
    }

    /// Lifecycle rules and cleanup completion are ordinary replicated commands,
    /// so they have to behave identically when applied through the log.
    #[tokio::test]
    async fn lifecycle_and_cleanup_commands_apply() {
        let (_directory, db) = database().await;
        let bucket_record = bucket("lifecycle");
        let rule = record_store_core::LifecycleRule {
            id: record_store_core::LifecycleRuleId::new(),
            bucket_id: bucket_record.id,
            prefix: "logs/".to_owned(),
            enabled: true,
            expiration: Some(record_store_core::ExpirationDays::new(7).expect("days")),
            noncurrent_version_expiration: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let stored = object(bucket_record.id, "logs/a", 8);

        apply_all(
            &db,
            vec![
                MetadataCommand::CreateBucket {
                    bucket: Box::new(bucket_record.clone()),
                },
                MetadataCommand::PutLifecycleRule {
                    rule: Box::new(rule.clone()),
                },
                MetadataCommand::PutObject {
                    metadata: Box::new(stored.clone()),
                    object_lock: None,
                    origin: WriteOrigin::Direct,
                },
                MetadataCommand::DeleteObject {
                    bucket_id: bucket_record.id,
                    key: stored.key.clone(),
                    marker: NewDeleteMarker::generate(),
                },
                MetadataCommand::CompleteCleanup {
                    object_id: stored.id,
                },
                MetadataCommand::DeleteLifecycleRule { rule_id: rule.id },
            ],
        );

        let write = db.begin_write().expect("write");
        assert!(matches!(
            apply_command_tx(
                &write,
                MetadataCommand::DeleteLifecycleRule { rule_id: rule.id }
            ),
            Err(MetadataError::LifecycleRuleNotFound)
        ));
    }
}
