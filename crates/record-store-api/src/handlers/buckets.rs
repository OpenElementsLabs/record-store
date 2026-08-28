use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use record_store_core::{Bucket, BucketName, BucketQuota, VersioningState};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::dto::BucketSummary;
use crate::error::{ApiError, internal_service_error, service_to_api_error};
use crate::*;

pub(crate) async fn list_buckets(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<BucketSummary>>, ApiError> {
    let buckets = state
        .services
        .buckets
        .list()
        .await
        .map_err(|error| internal_service_error(error, request_id.clone()))?;
    // Usage for every bucket is read in one pass so rendering a bucket table
    // never turns into one request per row.
    let usage = state.metadata.bucket_usage().await.map_err(|error| {
        error!(%error, request_id = %request_id, "bucket usage lookup failed");
        ApiError::internal(request_id)
    })?;
    Ok(Json(
        buckets
            .into_iter()
            .map(|bucket| {
                let counters = usage.get(&bucket.id).copied().unwrap_or_default();
                BucketSummary {
                    bucket,
                    object_count: counters.object_count,
                    logical_bytes: counters.logical_bytes,
                    version_count: counters.version_count,
                    version_bytes: counters.version_bytes,
                    multipart_bytes: counters.multipart_bytes,
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub(crate) struct CreateBucketRequest {
    name: String,
}

pub(crate) async fn create_bucket(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateBucketRequest>,
) -> Result<(StatusCode, Json<Bucket>), ApiError> {
    let name = BucketName::new(input.name).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .create(name)
        .await
        .map(|bucket| (StatusCode::CREATED, Json(bucket)))
        .map_err(|error| service_to_api_error(error, request_id))
}

pub(crate) async fn delete_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| service_to_api_error(error, request_id))
}

pub(crate) async fn get_bucket_versioning(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<VersioningResponse>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok(Json(VersioningResponse {
        versioning: bucket.versioning,
    }))
}

#[derive(Serialize)]
pub(crate) struct VersioningResponse {
    versioning: VersioningState,
}

#[derive(Deserialize)]
pub(crate) struct SetVersioningRequest {
    versioning: VersioningState,
}

pub(crate) async fn set_bucket_versioning(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<SetVersioningRequest>,
) -> Result<Json<Bucket>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .set_versioning(&name, input.versioning)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Deserialize)]
pub(crate) struct SetQuotaRequest {
    quota: BucketQuota,
}

pub(crate) async fn set_bucket_quota(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<SetQuotaRequest>,
) -> Result<Json<Bucket>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .set_quota(&name, input.quota)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}
