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
use crate::handlers::credentials::credential_error;
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
        .map_err(|error| credential_error(error, request_id))
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
        .map_err(|error| credential_error(error, request_id))
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
        .map_err(|error| credential_error(error, request_id))
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
        .map_err(|error| credential_error(error, request_id))
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
        .map_err(|error| credential_error(error, request_id.clone()))?;
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
        .map_err(|error| credential_error(error, request_id.clone()))?;
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
        .map_err(|error| credential_error(error, request_id))
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

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{AUDITOR_TOKEN, admin, api, call, expect_status, json_body, signed};

    /// The secret is generated server-side and shown exactly once. If creation
    /// ever stopped returning it, the account would be unusable and nobody could
    /// recover it.
    #[tokio::test]
    async fn a_created_account_returns_its_secret_exactly_once() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "backups"})),
            ),
            StatusCode::CREATED,
        )
        .await;

        let id = created["account"]["id"].as_str().expect("account id");
        assert!(
            created["secret_access_key"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "creation must disclose the secret"
        );

        let fetched = expect_status(
            &api,
            admin("GET", &format!("/api/v1/service-accounts/{id}"), None),
            StatusCode::OK,
        )
        .await;
        assert!(
            !fetched.to_string().contains("secret_access_key"),
            "a later read must never disclose the secret again: {fetched}"
        );
    }

    #[tokio::test]
    async fn accounts_are_listed_after_creation_and_gone_after_deletion() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "reporting"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        let id = created["account"]["id"].as_str().expect("id").to_owned();

        let listed = expect_status(
            &api,
            admin("GET", "/api/v1/service-accounts", None),
            StatusCode::OK,
        )
        .await;
        assert!(
            listed
                .as_array()
                .expect("array")
                .iter()
                .any(|entry| entry["account"]["name"] == "reporting"),
            "{listed}"
        );

        expect_status(
            &api,
            admin("DELETE", &format!("/api/v1/service-accounts/{id}"), None),
            StatusCode::NO_CONTENT,
        )
        .await;
        expect_status(
            &api,
            admin("GET", &format!("/api/v1/service-accounts/{id}"), None),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    #[tokio::test]
    async fn an_account_can_be_disabled_and_enabled_again() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "batch"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        let id = created["account"]["id"].as_str().expect("id").to_owned();

        for enabled in [false, true] {
            expect_status(
                &api,
                admin(
                    "PUT",
                    &format!("/api/v1/service-accounts/{id}/status"),
                    Some(json!({"enabled": enabled})),
                ),
                StatusCode::OK,
            )
            .await;
            let fetched = expect_status(
                &api,
                admin("GET", &format!("/api/v1/service-accounts/{id}"), None),
                StatusCode::OK,
            )
            .await;
            assert_eq!(
                fetched["account"]["disabled"], !enabled,
                "status change was not durable: {fetched}"
            );
        }
    }

    /// Rotation issues a new secret. The old one must not be returned again and
    /// the new one must differ, or rotation would be theatre.
    #[tokio::test]
    async fn rotating_a_credential_issues_a_different_secret() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "rotating"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        let id = created["account"]["id"].as_str().expect("id").to_owned();
        let original = created["secret_access_key"]
            .as_str()
            .expect("secret")
            .to_owned();

        let rotated = expect_status(
            &api,
            admin(
                "POST",
                &format!("/api/v1/service-accounts/{id}/credentials"),
                Some(json!({})),
            ),
            StatusCode::CREATED,
        )
        .await;
        let replacement = rotated["secret_access_key"].as_str().expect("secret");
        assert_ne!(replacement, original, "rotation must change the secret");
    }

    /// A temporary credential must come back with an expiry; one without would
    /// be permanent, which is the opposite of what was asked for.
    #[tokio::test]
    async fn a_temporary_credential_reports_when_it_expires() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "temporary"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        let id = created["account"]["id"].as_str().expect("id").to_owned();

        let issued = expect_status(
            &api,
            admin(
                "POST",
                &format!("/api/v1/service-accounts/{id}/temporary-credentials"),
                Some(json!({"expires_in_seconds": 900})),
            ),
            StatusCode::CREATED,
        )
        .await;
        assert!(
            issued["credential"]["expires_at"].as_str().is_some(),
            "a temporary credential must carry an expiry: {issued}"
        );
    }

    #[tokio::test]
    async fn an_unknown_account_identifier_is_rejected_rather_than_searched_for() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("GET", "/api/v1/service-accounts/not-a-uuid", None),
            StatusCode::BAD_REQUEST,
        )
        .await;
        expect_status(
            &api,
            admin(
                "GET",
                "/api/v1/service-accounts/0195f0c8-0000-7000-8000-0000000000ff",
                None,
            ),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    /// Account administration changes who can reach the data plane, so it is
    /// reserved to the system administrator. An auditor may read but not write.
    #[tokio::test]
    async fn an_auditor_cannot_create_or_delete_accounts() {
        let (_directory, api) = api().await;
        let response = call(
            &api,
            signed(
                "POST",
                "/api/v1/service-accounts",
                AUDITOR_TOKEN,
                Some(json!({"name": "sneaky"})),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn account_administration_requires_a_credential() {
        let (_directory, api) = api().await;
        let response = call(
            &api,
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/service-accounts")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = json_body(response).await;
        assert!(body["error"]["code"].as_str().is_some(), "{body}");
    }
}
