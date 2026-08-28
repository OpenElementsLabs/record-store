use axum::{
    Json,
    extract::{Extension, State},
};

use crate::dto::{Capabilities, StatusResponse, SystemInfoResponse};
use crate::error::ApiError;
use crate::handlers::cluster::collect_cluster_status;
use crate::*;

pub(crate) async fn health() -> Json<StatusResponse> {
    Json(StatusResponse { status: "ok" })
}

pub(crate) async fn ready(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StatusResponse>, ApiError> {
    ensure_ready(&state, request_id).await?;
    Ok(Json(StatusResponse { status: "ready" }))
}

pub(crate) async fn system_info(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<SystemInfoResponse>, ApiError> {
    ensure_ready(&state, request_id.clone()).await?;
    let cluster_id = match &state.cluster {
        Some(_) => Some(collect_cluster_status(&state, request_id).await?.cluster_id),
        None => None,
    };
    let capabilities = Capabilities::detect(&state);
    Ok(Json(SystemInfoResponse {
        name: "record-store",
        version: state.version,
        status: "ready",
        mode: state.mode,
        cluster_id,
        capabilities,
    }))
}
