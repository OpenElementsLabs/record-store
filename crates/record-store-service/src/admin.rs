//! Shared bucket and object application services.

use std::{
    io,
    sync::{Arc, atomic::Ordering},
};

use futures_util::TryStreamExt;
use record_store_core::{Bucket, BucketName, ObjectKey, ObjectMetadata, StorageUsage, VersionId};
use record_store_events::{StorageEvent, StorageEventType};
use record_store_metadata::ListObjectsRequest as MetadataListRequest;
use record_store_storage::{
    GetObjectRequest, GetObjectVersionRequest, PutObjectRequest, PutObjectResult,
    StorageInspection, StorageRepairRequest, StorageRepairResult, StorageStatus,
    VerifyObjectRequest, upload_stream,
};

use crate::error::map_storage;
use crate::events::publish_event;
use crate::*;

impl ObjectService {
    /// Streams a server-side copy without buffering payload bytes.
    pub async fn copy(&self, request: ServiceCopyRequest) -> Result<PutObjectResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        self.validate_custom_metadata(&request.replacement_metadata)?;
        let _permit = self.acquire().await?;
        let source_bucket = self.resolve_bucket(&request.source_bucket).await?;
        let destination_bucket = self.resolve_bucket(&request.destination_bucket).await?;
        let event_type = if self
            .metadata
            .get_object(destination_bucket.id, &request.destination_key)
            .await?
            .is_some()
        {
            StorageEventType::ObjectUpdated
        } else {
            StorageEventType::ObjectCreated
        };
        let source = if let Some(version_id) = request.source_version_id {
            self.storage
                .get_version(GetObjectVersionRequest {
                    bucket_id: source_bucket.id,
                    key: request.source_key,
                    version_id,
                    range: None,
                })
                .await
        } else {
            self.storage
                .get(GetObjectRequest {
                    bucket_id: source_bucket.id,
                    key: request.source_key,
                    range: None,
                })
                .await
        }
        .map_err(map_storage)?;
        let (content_type, custom_metadata) = match request.metadata_directive {
            CopyMetadataDirective::Copy => (
                source.metadata.content_type.clone(),
                source.metadata.custom_metadata.clone(),
            ),
            CopyMetadataDirective::Replace => (request.content_type, request.replacement_metadata),
        };
        let body = source
            .body
            .map_err(|error| io::Error::other(error.to_string()));
        let result = self
            .storage
            .put(PutObjectRequest {
                bucket_id: destination_bucket.id,
                key: request.destination_key.clone(),
                content_type,
                custom_metadata,
                expected_checksum: Some(source.metadata.checksum),
                object_id: None,
                protocol_etag: None,
                body: upload_stream(body),
            })
            .await
            .map_err(map_storage)?;
        self.metrics
            .upload_bytes
            .fetch_add(result.metadata.size, Ordering::Relaxed);
        publish_event(
            &self.events,
            StorageEvent::new(event_type, destination_bucket.name.as_str()).object(
                request.destination_key.as_str(),
                Some(result.metadata.version_id),
                Some(result.metadata.size),
            ),
        )
        .await;
        Ok(result)
    }

    /// Restores historical bytes by creating a fresh current version.
    pub async fn restore_version(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
        version_id: VersionId,
    ) -> Result<PutObjectResult, ServiceError> {
        let result = self
            .copy(ServiceCopyRequest {
                source_bucket: bucket_name.clone(),
                source_key: key.clone(),
                source_version_id: Some(version_id),
                destination_bucket: bucket_name.clone(),
                destination_key: key,
                metadata_directive: CopyMetadataDirective::Copy,
                content_type: None,
                replacement_metadata: Default::default(),
            })
            .await?;
        publish_event(
            &self.events,
            StorageEvent::new(StorageEventType::ObjectRestored, bucket_name.as_str()).object(
                result.metadata.key.as_str(),
                Some(result.metadata.version_id),
                Some(result.metadata.size),
            ),
        )
        .await;
        Ok(result)
    }

    /// Lists objects without loading the complete bucket into memory.
    pub async fn list(
        &self,
        request: ServiceListRequest,
    ) -> Result<ServiceListResult, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(&request.bucket).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        if request.maximum_keys > 1_000 {
            return Err(ServiceError::InvalidRequest(
                "maximum_keys must not exceed 1000".into(),
            ));
        }
        let mut result = ServiceListResult::default();
        if request.maximum_keys == 0 {
            return Ok(result);
        }
        let mut start_after = request.start_after;
        let mut at_capacity = false;
        let mut last_scanned_key = None;
        loop {
            let page = self
                .metadata
                .list_objects(MetadataListRequest {
                    bucket_id: bucket.id,
                    prefix: request.prefix.clone(),
                    start_after: start_after.clone(),
                    limit: 256,
                })
                .await?;
            if page.objects.is_empty() {
                break;
            }
            for metadata in &page.objects {
                let key = metadata.key.as_str();
                let common_prefix = request.delimiter.as_ref().and_then(|delimiter| {
                    key.strip_prefix(&request.prefix).and_then(|suffix| {
                        suffix.find(delimiter).map(|position| {
                            format!("{}{}{}", request.prefix, &suffix[..position], delimiter)
                        })
                    })
                });
                if at_capacity {
                    if common_prefix
                        .as_ref()
                        .is_some_and(|prefix| result.common_prefixes.contains(prefix))
                    {
                        last_scanned_key = Some(key.to_owned());
                        continue;
                    }
                    result.is_truncated = true;
                    result.next_marker = last_scanned_key;
                    return Ok(result);
                }

                if let Some(common_prefix) = common_prefix {
                    result.common_prefixes.insert(common_prefix);
                } else {
                    result.objects.push(metadata.clone());
                }
                last_scanned_key = Some(key.to_owned());
                at_capacity = result.entry_count() >= request.maximum_keys;
            }
            if page.next_key.is_none() {
                break;
            }
            start_after = page.next_key;
        }
        Ok(result)
    }

    /// Explicitly verifies a stored payload checksum.
    pub async fn verify(
        &self,
        bucket_name: &BucketName,
        key: ObjectKey,
    ) -> Result<ObjectMetadata, ServiceError> {
        self.metrics.requests.fetch_add(1, Ordering::Relaxed);
        let _permit = self.acquire().await?;
        let bucket = self.resolve_bucket(bucket_name).await?;
        let lock = self.coordinator.lock(bucket.id)?;
        let _guard = lock.read().await;
        self.storage
            .verify(VerifyObjectRequest {
                bucket_id: bucket.id,
                key,
            })
            .await
            .map_err(map_storage)
    }

    /// Returns aggregate logical usage counters.
    pub async fn usage(&self) -> Result<StorageUsage, ServiceError> {
        self.metadata
            .storage_usage()
            .await
            .map_err(ServiceError::Metadata)
    }

    /// Returns filesystem capacity and temporary usage.
    pub async fn status(&self) -> Result<StorageStatus, ServiceError> {
        self.storage.status().await.map_err(ServiceError::Storage)
    }

    /// Performs a bounded, read-only consistency inspection.
    pub async fn inspect(&self, maximum_entries: usize) -> Result<StorageInspection, ServiceError> {
        self.storage
            .inspect(maximum_entries)
            .await
            .map_err(ServiceError::Storage)
    }

    /// Performs an explicitly selected repair; callers should default to dry-run.
    pub async fn repair(
        &self,
        request: StorageRepairRequest,
    ) -> Result<StorageRepairResult, ServiceError> {
        self.storage
            .repair(request)
            .await
            .map_err(ServiceError::Storage)
    }

    pub(crate) fn validate_metadata(
        &self,
        request: &ServicePutRequest,
    ) -> Result<(), ServiceError> {
        self.validate_custom_metadata(&request.custom_metadata)
    }

    pub(crate) fn validate_custom_metadata(
        &self,
        custom_metadata: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), ServiceError> {
        if custom_metadata.len() > self.maximum_custom_metadata_entries {
            return Err(ServiceError::MetadataTooLarge);
        }
        let bytes = custom_metadata.iter().fold(0_usize, |total, (key, value)| {
            total.saturating_add(key.len()).saturating_add(value.len())
        });
        if bytes > self.maximum_custom_metadata_bytes {
            return Err(ServiceError::MetadataTooLarge);
        }
        Ok(())
    }

    pub(crate) async fn resolve_bucket(&self, name: &BucketName) -> Result<Bucket, ServiceError> {
        self.metadata
            .get_bucket_by_name(name)
            .await?
            .ok_or(ServiceError::BucketNotFound)
    }

    pub(crate) async fn acquire(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ServiceError> {
        Arc::clone(&self.operations)
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::Unavailable)
    }
}
