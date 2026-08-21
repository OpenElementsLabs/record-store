use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use chrono::Utc;
use futures_util::{StreamExt, TryStreamExt, stream};
use oes_core::{Bucket, BucketId, BucketName, ByteRange, ObjectId, ObjectKey, OrganizationId};
use oes_metadata::{MetadataRepository, RedbMetadataRepository};
use oes_storage::{
    DeleteObjectRequest, DownloadStream, GetObjectRequest, HeadObjectRequest, LocalFilesystemStore,
    ObjectStore, PutObjectRequest, StorageError, VerifyObjectRequest, upload_stream,
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
        name: BucketName::new("integration-bucket").expect("bucket name"),
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

fn payload_path(root: &std::path::Path, object_id: ObjectId) -> std::path::PathBuf {
    let encoded = object_id.as_uuid().simple().to_string();
    root.join("objects")
        .join(&encoded[..2])
        .join(&encoded[2..4])
        .join(encoded)
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

#[tokio::test]
async fn empty_and_large_generated_objects_remain_streaming() {
    let (_directory, storage, _repository, bucket) = store().await;
    let empty = storage
        .put(put_request(bucket.id, "empty", &[]))
        .await
        .expect("put empty object");
    assert_eq!(empty.metadata.size, 0);
    let empty_read = storage
        .get(GetObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("empty").expect("key"),
            range: None,
        })
        .await
        .expect("get empty object");
    assert!(
        collect(empty_read.body)
            .await
            .expect("empty body")
            .is_empty()
    );

    const CHUNK_SIZE: usize = 64 * 1024;
    const CHUNK_COUNT: usize = 256;
    let body = stream::unfold(0_usize, |index| async move {
        (index < CHUNK_COUNT).then(|| {
            (
                Ok(Bytes::from(vec![(index % 251) as u8; CHUNK_SIZE])),
                index + 1,
            )
        })
    });
    let large = storage
        .put(PutObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("generated/large.bin").expect("key"),
            content_type: None,
            custom_metadata: BTreeMap::new(),
            expected_checksum: None,
            body: upload_stream(body),
        })
        .await
        .expect("put generated large object");
    assert_eq!(large.metadata.size, (CHUNK_SIZE * CHUNK_COUNT) as u64);
    let read = storage
        .get(GetObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("generated/large.bin").expect("key"),
            range: None,
        })
        .await
        .expect("get generated large object");
    let downloaded = read
        .body
        .try_fold(0_u64, |total, chunk| async move {
            Ok(total + chunk.len() as u64)
        })
        .await
        .expect("stream generated object");
    assert_eq!(downloaded, large.metadata.size);
}

#[tokio::test]
async fn overwrite_failure_preserves_old_object_and_success_replaces_it() {
    let (_directory, storage, repository, bucket) = store().await;
    let first = storage
        .put(put_request(bucket.id, "replaceable", &[b"original"]))
        .await
        .expect("put original");
    let mut invalid = put_request(bucket.id, "replaceable", &[b"corrupt replacement"]);
    invalid.expected_checksum = Some(oes_core::Checksum::sha256([0; 32]));
    assert!(matches!(
        storage.put(invalid).await,
        Err(StorageError::ChecksumMismatch { .. })
    ));
    let after_failure = storage
        .get(GetObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("replaceable").expect("key"),
            range: None,
        })
        .await
        .expect("get original after failure");
    assert_eq!(
        collect(after_failure.body).await.expect("original body"),
        b"original"
    );

    let replacement = storage
        .put(put_request(bucket.id, "replaceable", &[b"replacement"]))
        .await
        .expect("put replacement");
    assert_ne!(first.metadata.id, replacement.metadata.id);
    let usage = repository.storage_usage().await.expect("usage");
    assert_eq!(usage.object_count, 1);
    assert_eq!(usage.bytes_used, 11);
}

#[tokio::test]
async fn concurrent_reads_and_same_key_writes_publish_only_complete_objects() {
    let (_directory, storage, _repository, bucket) = store().await;
    let storage = Arc::new(storage);
    storage
        .put(put_request(
            bucket.id,
            "concurrent/read",
            &[b"readable payload"],
        ))
        .await
        .expect("put read fixture");
    let mut readers = Vec::new();
    for _ in 0..8 {
        let storage = Arc::clone(&storage);
        readers.push(tokio::spawn(async move {
            let result = storage
                .get(GetObjectRequest {
                    bucket_id: bucket.id,
                    key: ObjectKey::new("concurrent/read").expect("key"),
                    range: None,
                })
                .await
                .expect("concurrent get");
            collect(result.body).await.expect("concurrent body")
        }));
    }
    for reader in readers {
        assert_eq!(reader.await.expect("reader task"), b"readable payload");
    }

    let mut writers = Vec::new();
    for index in 0_u8..8 {
        let storage = Arc::clone(&storage);
        writers.push(tokio::spawn(async move {
            let payload = Bytes::from(vec![index; 32 * 1024]);
            storage
                .put(PutObjectRequest {
                    bucket_id: bucket.id,
                    key: ObjectKey::new("concurrent/write").expect("key"),
                    content_type: None,
                    custom_metadata: BTreeMap::new(),
                    expected_checksum: None,
                    body: upload_stream(stream::once(async move { Ok(payload) })),
                })
                .await
                .expect("concurrent put")
        }));
    }
    for writer in writers {
        writer.await.expect("writer task");
    }
    let final_object = storage
        .get(GetObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("concurrent/write").expect("key"),
            range: None,
        })
        .await
        .expect("final object");
    let final_body = collect(final_object.body).await.expect("final body");
    assert_eq!(final_body.len(), 32 * 1024);
    assert!(final_body.iter().all(|byte| *byte == final_body[0]));
    storage
        .verify(VerifyObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("concurrent/write").expect("key"),
        })
        .await
        .expect("verify final object");
}

#[tokio::test]
async fn restart_preserves_metadata_detects_corruption_and_cleans_owned_temporary_files() {
    let (directory, storage, repository, bucket) = store().await;
    let committed = storage
        .put(put_request(
            bucket.id,
            "durable/object",
            &[b"durable bytes"],
        ))
        .await
        .expect("put durable object");
    let data_root = directory.path();
    let committed_path = payload_path(data_root, committed.metadata.id);
    let upload_id = uuid::Uuid::new_v4();
    let recognized_upload = data_root
        .join("tmp")
        .join(format!("{}.upload", upload_id.simple()));
    std::fs::write(&recognized_upload, b"partial").expect("write abandoned upload");
    let unrelated = data_root.join("tmp/not-an-oes-upload.upload");
    std::fs::write(&unrelated, b"preserve").expect("write unrelated file");

    drop(storage);
    let reopened = LocalFilesystemStore::open(data_root, data_root.join("tmp"), repository.clone())
        .await
        .expect("reopen storage");
    assert!(!recognized_upload.exists());
    assert!(unrelated.exists());
    assert_eq!(
        reopened
            .status()
            .await
            .expect("storage status")
            .temporary_upload_bytes,
        0
    );
    let durable = reopened
        .get(GetObjectRequest {
            bucket_id: bucket.id,
            key: ObjectKey::new("durable/object").expect("key"),
            range: None,
        })
        .await
        .expect("get after restart");
    assert_eq!(
        collect(durable.body).await.expect("durable body"),
        b"durable bytes"
    );

    std::fs::write(&committed_path, b"deliberate corruption").expect("corrupt payload");
    assert!(matches!(
        reopened
            .verify(VerifyObjectRequest {
                bucket_id: bucket.id,
                key: ObjectKey::new("durable/object").expect("key"),
            })
            .await,
        Err(StorageError::IntegrityMismatch)
    ));
}
