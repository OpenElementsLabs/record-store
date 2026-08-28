use chrono::Utc;
use record_store_core::{Bucket, BucketName, BucketQuota, OrganizationId, VersioningState};
use record_store_metadata::RedbMetadataRepository;
use tempfile::tempdir;

use std::path::PathBuf;

use record_store_core::{BucketId, ObjectId, ObjectKey};
use record_store_metadata::MetadataRepository;
use tokio::fs;
use uuid::Uuid;

use super::*;
use crate::layout::{PublicationRecord, StorageLayout};
use crate::maintenance::write_publication_record;

#[test]
fn payload_paths_only_contain_generated_identifier_components() {
    let root = PathBuf::from("/trusted/data");
    let layout = StorageLayout::new(&root, &root.join("tmp"));
    let id =
        ObjectId::from_uuid(Uuid::parse_str("12345678-1234-4234-8234-123456789abc").expect("UUID"));
    assert_eq!(
        layout.payload_path(id),
        root.join("objects/12/34/12345678123442348234123456789abc")
    );
}

#[tokio::test]
async fn startup_publication_journal_removes_an_uncommitted_payload() {
    let directory = tempdir().expect("temporary directory");
    let metadata = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let bucket = Bucket {
        id: BucketId::new(),
        organization_id: OrganizationId::new(),
        name: BucketName::new("journal-bucket").expect("bucket"),
        created_at: Utc::now(),
        versioning: VersioningState::Disabled,
        quota: BucketQuota::default(),
        durability_policy: None,
        cors: None,
    };
    metadata
        .create_bucket(&bucket)
        .await
        .expect("create bucket");
    let repository: Arc<dyn MetadataRepository> = metadata;
    let store = LocalFilesystemStore::open(
        directory.path(),
        directory.path().join("tmp"),
        Arc::clone(&repository),
    )
    .await
    .expect("store");
    let object_id = ObjectId::new();
    let payload = store.layout.payload_path(object_id);
    fs::create_dir_all(payload.parent().expect("payload parent"))
        .await
        .expect("payload parent directory");
    fs::write(&payload, b"unpublished")
        .await
        .expect("unpublished payload");
    let publication = store.layout.publication_path(object_id);
    write_publication_record(
        &publication,
        &PublicationRecord {
            object_id,
            bucket_id: Some(bucket.id),
            key: Some(ObjectKey::new("never-visible").expect("key")),
        },
    )
    .await
    .expect("publication record");
    drop(store);

    LocalFilesystemStore::open(directory.path(), directory.path().join("tmp"), repository)
        .await
        .expect("recover store");
    assert!(!payload.exists());
    assert!(!publication.exists());
}

#[tokio::test]
async fn inspection_and_explicit_repair_handle_only_owned_orphan_payloads() {
    let directory = tempdir().expect("temporary directory");
    let metadata = Arc::new(
        RedbMetadataRepository::open(directory.path().join("catalog.redb"))
            .await
            .expect("metadata"),
    );
    let metadata_dependency: Arc<dyn MetadataRepository> = metadata;
    let store = LocalFilesystemStore::open(
        directory.path().join("data"),
        directory.path().join("tmp"),
        metadata_dependency,
    )
    .await
    .expect("store");
    let orphan = ObjectId::new();
    let path = store.layout.payload_path(orphan);
    fs::create_dir_all(path.parent().expect("payload parent"))
        .await
        .expect("payload directory");
    fs::write(&path, b"orphan").await.expect("orphan payload");

    let inspection = store.inspect(100).await.expect("inspection");
    assert_eq!(inspection.data_without_metadata, 1);
    assert_eq!(inspection.orphan_payload_samples, vec![orphan]);

    let dry_run = store
        .repair(StorageRepairRequest {
            maximum_entries: 100,
            dry_run: true,
        })
        .await
        .expect("dry run");
    assert_eq!(dry_run.removed_orphan_payloads, 0);
    assert!(fs::try_exists(&path).await.expect("exists"));

    let repaired = store
        .repair(StorageRepairRequest {
            maximum_entries: 100,
            dry_run: false,
        })
        .await
        .expect("repair");
    assert_eq!(repaired.removed_orphan_payloads, 1);
    assert!(!fs::try_exists(path).await.expect("removed"));
}
