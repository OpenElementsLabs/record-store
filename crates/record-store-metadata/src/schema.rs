//! Durable single-node metadata catalog.

use chrono::Utc;
use record_store_core::{
    Bucket, BucketId, ObjectId, ObjectKey, ObjectMetadata, ObjectVersionRecord, VersionId,
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::Deserialize;

use crate::error::{backend, counter_error};
use crate::keys::{bucket_key, object_key};
use crate::tx::{insert_version, read_bucket_usage, read_counter, set_null, write_bucket_usage};
use crate::types::BucketUsage;
use crate::*;

pub(crate) const BUCKETS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("buckets.v1");
pub(crate) const BUCKET_NAMES: TableDefinition<&str, &[u8]> =
    TableDefinition::new("bucket_names.v1");
pub(crate) const OBJECTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("objects.v1");
pub(crate) const MARKERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("delete_markers.v1");
pub(crate) const VERSIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("versions.v1");
pub(crate) const VERSION_ORDER: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("version_order.v1");
pub(crate) const NULL_VERSIONS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("null_versions.v1");
pub(crate) const MULTIPART: TableDefinition<&[u8], &[u8]> = TableDefinition::new("multipart.v1");
pub(crate) const MULTIPART_ORDER: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("multipart_order.v1");
pub(crate) const PARTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("multipart_parts.v1");
pub(crate) const CLEANUP: TableDefinition<&[u8], u8> = TableDefinition::new("payload_cleanup.v1");
pub(crate) const BUCKET_USAGE: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("bucket_usage.v1");
pub(crate) const LIFECYCLE_RULES: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("lifecycle_rules.v1");
pub(crate) const COUNTERS: TableDefinition<&str, u64> = TableDefinition::new("counters.v1");
pub(crate) const SCHEMA: TableDefinition<&str, u64> = TableDefinition::new("schema.v1");

/// Current durable catalog format used by offline backup compatibility checks.
pub const METADATA_SCHEMA_VERSION: u64 = 4;
pub(crate) const CURRENT_SCHEMA_VERSION: u64 = METADATA_SCHEMA_VERSION;
pub(crate) const OBJECT_COUNT: &str = "objects";
pub(crate) const BUCKET_COUNT: &str = "buckets";
pub(crate) const LOGICAL_BYTES: &str = "logical_bytes";
pub(crate) const LEGACY_BYTES: &str = "bytes";
pub(crate) const VERSION_COUNT: &str = "versions";
pub(crate) const VERSION_BYTES: &str = "version_bytes";
pub(crate) const PHYSICAL_BYTES: &str = "physical_bytes";
pub(crate) const MULTIPART_BYTES: &str = "multipart_bytes";

pub(crate) fn initialize_schema(database: &Database) -> Result<(), MetadataError> {
    let write = database
        .begin_write()
        .map_err(|e| backend("initialize", e))?;
    for table in [
        BUCKETS,
        OBJECTS,
        MARKERS,
        VERSIONS,
        VERSION_ORDER,
        NULL_VERSIONS,
        MULTIPART,
        MULTIPART_ORDER,
        PARTS,
        BUCKET_USAGE,
        LIFECYCLE_RULES,
    ] {
        write
            .open_table(table)
            .map_err(|e| backend("initialize byte table", e))?;
    }
    write
        .open_table(BUCKET_NAMES)
        .map_err(|e| backend("initialize bucket names", e))?;
    write
        .open_table(CLEANUP)
        .map_err(|e| backend("initialize cleanup", e))?;
    write
        .open_table(COUNTERS)
        .map_err(|e| backend("initialize counters", e))?;
    write
        .open_table(SCHEMA)
        .map_err(|e| backend("initialize schema", e))?;
    let version = {
        let table = write
            .open_table(SCHEMA)
            .map_err(|e| backend("open schema", e))?;
        table
            .get("metadata")
            .map_err(|e| backend("read schema", e))?
            .map_or(0, |v| v.value())
    };
    if version > CURRENT_SCHEMA_VERSION {
        return Err(MetadataError::Database {
            operation: "schema compatibility",
            reason: format!(
                "schema {version} is newer than supported schema {CURRENT_SCHEMA_VERSION}"
            ),
        });
    }
    if version < 3 {
        migrate_v3(&write)?;
    }
    if version < 4 {
        migrate_v4(&write)?;
    }
    if version < CURRENT_SCHEMA_VERSION {
        let mut table = write
            .open_table(SCHEMA)
            .map_err(|e| backend("open schema", e))?;
        table
            .insert("metadata", &CURRENT_SCHEMA_VERSION)
            .map_err(|e| backend("write schema", e))?;
    }
    write
        .commit()
        .map_err(|e| backend("commit initialization", e))
}

pub(crate) fn migrate_v4(write: &redb::WriteTransaction) -> Result<(), MetadataError> {
    write
        .open_table(LIFECYCLE_RULES)
        .map_err(|e| backend("migrate lifecycle rules", e))?;
    Ok(())
}

pub(crate) fn migrate_v3(write: &redb::WriteTransaction) -> Result<(), MetadataError> {
    let buckets = {
        let table = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open migration buckets", e))?;
        let mut out = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| backend("iterate migration buckets", e))?
        {
            let (_, value) = entry.map_err(|e| backend("read migration bucket", e))?;
            out.push(serde_json::from_slice::<Bucket>(value.value())?);
        }
        out
    };
    let objects = {
        let table = write
            .open_table(OBJECTS)
            .map_err(|e| backend("open migration objects", e))?;
        let mut out = Vec::new();
        for entry in table
            .iter()
            .map_err(|e| backend("iterate migration objects", e))?
        {
            let (_, value) = entry.map_err(|e| backend("read migration object", e))?;
            out.push(decode_migrating_object(value.value())?);
        }
        out
    };
    {
        let mut ids = write
            .open_table(BUCKETS)
            .map_err(|e| backend("open migration buckets", e))?;
        let mut names = write
            .open_table(BUCKET_NAMES)
            .map_err(|e| backend("open migration names", e))?;
        for bucket in &buckets {
            let bytes = serde_json::to_vec(bucket)?;
            ids.insert(bucket_key(bucket.id).as_slice(), bytes.as_slice())
                .map_err(|e| backend("migrate bucket", e))?;
            names
                .insert(bucket.name.as_str(), bytes.as_slice())
                .map_err(|e| backend("migrate bucket index", e))?;
        }
    }
    let mut total = 0_u64;
    for metadata in &objects {
        let record = ObjectVersionRecord::Object {
            metadata: metadata.clone(),
            is_null: true,
        };
        insert_version(write, &record)?;
        set_null(
            write,
            &object_key(metadata.bucket_id, &metadata.key),
            metadata.version_id,
        )?;
        let mut usage = read_bucket_usage(write, metadata.bucket_id)?;
        usage.current_objects += 1;
        usage.logical_bytes = usage
            .logical_bytes
            .checked_add(metadata.size)
            .ok_or_else(counter_error)?;
        usage.versions += 1;
        usage.version_bytes = usage
            .version_bytes
            .checked_add(metadata.size)
            .ok_or_else(counter_error)?;
        write_bucket_usage(write, metadata.bucket_id, usage)?;
        total = total.checked_add(metadata.size).ok_or_else(counter_error)?;
    }
    for bucket in &buckets {
        if read_bucket_usage(write, bucket.id)? == BucketUsage::default() {
            write_bucket_usage(write, bucket.id, BucketUsage::default())?;
        }
    }
    let legacy = {
        let table = write
            .open_table(COUNTERS)
            .map_err(|e| backend("open migration counters", e))?;
        read_counter(&table, LEGACY_BYTES)?.max(total)
    };
    let mut table = write
        .open_table(COUNTERS)
        .map_err(|e| backend("open migration counters", e))?;
    for (name, value) in [
        (OBJECT_COUNT, objects.len() as u64),
        (BUCKET_COUNT, buckets.len() as u64),
        (LOGICAL_BYTES, legacy),
        (VERSION_COUNT, objects.len() as u64),
        (VERSION_BYTES, legacy),
        (PHYSICAL_BYTES, legacy),
        (MULTIPART_BYTES, 0),
    ] {
        table
            .insert(name, &value)
            .map_err(|e| backend("write migration counter", e))?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct LegacyObjectMetadata {
    id: ObjectId,
    bucket_id: BucketId,
    key: ObjectKey,
    version_id: VersionId,
    size: u64,
    checksum: record_store_core::Checksum,
    content_type: Option<String>,
    custom_metadata: std::collections::BTreeMap<String, String>,
    created_at: chrono::DateTime<Utc>,
    modified_at: chrono::DateTime<Utc>,
}

pub(crate) fn decode_migrating_object(bytes: &[u8]) -> Result<ObjectMetadata, MetadataError> {
    if let Ok(metadata) = serde_json::from_slice::<ObjectMetadata>(bytes) {
        return Ok(metadata);
    }
    let old: LegacyObjectMetadata = serde_json::from_slice(bytes)?;
    let etag = old
        .checksum
        .to_string()
        .split_once(':')
        .and_then(|(_, digest)| record_store_core::ETag::new(digest).ok())
        .ok_or_else(|| MetadataError::Database {
            operation: "migrate ETag",
            reason: "invalid legacy checksum".into(),
        })?;
    Ok(ObjectMetadata {
        id: old.id,
        bucket_id: old.bucket_id,
        key: old.key,
        version_id: old.version_id,
        size: old.size,
        checksum: old.checksum,
        payload_format: record_store_core::PayloadFormat::Plaintext,
        durability: record_store_core::DurabilityProfile::Single,
        etag,
        content_type: old.content_type,
        custom_metadata: old.custom_metadata,
        created_at: old.created_at,
        modified_at: old.modified_at,
    })
}
