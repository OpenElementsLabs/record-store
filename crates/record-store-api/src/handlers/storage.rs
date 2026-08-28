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
