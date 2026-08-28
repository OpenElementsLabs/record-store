use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use record_store_core::{BucketName, ObjectKey, ObjectMetadata, VersionId};
use record_store_service::ServiceListRequest;
use serde::{Deserialize, Serialize};

use crate::dto::ObjectSummary;
use crate::error::{ApiError, service_to_api_error};
use crate::handlers::objects::{parse_bucket_name, parse_object_key};
use crate::*;

#[derive(Deserialize)]
pub(crate) struct RestoreVersionRequest {
    version_id: VersionId,
}

pub(crate) async fn restore_version(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<RestoreVersionRequest>,
) -> Result<(StatusCode, Json<ObjectSummary>), ApiError> {
    let bucket = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    state
        .services
        .objects
        .restore_version(&bucket, key, input.version_id)
        .await
        .map(|result| {
            (
                StatusCode::CREATED,
                Json(ObjectSummary::from(result.metadata)),
            )
        })
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CopyObjectRequest {
    /// Bucket the bytes are read from.
    source_bucket: String,
    /// Key the bytes are read from.
    source_key: String,
    /// Optional historical version to copy instead of the current one.
    #[serde(default)]
    source_version_id: Option<VersionId>,
    /// Replacement media type. Supplying any replacement field replaces the
    /// source's metadata rather than carrying it across.
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    custom_metadata: Option<std::collections::BTreeMap<String, String>>,
}

/// Copies an object server side.
///
/// The path names the destination, because that is what the request creates.
/// Bytes are streamed by the service layer and never buffered here, so copying
/// a large object costs the API process nothing beyond the transfer itself.
pub(crate) async fn copy_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CopyObjectRequest>,
) -> Result<(StatusCode, Json<ObjectSummary>), ApiError> {
    let destination_bucket = parse_bucket_name(&bucket, &request_id)?;
    let destination_key = parse_object_key(&key, &request_id)?;
    let source_bucket = parse_bucket_name(&input.source_bucket, &request_id)?;
    let source_key = parse_object_key(&input.source_key, &request_id)?;
    let replaces_metadata = input.content_type.is_some() || input.custom_metadata.is_some();
    state
        .services
        .objects
        .copy(record_store_service::ServiceCopyRequest {
            source_bucket,
            source_key,
            source_version_id: input.source_version_id,
            destination_bucket,
            destination_key,
            metadata_directive: if replaces_metadata {
                record_store_service::CopyMetadataDirective::Replace
            } else {
                record_store_service::CopyMetadataDirective::Copy
            },
            content_type: input.content_type,
            replacement_metadata: input.custom_metadata.unwrap_or_default(),
        })
        .await
        .map(|result| {
            (
                StatusCode::CREATED,
                Json(ObjectSummary::from(result.metadata)),
            )
        })
        .map_err(|error| service_to_api_error(error, request_id))
}

pub(crate) async fn verify_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectMetadata>, ApiError> {
    let bucket = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let key = ObjectKey::new(key).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_OBJECT_KEY",
            "Invalid object key",
        )
    })?;
    state
        .services
        .objects
        .verify(&bucket, key)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Serialize)]
pub(crate) struct VerifyBucketResponse {
    verified_objects: u64,
    failures: u64,
}

pub(crate) async fn verify_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<VerifyBucketResponse>, ApiError> {
    let bucket = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let mut marker = None;
    let mut verified = 0_u64;
    let mut failures = 0_u64;
    loop {
        let page = state
            .services
            .objects
            .list(ServiceListRequest {
                bucket: bucket.clone(),
                prefix: String::new(),
                delimiter: None,
                maximum_keys: 1_000,
                start_after: marker.clone(),
            })
            .await
            .map_err(|error| service_to_api_error(error, request_id.clone()))?;
        for object in &page.objects {
            if state
                .services
                .objects
                .verify(&bucket, object.key.clone())
                .await
                .is_ok()
            {
                verified += 1;
            } else {
                failures += 1;
            }
        }
        if !page.is_truncated {
            break;
        }
        marker = page.next_marker;
    }
    Ok(Json(VerifyBucketResponse {
        verified_objects: verified,
        failures,
    }))
}
