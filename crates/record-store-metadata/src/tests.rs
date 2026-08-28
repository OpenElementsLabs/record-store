use std::collections::BTreeMap;

use chrono::Utc;
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, Checksum, CorsConfiguration, CorsMethod,
    CorsPattern, CorsRule, ETag, MultipartUpload, MultipartUploadState, ObjectId, ObjectKey,
    ObjectMetadata, ObjectVersionRecord, OrganizationId, PartNumber, UploadId, UploadedPart,
    VersionId, VersioningState,
};
use tempfile::tempdir;

use crate::*;

fn bucket(name: &str) -> Bucket {
    Bucket {
        id: BucketId::new(),
        organization_id: OrganizationId::new(),
        name: BucketName::new(name).expect("bucket"),
        created_at: Utc::now(),
        versioning: VersioningState::Disabled,
        quota: BucketQuota::default(),
        durability_policy: None,
        cors: None,
    }
}
fn object(bucket: BucketId, key: &str, size: u64) -> ObjectMetadata {
    let now = Utc::now();
    ObjectMetadata {
        id: ObjectId::new(),
        bucket_id: bucket,
        key: ObjectKey::new(key).expect("key"),
        version_id: VersionId::new(),
        size,
        checksum: Checksum::sha256([1; 32]),
        payload_format: record_store_core::PayloadFormat::Plaintext,
        durability: record_store_core::DurabilityProfile::Single,
        etag: ETag::from_md5([2; 16]),
        content_type: None,
        custom_metadata: BTreeMap::new(),
        created_at: now,
        modified_at: now,
    }
}

fn cors_configuration() -> CorsConfiguration {
    CorsConfiguration {
        rules: vec![CorsRule {
            id: Some("browser-upload".into()),
            allowed_origins: vec![CorsPattern::origin("https://app.example.com").expect("origin")],
            allowed_methods: vec![CorsMethod::Put, CorsMethod::Get],
            allowed_headers: vec![CorsPattern::header("x-amz-*").expect("header")],
            expose_headers: vec!["ETag".into()],
            max_age_seconds: Some(600),
        }],
    }
}

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
        },
        MetadataCommand::PutObject {
            metadata: Box::new(second_object.clone()),
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

#[tokio::test]
async fn snapshot_export_and_import_restore_the_catalog() {
    let dir = tempdir().expect("temp");
    let repo = RedbMetadataRepository::open(dir.path().join("catalog.redb"))
        .await
        .expect("repo");
    let bucket_record = bucket("snapshot-bucket");
    repo.create_bucket(&bucket_record).await.expect("bucket");
    let stored = object(bucket_record.id, "nested/key", 42);
    repo.put_object(&stored).await.expect("put");
    let database = repo.database();
    let entries = tokio::task::spawn_blocking(move || {
        let read = database.begin_read().expect("begin");
        export_tx(&read).expect("export")
    })
    .await
    .expect("join");

    let other_dir = tempdir().expect("temp");
    let other = RedbMetadataRepository::open(other_dir.path().join("catalog.redb"))
        .await
        .expect("repo");
    let other_database = other.database();
    tokio::task::spawn_blocking(move || {
        let write = other_database.begin_write().expect("begin");
        import_tx(&write, &entries).expect("import");
        write.commit().expect("commit");
    })
    .await
    .expect("join");

    let restored = other
        .get_object(bucket_record.id, &stored.key)
        .await
        .expect("read")
        .expect("object must exist after import");
    assert_eq!(restored, stored);
    assert_eq!(
        other
            .get_bucket_by_name(&bucket_record.name)
            .await
            .expect("read")
            .map(|bucket| bucket.id),
        Some(bucket_record.id)
    );
    let usage = other.storage_usage().await.expect("usage");
    assert_eq!(usage.object_count, 1);
    assert_eq!(usage.bytes_used, 42);
}

#[tokio::test]
async fn versions_markers_and_restart_are_durable() {
    let dir = tempdir().expect("temp");
    let path = dir.path().join("catalog.redb");
    let repo = RedbMetadataRepository::open(&path).await.expect("repo");
    let bucket = bucket("version-bucket");
    repo.create_bucket(&bucket).await.expect("bucket");
    repo.set_bucket_versioning(bucket.id, VersioningState::Enabled)
        .await
        .expect("enable");
    let first = object(bucket.id, "report", 10);
    let second = object(bucket.id, "report", 20);
    repo.put_object(&first).await.expect("first");
    repo.put_object(&second).await.expect("second");
    repo.delete_object(bucket.id, &first.key, NewDeleteMarker::generate())
        .await
        .expect("delete");
    drop(repo);
    let repo = RedbMetadataRepository::open(&path).await.expect("reopen");
    assert!(
        repo.get_object(bucket.id, &first.key)
            .await
            .expect("current")
            .is_none()
    );
    assert!(matches!(
        repo.get_object_version(bucket.id, &first.key, first.version_id)
            .await
            .expect("version"),
        Some(ObjectVersionRecord::Object { .. })
    ));
    assert_eq!(repo.storage_usage().await.expect("usage").version_count, 3);
}

#[tokio::test]
async fn bucket_cors_configuration_survives_restart_and_can_be_removed() {
    let dir = tempdir().expect("temp");
    let path = dir.path().join("catalog.redb");
    let bucket = bucket("cors-bucket");
    let configuration = cors_configuration();
    {
        let repo = RedbMetadataRepository::open(&path).await.expect("repo");
        repo.create_bucket(&bucket).await.expect("bucket");
        let updated = repo
            .set_bucket_cors(bucket.id, Some(configuration.clone()))
            .await
            .expect("set CORS");
        assert_eq!(updated.cors.as_ref(), Some(&configuration));
    }

    let repo = RedbMetadataRepository::open(&path).await.expect("reopen");
    let restored = repo
        .get_bucket_by_name(&bucket.name)
        .await
        .expect("read bucket")
        .expect("bucket exists");
    assert_eq!(restored.cors.as_ref(), Some(&configuration));
    let updated = repo
        .set_bucket_cors(bucket.id, None)
        .await
        .expect("remove CORS");
    assert!(updated.cors.is_none());
}

#[tokio::test]
async fn multipart_state_survives_restart() {
    let dir = tempdir().expect("temp");
    let path = dir.path().join("catalog.redb");
    let bucket = bucket("multipart-bucket");
    let upload = MultipartUpload {
        id: UploadId::new(),
        bucket_id: bucket.id,
        key: ObjectKey::new("large").expect("key"),
        content_type: None,
        custom_metadata: BTreeMap::new(),
        initiated_at: Utc::now(),
        state: MultipartUploadState::Active,
    };
    let part = UploadedPart {
        upload_id: upload.id,
        number: PartNumber::new(1).expect("part"),
        object_id: ObjectId::new(),
        size: 12,
        checksum: Checksum::sha256([3; 32]),
        payload_format: record_store_core::PayloadFormat::Plaintext,
        etag: ETag::from_md5([4; 16]),
        modified_at: Utc::now(),
    };
    {
        let repo = RedbMetadataRepository::open(&path).await.expect("repo");
        repo.create_bucket(&bucket).await.expect("bucket");
        repo.create_multipart_upload(&upload).await.expect("upload");
        repo.put_multipart_part(&part).await.expect("part");
    }
    let repo = RedbMetadataRepository::open(&path).await.expect("reopen");
    assert_eq!(
        repo.list_multipart_parts(upload.id, None, 10)
            .await
            .expect("parts"),
        vec![part]
    );
    assert_eq!(
        repo.storage_usage()
            .await
            .expect("usage")
            .temporary_multipart_bytes,
        12
    );
}
