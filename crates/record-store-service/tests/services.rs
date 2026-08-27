use std::{collections::BTreeMap, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::{TryStreamExt, stream};
use oes_core::{BucketName, ObjectKey, OrganizationId};
use oes_metadata::{MetadataRepository, RedbMetadataRepository};
use oes_service::{ServiceError, ServiceLimits, ServicePutRequest, Services};
use oes_storage::{LocalFilesystemStore, ObjectStore, upload_stream};
use tempfile::TempDir;
use tokio::time::timeout;

async fn services() -> (TempDir, Services) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let metadata_impl = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let metadata: Arc<dyn MetadataRepository> = metadata_impl;
    let storage_impl = Arc::new(
        LocalFilesystemStore::open(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("filesystem store"),
    );
    let storage: Arc<dyn ObjectStore> = storage_impl;
    let services = Services::new(
        storage,
        metadata,
        OrganizationId::new(),
        ServiceLimits {
            maximum_concurrent_operations: 8,
            maximum_custom_metadata_entries: 8,
            maximum_custom_metadata_bytes: 1_024,
        },
    );
    (directory, services)
}

#[tokio::test]
async fn active_streams_hold_bucket_lifecycle_but_survive_object_deletion() {
    let (_directory, services) = services().await;
    let bucket = BucketName::new("coordinated-bucket").expect("bucket");
    let key = ObjectKey::new("active/read").expect("key");
    services
        .buckets
        .create(bucket.clone())
        .await
        .expect("create bucket");
    services
        .objects
        .put(ServicePutRequest {
            bucket: bucket.clone(),
            key: key.clone(),
            content_type: Some("text/plain".into()),
            custom_metadata: BTreeMap::new(),
            expected_checksum: None,
            body: upload_stream(stream::once(async {
                Ok(Bytes::from_static(b"stable open read"))
            })),
        })
        .await
        .expect("put object");

    let held_read = services
        .objects
        .get(&bucket, key.clone(), None)
        .await
        .expect("open held read");
    let bucket_service = Arc::clone(&services.buckets);
    let delete_name = bucket.clone();
    let mut delete_bucket = tokio::spawn(async move { bucket_service.delete(&delete_name).await });
    assert!(
        timeout(Duration::from_millis(50), &mut delete_bucket)
            .await
            .is_err(),
        "bucket deletion must wait for an active object stream"
    );
    drop(held_read);
    assert!(matches!(
        delete_bucket.await.expect("bucket deletion task"),
        Err(ServiceError::BucketNotEmpty)
    ));

    let open_read = services
        .objects
        .get(&bucket, key.clone(), None)
        .await
        .expect("open read before deletion");
    assert!(
        services
            .objects
            .delete(&bucket, key.clone())
            .await
            .expect("delete visible object")
    );
    let bytes = open_read
        .body
        .try_fold(Vec::new(), |mut output, chunk| async move {
            output.extend_from_slice(&chunk);
            Ok(output)
        })
        .await
        .expect("finish already-open read");
    assert_eq!(bytes, b"stable open read");
    assert!(matches!(
        services.objects.head(&bucket, key).await,
        Err(ServiceError::ObjectNotFound)
    ));
    services
        .buckets
        .delete(&bucket)
        .await
        .expect("delete empty bucket");
}
