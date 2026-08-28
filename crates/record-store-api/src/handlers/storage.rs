use axum::{
    Json,
    extract::{Extension, Query, State},
};
use record_store_core::StorageUsage;
use record_store_storage::{StorageInspection, StorageRepairRequest, StorageRepairResult};
use serde::Deserialize;

use crate::dto::StorageStatusResponse;
use crate::error::{ApiError, internal_service_error, service_to_api_error};
use crate::*;

pub(crate) async fn storage_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageStatusResponse>, ApiError> {
    state
        .services
        .objects
        .status()
        .await
        .map(StorageStatusResponse::from)
        .map(Json)
        .map_err(|error| internal_service_error(error, request_id))
}

pub(crate) async fn storage_usage(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageUsage>, ApiError> {
    state
        .services
        .objects
        .usage()
        .await
        .map(Json)
        .map_err(|error| internal_service_error(error, request_id))
}

#[derive(Debug, Deserialize)]
pub(crate) struct StorageInspectionQuery {
    #[serde(default = "default_inspection_limit")]
    maximum_entries: usize,
}

pub(crate) const fn default_inspection_limit() -> usize {
    100_000
}

pub(crate) async fn storage_inspect(
    State(state): State<AppState>,
    Query(query): Query<StorageInspectionQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageInspection>, ApiError> {
    state
        .services
        .objects
        .inspect(query.maximum_entries)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Debug, Deserialize)]
pub(crate) struct StorageRepairInput {
    #[serde(default = "default_inspection_limit")]
    maximum_entries: usize,
    #[serde(default = "dry_run_by_default")]
    dry_run: bool,
}

pub(crate) const fn dry_run_by_default() -> bool {
    true
}

pub(crate) async fn storage_repair(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StorageRepairInput>,
) -> Result<Json<StorageRepairResult>, ApiError> {
    state
        .services
        .objects
        .repair(StorageRepairRequest {
            maximum_entries: input.maximum_entries,
            dry_run: input.dry_run,
        })
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{admin, api, expect_status};

    #[tokio::test]
    async fn storage_status_and_usage_are_readable_on_a_fresh_deployment() {
        let (_directory, api) = api().await;

        let status = expect_status(
            &api,
            admin("GET", "/api/v1/storage/status", None),
            StatusCode::OK,
        )
        .await;
        assert!(status.is_object(), "{status}");

        let usage = expect_status(
            &api,
            admin("GET", "/api/v1/storage/usage", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(usage["object_count"], 0, "{usage}");
        assert_eq!(usage["bucket_count"], 0, "{usage}");
    }

    #[tokio::test]
    async fn usage_follows_the_buckets_that_exist() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/buckets", Some(json!({"name": "photos"}))),
            StatusCode::CREATED,
        )
        .await;

        let usage = expect_status(
            &api,
            admin("GET", "/api/v1/storage/usage", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(usage["bucket_count"], 1, "{usage}");
    }

    /// Inspection reports orphan payloads. On a clean deployment it must report
    /// none rather than failing, because operators run it routinely.
    #[tokio::test]
    async fn inspection_of_a_clean_deployment_finds_nothing_to_repair() {
        let (_directory, api) = api().await;
        let report = expect_status(
            &api,
            admin("GET", "/api/v1/storage/inspect", None),
            StatusCode::OK,
        )
        .await;
        assert!(report.is_object(), "{report}");
    }

    /// Repair defaults to a dry run. A request that omits the flag must not
    /// delete anything, because the destructive form has to be deliberate.
    #[tokio::test]
    async fn repair_defaults_to_reporting_rather_than_deleting() {
        let (_directory, api) = api().await;
        let dry_run = expect_status(
            &api,
            admin("POST", "/api/v1/storage/repair", Some(json!({}))),
            StatusCode::OK,
        )
        .await;
        assert!(dry_run.is_object(), "{dry_run}");

        let applied = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/storage/repair",
                Some(json!({"dry_run": false})),
            ),
            StatusCode::OK,
        )
        .await;
        assert!(applied.is_object(), "{applied}");
    }

    #[tokio::test]
    async fn an_inspection_limit_is_accepted_from_the_query_string() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("GET", "/api/v1/storage/inspect?maximum_entries=5", None),
            StatusCode::OK,
        )
        .await;
    }
}
