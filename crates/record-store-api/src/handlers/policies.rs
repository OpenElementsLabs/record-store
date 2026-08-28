use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use record_store_auth::{Policy, PolicyStatement};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::ApiError;
use crate::handlers::accounts::parse_service_account_id;
use crate::handlers::credentials::credential_error;
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
        .map_err(|error| credential_error(error, request_id))
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
        .map_err(|error| credential_error(error, request_id))
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
        .map_err(|error| credential_error(error, request_id))
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
        .map_err(|error| credential_error(error, request_id))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{AUDITOR_TOKEN, admin, api, call, expect_status, signed};

    fn document(name: &str) -> serde_json::Value {
        json!({
            "name": name,
            "description": "read-only access to one bucket",
            "statements": [{
                "effect": "allow",
                "actions": ["s3:GetObject", "s3:ListBucket"],
                "resources": ["bucket:photos", "bucket:photos/*"],
            }],
        })
    }

    async fn account(router: &axum::Router) -> String {
        let created = expect_status(
            router,
            admin(
                "POST",
                "/api/v1/service-accounts",
                Some(json!({"name": "bound"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        created["account"]["id"]
            .as_str()
            .expect("account id")
            .to_owned()
    }

    #[tokio::test]
    async fn a_policy_is_listed_once_created() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin("POST", "/api/v1/policies", Some(document("read-only"))),
            StatusCode::CREATED,
        )
        .await;
        assert_eq!(created["name"], "read-only", "{created}");

        let listed =
            expect_status(&api, admin("GET", "/api/v1/policies", None), StatusCode::OK).await;
        assert_eq!(listed.as_array().expect("array").len(), 1, "{listed}");
    }

    #[tokio::test]
    async fn a_duplicate_policy_name_is_a_conflict() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/policies", Some(document("read-only"))),
            StatusCode::CREATED,
        )
        .await;
        let response = call(
            &api,
            admin("POST", "/api/v1/policies", Some(document("read-only"))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    /// A policy with no statements grants nothing and denies nothing; storing
    /// one would leave an operator believing an account is constrained when it
    /// is not.
    #[tokio::test]
    async fn a_policy_without_statements_is_refused() {
        let (_directory, api) = api().await;
        let response = call(
            &api,
            admin(
                "POST",
                "/api/v1/policies",
                Some(json!({"name": "empty", "statements": []})),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// Binding is what actually changes an account's authority, so both
    /// directions have to work and be idempotent enough to retry.
    #[tokio::test]
    async fn a_policy_can_be_attached_to_an_account_and_detached_again() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin("POST", "/api/v1/policies", Some(document("read-only"))),
            StatusCode::CREATED,
        )
        .await;
        let policy = created["id"].as_str().expect("policy id").to_owned();
        let account = account(&api).await;

        expect_status(
            &api,
            admin(
                "PUT",
                &format!("/api/v1/policies/{policy}/bindings/{account}"),
                None,
            ),
            StatusCode::NO_CONTENT,
        )
        .await;

        let bound = expect_status(
            &api,
            admin("GET", &format!("/api/v1/service-accounts/{account}"), None),
            StatusCode::OK,
        )
        .await;
        assert!(
            !bound["policy_bindings"]
                .as_array()
                .expect("bindings")
                .is_empty(),
            "the binding must be visible on the account: {bound}"
        );

        expect_status(
            &api,
            admin(
                "DELETE",
                &format!("/api/v1/policies/{policy}/bindings/{account}"),
                None,
            ),
            StatusCode::NO_CONTENT,
        )
        .await;
        let unbound = expect_status(
            &api,
            admin("GET", &format!("/api/v1/service-accounts/{account}"), None),
            StatusCode::OK,
        )
        .await;
        assert!(
            unbound["policy_bindings"]
                .as_array()
                .expect("bindings")
                .is_empty(),
            "{unbound}"
        );
    }

    #[tokio::test]
    async fn binding_an_unknown_policy_or_account_is_refused() {
        let (_directory, api) = api().await;
        let absent = "0195f0c8-0000-7000-8000-0000000000ff";
        expect_status(
            &api,
            admin(
                "PUT",
                &format!("/api/v1/policies/{absent}/bindings/{absent}"),
                None,
            ),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    #[tokio::test]
    async fn a_malformed_identifier_in_a_binding_is_refused() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("PUT", "/api/v1/policies/not-a-uuid/bindings/also-not", None),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }

    /// Authorization policy is the security boundary itself, so an auditor must
    /// not be able to write one.
    #[tokio::test]
    async fn an_auditor_cannot_create_a_policy() {
        let (_directory, api) = api().await;
        let response = call(
            &api,
            signed(
                "POST",
                "/api/v1/policies",
                AUDITOR_TOKEN,
                Some(document("sneaky")),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
