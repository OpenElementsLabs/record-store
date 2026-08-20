use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use chrono::Utc;
use futures_util::{StreamExt, TryStreamExt, stream};
use oes_core::{Bucket, BucketId, ByteRange, ObjectKey, OrganizationId};
use oes_metadata::{MetadataRepository, RedbMetadataRepository};
use oes_storage::{
    DeleteObjectRequest, DownloadStream, GetObjectRequest, HeadObjectRequest, LocalFilesystemStore,
    ObjectStore, PutObjectRequest, StorageError, upload_stream,
};
use tempfile::tempdir;
use tokio::sync::Notify;

async fn store() -> (
    tempfile::TempDir,
    LocalFilesystemStore,
    Arc<RedbMetadataRepository>,
    Bucket,
) {
    let directory = tempdir().expect("temporary directory");
    let repository = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata/catalog.redb"))
            .await
            .expect("metadata repository"),
    );
    let bucket = Bucket {
        id: BucketId::new(),
        organization_id: OrganizationId::new(),
        name: "integration".into(),
        created_at: Utc::now(),
    };
    repository
        .create_bucket(&bucket)
        .await
        .expect("create bucket");
    let storage = LocalFilesystemStore::open(
        directory.path(),
        directory.path().join("tmp"),
        repository.clone(),
    )
    .await
    .expect("local store");
    (directory, storage, repository, bucket)
}

fn put_request(bucket_id: BucketId, key: &str, chunks: &[&'static [u8]]) -> PutObjectRequest {
    let chunks = chunks
        .iter()
        .map(|chunk| Ok(Bytes::from_static(chunk)))
        .collect::<Vec<_>>();
    PutObjectRequest {
        bucket_id,
        key: ObjectKey::new(key).expect("valid key"),
        content_type: Some("application/octet-stream".into()),
        custom_metadata: BTreeMap::from([("source".into(), "integration-test".into())]),
        expected_checksum: None,
        body: upload_stream(stream::iter(chunks)),
    }
}

async fn collect(stream: DownloadStream) -> Result<Vec<u8>, StorageError> {
    stream
        .try_fold(Vec::new(), |mut collected, chunk| async move {
            collected.extend_from_slice(&chunk);
            Ok(collected)
        })
        .await
}

#[tokio::test]
async fn streams_put_get_range_head_and_delete() {
    let (_directory, storage, _repository, bucket) = store().await;
    let result = storage
        .put(put_request(
            bucket.id,
            "large/object.bin",
            &[b"streamed ", b"without ", b"buffering"],
        ))
        .await
        .expect("put object");
    assert_eq!(result.metadata.size, 26);

    let head = storage
        .head(HeadObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("large/object.bin").expect("key"),
        })
        .await
        .expect("head object");
    assert_eq!(head, result.metadata);

    let read = storage
        .get(GetObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("large/object.bin").expect("key"),
            range: None,
        })
        .await
        .expect("get object");
    let bytes = collect(read.body).await.expect("stream body");
    assert_eq!(bytes.as_slice(), b"streamed without buffering");

    let range = storage
        .get(GetObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("large/object.bin").expect("key"),
            range: Some(ByteRange::new(9, 7).expect("range")),
        })
        .await
        .expect("get range");
    assert_eq!(
        collect(range.body).await.expect("range body").as_slice(),
        b"without"
    );

    storage
        .delete(DeleteObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("large/object.bin").expect("key"),
        })
        .await
        .expect("delete object");
    assert!(matches!(
        storage
            .head(HeadObjectRequest {
                bucket_id: bucket.id,
                key: ObjectKey::new("large/object.bin").expect("key"),
            })
            .await,
        Err(StorageError::ObjectNotFound)
    ));
}

#[tokio::test]
async fn failed_checksum_never_publishes_object_or_leaves_temporary_file() {
    let (directory, storage, _repository, bucket) = store().await;
    let mut request = put_request(bucket.id, "checksums/test", &[b"actual bytes"]);
    request.expected_checksum = Some(oes_core::Checksum::sha256([0; 32]));
    assert!(matches!(
        storage.put(request).await,
        Err(StorageError::ChecksumMismatch { .. })
    ));
    assert_eq!(
        std::fs::read_dir(directory.path().join("tmp"))
            .expect("read temporary directory")
            .count(),
        0
    );
}

#[tokio::test]
async fn cancelled_upload_cleans_its_temporary_file() {
    let (directory, storage, _repository, bucket) = store().await;
    let first_chunk_polled = Arc::new(Notify::new());
    let notification = Arc::clone(&first_chunk_polled);
    let body = stream::once(async move {
        notification.notify_one();
        Ok(Bytes::from_static(b"partial"))
    })
    .chain(stream::pending());
    let request = PutObjectRequest {
        bucket_id: bucket.id,
        key: ObjectKey::new("cancelled/upload").expect("key"),
        content_type: None,
        custom_metadata: BTreeMap::new(),
        expected_checksum: None,
        body: upload_stream(body),
    };

    let upload = tokio::spawn(async move { storage.put(request).await });
    first_chunk_polled.notified().await;
    upload.abort();
    assert!(upload.await.expect_err("cancelled task").is_cancelled());
    assert_eq!(
        std::fs::read_dir(directory.path().join("tmp"))
            .expect("read temporary directory")
            .count(),
        0
    );
}

#[tokio::test]
async fn missing_bucket_and_invalid_keys_are_rejected() {
    let (_directory, storage, _repository, _bucket) = store().await;
    assert!(ObjectKey::new("../../etc/passwd").is_err());
    assert!(matches!(
        storage
            .put(put_request(BucketId::new(), "safe-key", &[b"payload"]))
            .await,
        Err(StorageError::BucketNotFound)
    ));
}

#[tokio::test]
async fn readiness_performs_a_real_write_probe() {
    let (_directory, storage, _repository, _bucket) = store().await;
    storage.check_ready().await.expect("storage ready");
}
