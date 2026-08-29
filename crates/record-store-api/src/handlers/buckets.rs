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
    /// Storage class new objects are placed on. Omitted uses the default class.
    #[serde(default)]
    storage_class: Option<String>,
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
    let storage_class = match input.storage_class {
        Some(value) => Some(record_store_core::StorageClass::new(value).map_err(|_| {
            ApiError::bad_request(
                request_id.clone(),
                "INVALID_STORAGE_CLASS",
                "Storage class must be 1 to 32 lowercase letters, digits, or hyphens",
            )
        })?),
        None => None,
    };
    state
        .services
        .buckets
        .create_on(name, storage_class)
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

#[cfg(test)]
mod tests {
    /// A bucket records the class it was created on, and one that chose nothing
    /// records nothing rather than being pinned to today's default.
    #[tokio::test]
    async fn a_bucket_records_the_storage_class_it_was_created_on() {
        use axum::http::StatusCode;
        use serde_json::json;

        use crate::test_support::{admin, api, expect_status};

        let (_directory, api) = api().await;

        let default = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets",
                Some(json!({"name": "unqualified"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        assert!(
            default["storage_class"].is_null(),
            "a bucket that chose no class must not be pinned to one: {default}"
        );

        let chosen = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets",
                Some(json!({"name": "qualified", "storage_class": "archive"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        assert_eq!(chosen["storage_class"], "archive");

        let rejected = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets",
                Some(json!({"name": "bad-class", "storage_class": "NOT VALID"})),
            ),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(rejected["error"]["code"], "INVALID_STORAGE_CLASS");
    }

    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{AUDITOR_TOKEN, admin, api, call, expect_status, signed};

    #[tokio::test]
    async fn a_bucket_appears_in_the_listing_once_created_and_goes_when_deleted() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/buckets", Some(json!({"name": "photos"}))),
            StatusCode::CREATED,
        )
        .await;

        let listed =
            expect_status(&api, admin("GET", "/api/v1/buckets", None), StatusCode::OK).await;
        assert!(
            listed
                .as_array()
                .expect("array")
                .iter()
                .any(|entry| entry["name"] == "photos"),
            "{listed}"
        );

        expect_status(
            &api,
            admin("DELETE", "/api/v1/buckets/photos", None),
            StatusCode::NO_CONTENT,
        )
        .await;
        let empty =
            expect_status(&api, admin("GET", "/api/v1/buckets", None), StatusCode::OK).await;
        assert!(empty.as_array().expect("array").is_empty(), "{empty}");
    }

    /// The name is validated by the domain type, so an unusable name has to be
    /// refused at the edge rather than reaching the catalog.
    #[tokio::test]
    async fn an_invalid_bucket_name_is_refused_before_it_reaches_the_catalog() {
        let (_directory, api) = api().await;
        for name in ["", "UPPERCASE", "a", "has spaces", "192.168.1.1"] {
            let response = call(
                &api,
                admin("POST", "/api/v1/buckets", Some(json!({"name": name}))),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "accepted invalid bucket name {name:?}"
            );
        }
    }

    #[tokio::test]
    async fn creating_the_same_bucket_twice_reports_the_conflict() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/buckets", Some(json!({"name": "photos"}))),
            StatusCode::CREATED,
        )
        .await;
        let response = call(
            &api,
            admin("POST", "/api/v1/buckets", Some(json!({"name": "photos"}))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn deleting_a_bucket_that_was_never_created_is_a_not_found() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("DELETE", "/api/v1/buckets/absent", None),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    /// Versioning is a three-state switch and every transition must be durable,
    /// because suspending it changes how every later write behaves.
    #[tokio::test]
    async fn versioning_transitions_are_readable_after_they_are_applied() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/buckets", Some(json!({"name": "photos"}))),
            StatusCode::CREATED,
        )
        .await;

        for state in ["enabled", "suspended"] {
            expect_status(
                &api,
                admin(
                    "PUT",
                    "/api/v1/buckets/photos/versioning",
                    Some(json!({"versioning": state})),
                ),
                StatusCode::OK,
            )
            .await;
            let read = expect_status(
                &api,
                admin("GET", "/api/v1/buckets/photos/versioning", None),
                StatusCode::OK,
            )
            .await;
            assert_eq!(read["versioning"], state, "{read}");
        }
    }

    #[tokio::test]
    async fn a_quota_can_be_set_and_lifted_again() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/buckets", Some(json!({"name": "photos"}))),
            StatusCode::CREATED,
        )
        .await;

        expect_status(
            &api,
            admin(
                "PUT",
                "/api/v1/buckets/photos/quota",
                Some(json!({
                    "quota": {
                        "bytes": {"mode": "limit", "bytes": 4096},
                        "objects": {"mode": "limit", "objects": 10},
                    }
                })),
            ),
            StatusCode::OK,
        )
        .await;

        let listed =
            expect_status(&api, admin("GET", "/api/v1/buckets", None), StatusCode::OK).await;
        let bucket = listed
            .as_array()
            .expect("array")
            .iter()
            .find(|entry| entry["name"] == "photos")
            .expect("bucket");
        assert_eq!(bucket["quota"]["bytes"]["bytes"], 4096, "{bucket}");
        assert_eq!(bucket["quota"]["objects"]["objects"], 10, "{bucket}");
    }

    /// Bucket lifecycle is a storage-administration action, so an auditor may
    /// read the listing but must not be able to create or destroy a bucket.
    #[tokio::test]
    async fn an_auditor_may_read_buckets_but_not_change_them() {
        let (_directory, api) = api().await;
        let read = call(&api, signed("GET", "/api/v1/buckets", AUDITOR_TOKEN, None)).await;
        assert_eq!(read.status(), StatusCode::OK);

        let write = call(
            &api,
            signed(
                "POST",
                "/api/v1/buckets",
                AUDITOR_TOKEN,
                Some(json!({"name": "photos"})),
            ),
        )
        .await;
        assert_eq!(write.status(), StatusCode::FORBIDDEN);
    }
}
