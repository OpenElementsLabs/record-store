//! Shared bucket and object application services.

use std::sync::atomic::Ordering;

use chrono::Utc;
use record_store_core::{
    BucketName, MultipartUpload, MultipartUploadState, ObjectKey, PartNumber, UploadId,
    UploadedPart,
};
use record_store_events::{StorageEvent, StorageEventType};
use record_store_metadata::ListMultipartUploadsRequest as MetadataMultipartListRequest;
use record_store_storage::{CompleteMultipartRequest, PutMultipartPartRequest, PutObjectResult};

use crate::error::map_storage;
use crate::events::publish_event;
use crate::*;

impl ObjectService {
    /// Starts a durable resumable multipart upload.
    pub async fn create_multipart(
        &self,
        request: ServiceCreateMultipartRequest,
    ) -> Result<MultipartUpload, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        self.validate_custom_metadata(&request.custom_metadata)?;
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let upload = MultipartUpload {
            id: UploadId::new(),
            bucket_id: bucket.id,
            key: request.key,
            content_type: request.content_type,
            custom_metadata: request.custom_metadata,
            initiated_at: Utc::now(),
            state: MultipartUploadState::Active,
        };
        self.metadata.create_multipart_upload(&upload).await?;
        Ok(upload)
    }

    /// Streams one multipart part directly to durable immutable storage.
    pub async fn upload_part(
        &self,
        request: ServiceUploadPartRequest,
    ) -> Result<UploadedPart, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let upload = self
            .metadata
            .get_multipart_upload(request.upload_id)
            .await?
            .ok_or(ServiceError::MultipartUploadNotFound)?;
        if upload.bucket_id != bucket.id || upload.key != request.key {
            return Err(ServiceError::MultipartUploadNotFound);
        }
        self.storage
            .put_multipart_part(PutMultipartPartRequest {
                upload_id: upload.id,
                number: request.number,
                expected_checksum: request.expected_checksum,
                body: request.body,
            })
            .await
            .map_err(map_storage)
    }

    /// Returns a bounded ascending part page.
    pub async fn list_parts(
        &self,
        bucket_name: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
        after: Option<PartNumber>,
        maximum_parts: usize,
    ) -> Result<Vec<UploadedPart>, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        if maximum_parts > 1_001 {
            return Err(ServiceError::InvalidRequest(
                "maximum_parts must not exceed 1001".into(),
            ));
        }
        let bucket = self.resolve_bucket(bucket_name).await?;
        let upload = self
            .metadata
            .get_multipart_upload(upload_id)
            .await?
            .ok_or(ServiceError::MultipartUploadNotFound)?;
        if upload.bucket_id != bucket.id || upload.key != *key {
            return Err(ServiceError::MultipartUploadNotFound);
        }
        self.metadata
            .list_multipart_parts(upload_id, after, maximum_parts)
            .await
            .map_err(ServiceError::Metadata)
    }

    /// Validates a manifest and atomically publishes the composed object.
    pub async fn complete_multipart(
        &self,
        request: ServiceCompleteMultipartRequest,
    ) -> Result<PutObjectResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        let upload = self
            .metadata
            .get_multipart_upload(request.upload_id)
            .await?
            .ok_or(ServiceError::MultipartUploadNotFound)?;
        if upload.bucket_id != bucket.id || upload.key != request.key {
            return Err(ServiceError::MultipartUploadNotFound);
        }
        if request.manifest.is_empty() || request.manifest.len() > PartNumber::MAX as usize {
            return Err(ServiceError::InvalidPart);
        }
        if !request
            .manifest
            .windows(2)
            .all(|parts| parts[0].number < parts[1].number)
        {
            return Err(ServiceError::InvalidPartOrder);
        }
        let persisted = self
            .metadata
            .list_multipart_parts(upload.id, None, PartNumber::MAX as usize)
            .await?;
        let by_number = persisted
            .into_iter()
            .map(|part| (part.number, part))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut parts = Vec::with_capacity(request.manifest.len());
        for (index, item) in request.manifest.iter().enumerate() {
            let part = by_number
                .get(&item.number)
                .filter(|part| part.etag == item.etag)
                .ok_or(ServiceError::InvalidPart)?;
            if index + 1 != request.manifest.len() && part.size < 5 * 1024 * 1024 {
                return Err(ServiceError::EntityTooSmall);
            }
            parts.push(part.clone());
        }
        let result = self
            .storage
            .complete_multipart(CompleteMultipartRequest { upload, parts })
            .await
            .map_err(map_storage)?;
        self.metrics
            .upload_bytes
            .fetch_add(result.metadata.size, Ordering::Relaxed);
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::MultipartCompleted, bucket.name.as_str()).object(
                result.metadata.key.as_str(),
                Some(result.metadata.version_id),
                Some(result.metadata.size),
            ),
        )
        .await;
        Ok(result)
    }

    /// Aborts an upload and schedules all durable parts for cleanup.
    pub async fn abort_multipart(
        &self,
        bucket_name: &BucketName,
        key: &ObjectKey,
        upload_id: UploadId,
    ) -> Result<(), ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let upload = self
            .metadata
            .get_multipart_upload(upload_id)
            .await?
            .ok_or(ServiceError::MultipartUploadNotFound)?;
        if upload.bucket_id != bucket.id || upload.key != *key {
            return Err(ServiceError::MultipartUploadNotFound);
        }
        self.metadata.abort_multipart_upload(upload_id).await?;
        if let Err(error) = self.storage.cleanup_pending(10_000).await {
            tracing::warn!(%error, %upload_id, "multipart abort payload cleanup deferred");
        }
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::MultipartAborted, bucket.name.as_str()).object(
                key.as_str(),
                None,
                None,
            ),
        )
        .await;
        Ok(())
    }

    /// Lists active multipart uploads using indexed metadata pagination.
    pub async fn list_multipart_uploads(
        &self,
        request: ServiceListMultipartUploadsRequest,
    ) -> Result<ServiceListMultipartUploadsResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        if request.maximum_uploads > 1_000 {
            return Err(ServiceError::InvalidRequest(
                "maximum_uploads must not exceed 1000".into(),
            ));
        }
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let page = self
            .metadata
            .list_multipart_uploads(MetadataMultipartListRequest {
                bucket_id: bucket.id,
                prefix: request.prefix,
                upload_id_marker: request.upload_id_marker,
                limit: request.maximum_uploads,
            })
            .await?;
        Ok(ServiceListMultipartUploadsResult {
            uploads: page.uploads,
            next_upload_id_marker: page.next_upload_id_marker,
        })
    }
}

#[cfg(test)]
mod tests {
    use record_store_core::{BucketName, CompletedPart, ETag, ObjectKey, PartNumber, UploadId};

    use crate::test_support::{body, services};
    use crate::{
        ServiceCompleteMultipartRequest, ServiceCreateMultipartRequest, ServiceError,
        ServiceUploadPartRequest, Services,
    };

    const MINIMUM_PART: usize = 5 * 1024 * 1024;

    fn bucket() -> BucketName {
        BucketName::new("multipart-tests").expect("bucket name")
    }

    fn key() -> ObjectKey {
        ObjectKey::new("archive.bin").expect("object key")
    }

    /// Starts an upload and stores `sizes.len()` parts, returning the upload and
    /// the ETag the store recorded for each part.
    async fn upload_with(services: &Services, sizes: &[usize]) -> (UploadId, Vec<CompletedPart>) {
        services.buckets.create(bucket()).await.expect("bucket");
        let upload = services
            .objects
            .create_multipart(ServiceCreateMultipartRequest {
                bucket: bucket(),
                key: key(),
                content_type: None,
                custom_metadata: Default::default(),
            })
            .await
            .expect("create multipart");

        let mut manifest = Vec::new();
        for (index, size) in sizes.iter().enumerate() {
            let number = PartNumber::new(u16::try_from(index + 1).expect("part number"))
                .expect("part number");
            let stored = services
                .objects
                .upload_part(ServiceUploadPartRequest {
                    bucket: bucket(),
                    key: key(),
                    upload_id: upload.id,
                    number,
                    expected_checksum: None,
                    body: body(&vec![b'x'; *size]),
                })
                .await
                .expect("upload part");
            manifest.push(CompletedPart {
                number,
                etag: stored.etag,
            });
        }
        (upload.id, manifest)
    }

    async fn complete(
        services: &Services,
        upload_id: UploadId,
        manifest: Vec<CompletedPart>,
    ) -> Result<(), ServiceError> {
        services
            .objects
            .complete_multipart(ServiceCompleteMultipartRequest {
                bucket: bucket(),
                key: key(),
                upload_id,
                manifest,
            })
            .await
            .map(|_| ())
    }

    /// The manifest is the client's assertion about assembly order. Accepting a
    /// descending or repeated part number would silently produce an object whose
    /// bytes are not what the client uploaded.
    #[tokio::test]
    async fn a_manifest_that_is_not_strictly_ascending_is_refused() {
        let (_directory, services) = services().await;
        let (upload_id, manifest) = upload_with(&services, &[MINIMUM_PART, 16]).await;

        let mut descending = manifest.clone();
        descending.reverse();
        assert!(matches!(
            complete(&services, upload_id, descending).await,
            Err(ServiceError::InvalidPartOrder)
        ));

        let repeated = vec![manifest[0].clone(), manifest[0].clone()];
        assert!(
            matches!(
                complete(&services, upload_id, repeated).await,
                Err(ServiceError::InvalidPartOrder)
            ),
            "a repeated part number is not strictly ascending"
        );
    }

    #[tokio::test]
    async fn an_empty_manifest_is_refused_rather_than_committing_nothing() {
        let (_directory, services) = services().await;
        let (upload_id, _) = upload_with(&services, &[16]).await;
        assert!(matches!(
            complete(&services, upload_id, Vec::new()).await,
            Err(ServiceError::InvalidPart)
        ));
    }

    /// The ETag is what ties the manifest entry to the bytes actually stored.
    /// A mismatch means the client is describing a part the store does not have.
    #[tokio::test]
    async fn a_manifest_entry_whose_etag_does_not_match_the_stored_part_is_refused() {
        let (_directory, services) = services().await;
        let (upload_id, mut manifest) = upload_with(&services, &[16]).await;
        manifest[0].etag = ETag::new("00000000000000000000000000000000").expect("etag");
        assert!(matches!(
            complete(&services, upload_id, manifest).await,
            Err(ServiceError::InvalidPart)
        ));
    }

    /// S3 requires every part except the last to reach the minimum size. The
    /// final part is deliberately exempt, so both halves of the rule are pinned.
    #[tokio::test]
    async fn only_the_final_part_may_be_below_the_minimum_size() {
        let (_first_directory, first) = services().await;
        let (small_first, manifest) = upload_with(&first, &[16, MINIMUM_PART]).await;
        assert!(
            matches!(
                complete(&first, small_first, manifest).await,
                Err(ServiceError::EntityTooSmall)
            ),
            "a small non-final part must be refused"
        );

        let (_second_directory, second) = services().await;
        let (small_last, manifest) = upload_with(&second, &[MINIMUM_PART, 16]).await;
        assert!(
            complete(&second, small_last, manifest).await.is_ok(),
            "a small final part is allowed"
        );
    }

    /// An upload identifier is not a capability for any key. Completing against
    /// a different key must not be able to reach another upload's parts.
    #[tokio::test]
    async fn an_upload_cannot_be_completed_against_a_different_key() {
        let (_directory, services) = services().await;
        let (upload_id, manifest) = upload_with(&services, &[16]).await;

        let result = services
            .objects
            .complete_multipart(ServiceCompleteMultipartRequest {
                bucket: bucket(),
                key: ObjectKey::new("somewhere-else.bin").expect("object key"),
                upload_id,
                manifest,
            })
            .await;
        assert!(matches!(result, Err(ServiceError::MultipartUploadNotFound)));
    }

    #[tokio::test]
    async fn an_unknown_upload_identifier_is_not_found() {
        let (_directory, services) = services().await;
        services.buckets.create(bucket()).await.expect("bucket");
        assert!(matches!(
            complete(&services, UploadId::new(), Vec::new()).await,
            Err(ServiceError::MultipartUploadNotFound)
        ));
    }
}
