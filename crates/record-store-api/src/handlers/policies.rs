use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use record_store_auth::{Policy, PolicyStatement};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use crate::error::ApiError;
use crate::handlers::accounts::parse_service_account_id;
use crate::*;

#[derive(Deserialize)]
pub(crate) struct CreatePolicyRequest {
    name: String,
    #[serde(default)]
    description: String,
    statements: Vec<PolicyStatement>,
}

pub(crate) async fn create_policy(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<Policy>), ApiError> {
    state
        .credentials
        .create_policy(input.name, input.description, input.statements)
        .await
        .map(|policy| (StatusCode::CREATED, Json(policy)))
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy creation failed");
            ApiError::bad_request(request_id, "INVALID_POLICY", "Invalid policy")
        })
}

pub(crate) async fn list_policies(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<Policy>>, ApiError> {
    state
        .credentials
        .list_policies()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy listing failed");
            ApiError::internal(request_id)
        })
}

pub(crate) async fn attach_policy(
    State(state): State<AppState>,
    Path((policy_id, account_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let policy_id = Uuid::parse_str(&policy_id).map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_POLICY_ID", "Invalid policy ID")
    })?;
    let account_id = parse_service_account_id(&account_id, &request_id)?;
    state
        .credentials
        .attach_policy(account_id, policy_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy attachment failed");
            ApiError::internal(request_id)
        })
}

pub(crate) async fn detach_policy(
    State(state): State<AppState>,
    Path((policy_id, account_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let policy_id = Uuid::parse_str(&policy_id).map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_POLICY_ID", "Invalid policy ID")
    })?;
    let account_id = parse_service_account_id(&account_id, &request_id)?;
    state
        .credentials
        .detach_policy(account_id, policy_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy detachment failed");
            ApiError::internal(request_id)
        })
}
