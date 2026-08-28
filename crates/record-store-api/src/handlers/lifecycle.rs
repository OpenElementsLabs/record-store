use std::str::FromStr;

use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use record_store_core::{BucketName, ExpirationDays, LifecycleRule, LifecycleRuleId};
use serde::Deserialize;
use tracing::error;

use crate::error::{ApiError, service_to_api_error};
use crate::handlers::objects::parse_bucket_name;
use crate::*;

#[derive(Debug, Deserialize)]
pub(crate) struct CreateLifecycleRuleRequest {
    #[serde(default)]
    prefix: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    expiration: Option<ExpirationDays>,
    noncurrent_version_expiration: Option<ExpirationDays>,
}

pub(crate) const fn enabled_by_default() -> bool {
    true
}

pub(crate) async fn create_lifecycle_rule(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateLifecycleRuleRequest>,
) -> Result<(StatusCode, Json<LifecycleRule>), ApiError> {
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
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    let now = chrono::Utc::now();
    let rule = LifecycleRule {
        id: LifecycleRuleId::new(),
        bucket_id: bucket.id,
        prefix: input.prefix,
        enabled: input.enabled,
        expiration: input.expiration,
        noncurrent_version_expiration: input.noncurrent_version_expiration,
        created_at: now,
        updated_at: now,
    };
    rule.validate().map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE",
            "Lifecycle rule is invalid",
        )
    })?;
    state
        .metadata
        .put_lifecycle_rule(&rule)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule creation failed");
            ApiError::internal(request_id)
        })?;
    Ok((StatusCode::CREATED, Json(rule)))
}

pub(crate) async fn list_lifecycle_rules(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<LifecycleRule>>, ApiError> {
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
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    state
        .metadata
        .list_lifecycle_rules(Some(bucket.id))
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule listing failed");
            ApiError::internal(request_id)
        })
}

/// A complete replacement for one lifecycle rule.
///
/// Every field is sent, so clearing an expiration is expressed as an explicit
/// null rather than being indistinguishable from "leave this alone". The console
/// already holds the whole rule from the listing, so there is nothing to gain
/// from a partial update and a real ambiguity to avoid.
#[derive(Debug, Deserialize)]
pub(crate) struct UpdateLifecycleRuleRequest {
    prefix: String,
    enabled: bool,
    #[serde(default)]
    expiration: Option<ExpirationDays>,
    #[serde(default)]
    noncurrent_version_expiration: Option<ExpirationDays>,
}

/// Replaces one lifecycle rule belonging to a bucket.
///
/// The rule is addressed through its bucket so a rule identifier from one bucket
/// cannot be used to edit another's, and so the lookup stays bounded by that
/// bucket's rule count.
pub(crate) async fn update_lifecycle_rule(
    State(state): State<AppState>,
    Path((bucket, rule_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<UpdateLifecycleRuleRequest>,
) -> Result<Json<LifecycleRule>, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let rule_id = LifecycleRuleId::from_str(&rule_id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE_ID",
            "Invalid lifecycle rule ID",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    let existing = state
        .metadata
        .list_lifecycle_rules(Some(bucket.id))
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule listing failed");
            ApiError::internal(request_id.clone())
        })?
        .into_iter()
        .find(|rule| rule.id == rule_id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "LIFECYCLE_RULE_NOT_FOUND",
                "Lifecycle rule was not found in this bucket",
                request_id.clone(),
            )
        })?;

    let updated = LifecycleRule {
        id: existing.id,
        bucket_id: existing.bucket_id,
        prefix: input.prefix,
        enabled: input.enabled,
        expiration: input.expiration,
        noncurrent_version_expiration: input.noncurrent_version_expiration,
        created_at: existing.created_at,
        updated_at: chrono::Utc::now(),
    };
    updated.validate().map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE",
            "Lifecycle rule is invalid",
        )
    })?;
    state
        .metadata
        .put_lifecycle_rule(&updated)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule update failed");
            ApiError::internal(request_id)
        })?;
    Ok(Json(updated))
}

pub(crate) async fn delete_lifecycle_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = LifecycleRuleId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE_ID",
            "Invalid lifecycle rule ID",
        )
    })?;
    state
        .metadata
        .delete_lifecycle_rule(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule deletion failed");
            ApiError::bad_request(
                request_id,
                "LIFECYCLE_RULE_NOT_FOUND",
                "Lifecycle rule was not found",
            )
        })
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{admin, api, call, expect_status};

    async fn bucket(router: &axum::Router) {
        expect_status(
            router,
            admin("POST", "/api/v1/buckets", Some(json!({"name": "photos"}))),
            StatusCode::CREATED,
        )
        .await;
    }

    #[tokio::test]
    async fn a_rule_is_listed_after_creation_and_gone_after_deletion() {
        let (_directory, api) = api().await;
        bucket(&api).await;

        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets/photos/lifecycle",
                Some(json!({"prefix": "logs/", "expiration": 30})),
            ),
            StatusCode::CREATED,
        )
        .await;
        let id = created["id"].as_str().expect("rule id").to_owned();

        let listed = expect_status(
            &api,
            admin("GET", "/api/v1/buckets/photos/lifecycle", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(listed.as_array().expect("array").len(), 1, "{listed}");

        expect_status(
            &api,
            admin("DELETE", &format!("/api/v1/lifecycle-rules/{id}"), None),
            StatusCode::NO_CONTENT,
        )
        .await;
        let empty = expect_status(
            &api,
            admin("GET", "/api/v1/buckets/photos/lifecycle", None),
            StatusCode::OK,
        )
        .await;
        assert!(empty.as_array().expect("array").is_empty(), "{empty}");
    }

    /// A rule that expires nothing would be silently inert. Requiring at least
    /// one expiration is what stops an operator believing data is being cleaned
    /// up when nothing is.
    #[tokio::test]
    async fn a_rule_that_expires_nothing_is_refused() {
        let (_directory, api) = api().await;
        bucket(&api).await;

        let response = call(
            &api,
            admin(
                "POST",
                "/api/v1/buckets/photos/lifecycle",
                Some(json!({"prefix": "logs/"})),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_rule_can_be_updated_in_place() {
        let (_directory, api) = api().await;
        bucket(&api).await;
        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets/photos/lifecycle",
                Some(json!({"prefix": "logs/", "expiration": 30})),
            ),
            StatusCode::CREATED,
        )
        .await;
        let id = created["id"].as_str().expect("rule id").to_owned();

        let updated = expect_status(
            &api,
            admin(
                "PUT",
                &format!("/api/v1/buckets/photos/lifecycle/{id}"),
                Some(json!({"prefix": "logs/", "expiration": 7, "enabled": false})),
            ),
            StatusCode::OK,
        )
        .await;
        assert_eq!(updated["expiration"], 7, "{updated}");
        assert_eq!(updated["enabled"], false, "{updated}");
    }

    #[tokio::test]
    async fn lifecycle_rules_on_an_unknown_bucket_are_not_found() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("GET", "/api/v1/buckets/absent/lifecycle", None),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    /// Deleting an absent rule reports `LIFECYCLE_RULE_NOT_FOUND`.
    ///
    /// The status is `400` rather than `404`, which is pinned here as the
    /// behaviour clients see today. The stable error code is what a caller
    /// should branch on; the status is worth revisiting, because this arm also
    /// catches genuine catalog failures and reports them as the caller's fault.
    #[tokio::test]
    async fn deleting_a_rule_that_does_not_exist_reports_a_stable_error_code() {
        let (_directory, api) = api().await;
        bucket(&api).await;
        let body = expect_status(
            &api,
            admin(
                "DELETE",
                "/api/v1/lifecycle-rules/0195f0c8-0000-7000-8000-0000000000ff",
                None,
            ),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(body["error"]["code"], "LIFECYCLE_RULE_NOT_FOUND", "{body}");
    }

    #[tokio::test]
    async fn a_malformed_rule_identifier_is_refused() {
        let (_directory, api) = api().await;
        let body = expect_status(
            &api,
            admin("DELETE", "/api/v1/lifecycle-rules/not-a-uuid", None),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(body["error"]["code"], "INVALID_LIFECYCLE_RULE_ID", "{body}");
    }
}
