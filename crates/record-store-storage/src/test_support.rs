//! Shared fixtures for object-store tests.

use std::sync::Arc;

use chrono::Utc;
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, ObjectKey, OrganizationId, VersioningState,
};
use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
use tempfile::TempDir;

use crate::{LocalFilesystemStore, ObjectStore, PutObjectRequest, upload_stream};

/// Opens a plaintext store over a throwaway directory and registers one bucket.
pub(crate) async fn store() -> (TempDir, LocalFilesystemStore, Bucket) {
    open(None).await
}

/// Opens a store, optionally encrypted with the supplied master key.
pub(crate) async fn open(master_key: Option<&[u8]>) -> (TempDir, LocalFilesystemStore, Bucket) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let metadata: Arc<dyn MetadataRepository> = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let bucket = bucket("payloads");
    metadata.create_bucket(&bucket).await.expect("bucket");

    let store = match master_key {
        Some(key) => LocalFilesystemStore::open_encrypted(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
            key,
        )
        .await
        .expect("encrypted store"),
        None => LocalFilesystemStore::open(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("store"),
    };
    (directory, store, bucket)
}

/// Builds a bucket record.
pub(crate) fn bucket(name: &str) -> Bucket {
    Bucket {
        id: BucketId::new(),
        organization_id: OrganizationId::new(),
        name: BucketName::new(name).expect("bucket name"),
        created_at: Utc::now(),
        versioning: VersioningState::Disabled,
        quota: BucketQuota::default(),
        storage_class: None,
        durability_policy: None,
        cors: None,
    }
}

/// Stores an object and returns the commit result.
pub(crate) async fn put(
    store: &LocalFilesystemStore,
    bucket: &Bucket,
    key: &str,
    body: &[u8],
) -> crate::PutObjectResult {
    let owned = bytes::Bytes::copy_from_slice(body);
    store
        .put(PutObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new(key).expect("key"),
            content_type: None,
            custom_metadata: Default::default(),
            expected_checksum: None,
            object_id: None,
            protocol_etag: None,
            body: upload_stream(futures_util::stream::once(async move { Ok(owned) })),
        })
        .await
        .expect("put object")
}

/// Returns the catalog a store was opened over, for tests that need to seed it.
pub(crate) fn metadata_of(store: &LocalFilesystemStore) -> Arc<dyn MetadataRepository> {
    Arc::clone(&store.metadata)
}
