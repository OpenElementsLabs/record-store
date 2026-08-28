use std::str::FromStr;

use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use record_store_auth::ServiceAccountInfo;
use record_store_core::ServiceAccountId;
use serde::{Deserialize, Serialize};
use tracing::error;
use uuid::Uuid;

use crate::error::ApiError;
use crate::*;

pub(crate) async fn list_service_accounts(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<ServiceAccountInfo>>, ApiError> {
    state
        .credentials
        .list_service_accounts()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential operation failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
pub(crate) struct CreateServiceAccountRequest {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Serialize)]
pub(crate) struct IssuedServiceAccountResponse {
    account: record_store_auth::ServiceAccount,
    credential: record_store_auth::Credential,
    secret_access_key: String,
}

pub(crate) async fn create_service_account(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateServiceAccountRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    let issued = state
        .credentials
        .create_service_account_with_description(input.name, input.description, state.owner)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential issuance failed");
            ApiError::bad_request(
                request_id.clone(),
                "INVALID_SERVICE_ACCOUNT",
                "Invalid service account",
            )
        })?;
    let secret_access_key =
        String::from_utf8(issued.secret.expose().to_vec()).map_err(|error| {
            error!(%error, request_id = %request_id, "generated credential was not UTF-8");
            ApiError::internal(request_id)
        })?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

pub(crate) async fn get_service_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ServiceAccountInfo>, ApiError> {
    let id = ServiceAccountId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_SERVICE_ACCOUNT_ID",
            "Invalid service account ID",
        )
    })?;
    state
        .credentials
        .get_service_account(id)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account lookup failed");
            ApiError::internal(request_id)
        })
}

pub(crate) async fn delete_service_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    state
        .credentials
        .delete_service_account(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account deletion failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
pub(crate) struct StatusChangeRequest {
    enabled: bool,
}

pub(crate) async fn set_service_account_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StatusChangeRequest>,
) -> Result<Json<ServiceAccountInfo>, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    state
        .credentials
        .set_service_account_enabled(id, input.enabled)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account status update failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
pub(crate) struct RotateCredentialRequest {
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub(crate) async fn rotate_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<RotateCredentialRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    let issued = state
        .credentials
        .rotate_credential(id, input.expires_at)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential rotation failed");
            ApiError::internal(request_id.clone())
        })?;
    let secret_access_key = String::from_utf8(issued.secret.expose().to_vec())
        .map_err(|_| ApiError::internal(request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct TemporaryCredentialRequest {
    expires_in_seconds: u64,
}

pub(crate) async fn issue_temporary_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<TemporaryCredentialRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    if !(60..=86_400).contains(&input.expires_in_seconds) {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_EXPIRATION",
            "Temporary credentials must expire between 60 and 86400 seconds",
        ));
    }
    let id = parse_service_account_id(&id, &request_id)?;
    let seconds = i64::try_from(input.expires_in_seconds).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_EXPIRATION",
            "Temporary credential expiration is invalid",
        )
    })?;
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(seconds);
    let issued = state
        .credentials
        .rotate_credential(id, Some(expires_at))
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "temporary credential issuance failed");
            ApiError::internal(request_id.clone())
        })?;
    let secret_access_key = String::from_utf8(issued.secret.expose().to_vec())
        .map_err(|_| ApiError::internal(request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

pub(crate) async fn set_credential_status(
    State(state): State<AppState>,
    Path((id, credential_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StatusChangeRequest>,
) -> Result<StatusCode, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    let credential_id = Uuid::parse_str(&credential_id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_CREDENTIAL_ID",
            "Invalid credential ID",
        )
    })?;
    state
        .credentials
        .set_credential_enabled(id, credential_id, input.enabled)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential status update failed");
            ApiError::internal(request_id)
        })
}

pub(crate) fn parse_service_account_id(
    value: &str,
    request_id: &RequestId,
) -> Result<ServiceAccountId, ApiError> {
    ServiceAccountId::from_str(value).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_SERVICE_ACCOUNT_ID",
            "Invalid service account ID",
        )
    })
}
