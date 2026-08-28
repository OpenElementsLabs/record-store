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
