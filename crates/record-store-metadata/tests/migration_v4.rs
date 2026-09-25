//! Migrating a real schema-4 catalog directory to schema 5.
//!
//! The fixture is written with raw redb tables and raw v4-shaped JSON rather
//! than through `RedbMetadataRepository`, deliberately. A fixture built by the
//! same code under test would drift along with it and stop being evidence that
//! a *deployed* v4 database still opens. This is also why it is built here
//! rather than committed as a binary `.redb`: a committed file is opaque in
//! review and breaks whenever the redb file format moves, which this repository
//! has already lived through once.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use record_store_metadata::{METADATA_SCHEMA_VERSION, MetadataRepository, RedbMetadataRepository};
use redb::{Database, ReadableDatabase, TableDefinition};
use serde_json::json;
use uuid::Uuid;

const BUCKETS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("buckets.v1");
const BUCKET_NAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("bucket_names.v1");
const OBJECTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("objects.v1");
const VERSIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("versions.v1");
const VERSION_ORDER: TableDefinition<&[u8], &[u8]> = TableDefinition::new("version_order.v1");
const NULL_VERSIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("null_versions.v1");
const MULTIPART: TableDefinition<&[u8], &[u8]> = TableDefinition::new("multipart.v1");
const MULTIPART_ORDER: TableDefinition<&[u8], &[u8]> = TableDefinition::new("multipart_order.v1");
const PARTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("multipart_parts.v1");
const CLEANUP: TableDefinition<&[u8], u8> = TableDefinition::new("payload_cleanup.v1");
const BUCKET_USAGE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("bucket_usage.v1");
const LIFECYCLE_RULES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("lifecycle_rules.v1");
const MARKERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("delete_markers.v1");
const COUNTERS: TableDefinition<&str, u64> = TableDefinition::new("counters.v1");
const SCHEMA: TableDefinition<&str, u64> = TableDefinition::new("schema.v1");

struct Fixture {
    bucket_id: Uuid,
    version_id: Uuid,
    created_at: DateTime<Utc>,
}

/// Writes a data directory exactly as a schema-4 deployment left it.
///
/// One versioned bucket with one current object, no Object Lock field anywhere,
/// and no `object_locks` or `clock` table — because neither existed yet.
fn write_v4_fixture(directory: &std::path::Path) -> Fixture {
    std::fs::create_dir_all(directory.join("metadata")).expect("metadata directory");
    std::fs::create_dir_all(directory.join("objects")).expect("objects directory");
    let database =
        Database::create(directory.join("metadata").join("catalog.redb")).expect("create v4");

    let bucket_id = Uuid::from_u128(0x4242);
    let object_id = Uuid::from_u128(0x1001);
    let version_id = Uuid::from_u128(0x2002);
    let created_at = DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .expect("fixture timestamp")
        .with_timezone(&Utc);

    // A v4 bucket record: no `object_lock` key at all.
    let bucket = json!({
        "id": bucket_id,
        "organization_id": Uuid::from_u128(1),
        "name": "archive",
        "created_at": created_at,
        "versioning": "enabled",
        "quota": {"bytes": {"mode": "unlimited"}, "objects": {"mode": "unlimited"}},
        "storage_class": null,
        "durability_policy": null,
        "cors": null,
    });
    let object = json!({
        "id": object_id,
        "bucket_id": bucket_id,
        "key": "reports/2026-q1.pdf",
        "version_id": version_id,
        "size": 4_096,
        "checksum": "sha256:0101010101010101010101010101010101010101010101010101010101010101",
        "payload_format": "plaintext",
        "durability": {"strategy": "single"},
        "etag": "02020202020202020202020202020202",
        "content_type": "application/pdf",
        "custom_metadata": {"department": "finance"},
        "created_at": created_at,
        "modified_at": created_at,
    });
    let version = json!({"kind": "object", "metadata": object, "is_null": false});

    let bucket_key = bucket_id.as_bytes().to_vec();
    let mut object_key = bucket_key.clone();
    object_key.extend_from_slice(b"reports/2026-q1.pdf");
    let mut order_key = object_key.clone();
    order_key.push(0);
    order_key
        .extend_from_slice(&(u64::MAX - created_at.timestamp_micros().max(0) as u64).to_be_bytes());
    order_key.extend_from_slice(version_id.as_bytes());

    let write = database.begin_write().expect("begin");
    {
        // Every table a v4 database carried, and only those.
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
            write.open_table(table).expect("open v4 table");
        }
        write.open_table(CLEANUP).expect("cleanup");

        let encoded_bucket = serde_json::to_vec(&bucket).expect("encode bucket");
        write
            .open_table(BUCKETS)
            .expect("buckets")
            .insert(bucket_key.as_slice(), encoded_bucket.as_slice())
            .expect("insert bucket");
        write
            .open_table(BUCKET_NAMES)
            .expect("bucket names")
            .insert("archive", encoded_bucket.as_slice())
            .expect("index bucket");
        let encoded_object = serde_json::to_vec(&object).expect("encode object");
        write
            .open_table(OBJECTS)
            .expect("objects")
            .insert(object_key.as_slice(), encoded_object.as_slice())
            .expect("insert object");
        let encoded_version = serde_json::to_vec(&version).expect("encode version");
        write
            .open_table(VERSIONS)
            .expect("versions")
            .insert(version_id.as_bytes().as_slice(), encoded_version.as_slice())
            .expect("insert version");
        write
            .open_table(VERSION_ORDER)
            .expect("version order")
            .insert(order_key.as_slice(), version_id.as_bytes().as_slice())
            .expect("index version");
        let usage = json!({
            "current_objects": 1,
            "logical_bytes": 4_096,
            "versions": 1,
            "version_bytes": 4_096,
            "multipart_bytes": 0,
        });
        let encoded_usage = serde_json::to_vec(&usage).expect("encode usage");
        write
            .open_table(BUCKET_USAGE)
            .expect("bucket usage")
            .insert(bucket_key.as_slice(), encoded_usage.as_slice())
            .expect("insert usage");
        let mut counters = write.open_table(COUNTERS).expect("counters");
        for (name, value) in [
            ("objects", 1_u64),
            ("buckets", 1),
            ("logical_bytes", 4_096),
            ("versions", 1),
            ("version_bytes", 4_096),
            ("physical_bytes", 4_096),
            ("multipart_bytes", 0),
        ] {
            counters.insert(name, &value).expect("insert counter");
        }
        write
            .open_table(SCHEMA)
            .expect("schema")
            .insert("metadata", &4_u64)
            .expect("write schema 4");
    }
    write.commit().expect("commit v4 fixture");

    Fixture {
        bucket_id,
        version_id,
        created_at,
    }
}

#[tokio::test]
async fn a_schema_four_directory_starts_migrates_and_serves_every_object_unchanged() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture = write_v4_fixture(directory.path());
    let path = directory.path().join("metadata").join("catalog.redb");

    // Read without migrating, as a backup labels the catalog it copied, and
    // without changing the file.
    let before = std::fs::read(&path).expect("fixture bytes");
    assert_eq!(
        record_store_metadata::stored_schema_version(&path).expect("read-only schema read"),
        Some(4)
    );
    assert_eq!(
        std::fs::read(&path).expect("fixture bytes"),
        before,
        "reading changed the file"
    );

    // Confirm the fixture really is v4 and really lacks the new tables, so a
    // later refactor cannot turn this into a test of nothing.
    {
        let database = Database::open(&path).expect("open fixture");
        let read = database.begin_read().expect("read");
        assert_eq!(
            read.open_table(SCHEMA)
                .expect("schema")
                .get("metadata")
                .expect("read schema")
                .expect("schema present")
                .value(),
            4
        );
        assert!(
            read.open_table(TableDefinition::<&[u8], &[u8]>::new("object_locks.v1"))
                .is_err(),
            "a v4 fixture has no object lock table"
        );
    }

    let repository = RedbMetadataRepository::open(&path)
        .await
        .expect("a v4 deployment must start");

    // The bucket is unchanged, and simply has no Object Lock.
    let bucket_id = record_store_core::BucketId::from_uuid(fixture.bucket_id);
    let bucket = repository
        .get_bucket(bucket_id)
        .await
        .expect("read bucket")
        .expect("the migrated bucket is still there");
    assert_eq!(bucket.name.as_str(), "archive");
    assert_eq!(
        bucket.versioning,
        record_store_core::VersioningState::Enabled
    );
    assert!(
        bucket.object_lock.is_none(),
        "migration must not invent an Object Lock configuration"
    );

    // The object is byte-for-byte what v4 stored.
    let key = record_store_core::ObjectKey::new("reports/2026-q1.pdf").expect("key");
    let object = repository
        .get_object(bucket_id, &key)
        .await
        .expect("read object")
        .expect("the migrated object is still served");
    assert_eq!(object.size, 4_096);
    assert_eq!(object.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(object.created_at, fixture.created_at);
    assert_eq!(
        object.custom_metadata,
        BTreeMap::from([("department".to_owned(), "finance".to_owned())])
    );
    assert_eq!(object.version_id.as_uuid(), fixture.version_id);

    // Its history is intact, and it is reported as unlocked rather than as an
    // error: a version that predates Object Lock simply has no lock.
    let version = repository
        .get_object_version(bucket_id, &key, object.version_id)
        .await
        .expect("read version")
        .expect("the version survives migration");
    assert_eq!(version.version_id(), object.version_id);
    assert!(
        repository
            .get_object_lock(object.version_id)
            .await
            .expect("read lock")
            .is_unlocked()
    );

    // Accounting survived, so a migrated deployment does not misreport usage.
    let usage = repository.storage_usage().await.expect("usage");
    assert_eq!(usage.object_count, 1);
    assert_eq!(usage.bytes_used, 4_096);

    drop(repository);

    // The schema advanced on disk, and the new tables now exist.
    {
        let database = Database::open(&path).expect("reopen");
        let read = database.begin_read().expect("read");
        assert_eq!(
            read.open_table(SCHEMA)
                .expect("schema")
                .get("metadata")
                .expect("read schema")
                .expect("schema present")
                .value(),
            METADATA_SCHEMA_VERSION
        );
        assert_eq!(METADATA_SCHEMA_VERSION, 6);
        assert!(
            read.open_table(TableDefinition::<&[u8], &[u8]>::new("object_locks.v1"))
                .is_ok(),
            "migration creates the object lock table"
        );
        assert!(
            read.open_table(TableDefinition::<&str, i64>::new("clock.v1"))
                .is_ok(),
            "migration creates the clock table"
        );
        assert!(
            read.open_table(TableDefinition::<u64, &[u8]>::new("mutation_events.v1"))
                .is_ok(),
            "migration creates the storage-event journal"
        );
    }

    // Migration is idempotent: opening an already-migrated directory is a no-op.
    let reopened = RedbMetadataRepository::open(&path)
        .await
        .expect("reopening a migrated directory");
    assert!(
        reopened
            .get_object(bucket_id, &key)
            .await
            .expect("read object")
            .is_some()
    );
}
