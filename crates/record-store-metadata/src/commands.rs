//! Durable single-node metadata catalog.

use chrono::{DateTime, Utc};
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, CorsConfiguration, DeleteMarker, LifecycleRule,
    LifecycleRuleId, MultipartUpload, MultipartUploadState, ObjectId, ObjectKey, ObjectMetadata,
    ObjectVersionRecord, UploadId, UploadedPart, VersionId, VersioningState,
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
    adjust_counter, as_object, clear_null, current_version, has_multipart, insert_version,
    latest_version, list_parts_tx, publish_current, queue_cleanup, read_bucket, read_bucket_usage,
    read_tx, record_matches, remove_current, remove_version, set_null, take_null, update_bucket_tx,
    write_bucket_usage,
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
    },
    /// Publish an immutable object version and make it current.
    PutObject {
        /// Object metadata to publish.
        metadata: Box<ObjectMetadata>,
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
            Self::DeleteBucket { .. } => "delete_bucket",
            Self::PutObject { .. } => "put_object",
            Self::DeleteObject { .. } => "delete_object",
            Self::DeleteObjectVersion { .. } => "delete_object_version",
            Self::CreateMultipartUpload { .. } => "create_multipart_upload",
            Self::PutMultipartPart { .. } => "put_multipart_part",
            Self::BeginMultipartCompletion { .. } => "begin_multipart_completion",
            Self::FinishMultipartUpload { .. } => "finish_multipart_upload",
            Self::AbortMultipartUpload { .. } => "abort_multipart_upload",
            Self::RecoverMultipartCompletions => "recover_multipart_completions",
            Self::PutLifecycleRule { .. } => "put_lifecycle_rule",
            Self::DeleteLifecycleRule { .. } => "delete_lifecycle_rule",
            Self::CompleteCleanup { .. } => "complete_cleanup",
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
                bucket.versioning = state;
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
        MetadataCommand::DeleteBucket { name } => {
            let bucket = delete_bucket_tx(write, &name)?;
            Ok(MetadataOutcome::Bucket(Box::new(bucket)))
        }
        MetadataCommand::PutObject { metadata } => Ok(MetadataOutcome::ObjectCommit(
            put_object_tx(write, &metadata)?,
        )),
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
        } => Ok(MetadataOutcome::DeleteVersion(
            delete_object_version_tx(write, bucket_id, &key, version_id)?.map(Box::new),
        )),
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
        MetadataCommand::FinishMultipartUpload { upload_id } => Ok(
            MetadataOutcome::MultipartCleanup(remove_multipart_tx(write, upload_id, true)?),
        ),
        MetadataCommand::AbortMultipartUpload { upload_id } => Ok(
            MetadataOutcome::MultipartCleanup(remove_multipart_tx(write, upload_id, false)?),
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
    }
}

pub(crate) fn create_bucket_tx(
    write: &redb::WriteTransaction,
    bucket: &Bucket,
) -> Result<(), MetadataError> {
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
    adjust_counter(write, BUCKET_COUNT, 1)
}

pub(crate) fn delete_bucket_tx(
    write: &redb::WriteTransaction,
    name: &BucketName,
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
    Ok(bucket)
}

pub(crate) fn put_object_tx(
    write: &redb::WriteTransaction,
    metadata: &ObjectMetadata,
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
        remove_version(write, record)?;
    }
    let record = ObjectVersionRecord::Object {
        metadata: metadata.clone(),
        is_null,
    };
    insert_version(write, &record)?;
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
    Ok(result)
}

pub(crate) fn delete_object_version_tx(
    write: &redb::WriteTransaction,
    bucket: BucketId,
    object_key_value: &ObjectKey,
    version: VersionId,
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
    let key = object_key(bucket, object_key_value);
    let was_current = current_version(write, &key)? == Some(version);
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
    Ok(Some(DeleteVersionResult {
        removed: record,
        cleanup,
    }))
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
            cleaned.extend(remove_multipart_tx(write, upload.id, true)?.parts);
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
