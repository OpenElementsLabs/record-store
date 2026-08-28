use std::{str::FromStr, sync::Arc};

use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use record_store_core::WebhookId;
use record_store_events::{
    CreateWebhookRequest, CreatedWebhook, EventRepository, WebhookDeliveryLog, WebhookSubscription,
};
use serde::Deserialize;
use tracing::error;

use crate::error::ApiError;
use crate::*;

pub(crate) async fn create_webhook(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<CreatedWebhook>), ApiError> {
    event_repository(&state, &request_id)?
        .create_webhook(input)
        .await
        .map(|created| (StatusCode::CREATED, Json(created)))
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook creation failed");
            ApiError::bad_request(
                request_id,
                "INVALID_WEBHOOK",
                "Webhook configuration is invalid or disallowed",
            )
        })
}

pub(crate) async fn list_webhooks(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<WebhookSubscription>>, ApiError> {
    event_repository(&state, &request_id)?
        .list_webhooks()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook listing failed");
            ApiError::internal(request_id)
        })
}

#[derive(Debug, Deserialize)]
pub(crate) struct WebhookStatusRequest {
    enabled: bool,
}

pub(crate) async fn set_webhook_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<WebhookStatusRequest>,
) -> Result<Json<WebhookSubscription>, ApiError> {
    let id = WebhookId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_WEBHOOK_ID",
            "Invalid webhook ID",
        )
    })?;
    event_repository(&state, &request_id)?
        .set_webhook_enabled(id, input.enabled)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook status update failed");
            ApiError::bad_request(request_id, "WEBHOOK_NOT_FOUND", "Webhook was not found")
        })
}

pub(crate) async fn delete_webhook(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = WebhookId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_WEBHOOK_ID",
            "Invalid webhook ID",
        )
    })?;
    event_repository(&state, &request_id)?
        .delete_webhook(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook deletion failed");
            ApiError::bad_request(request_id, "WEBHOOK_NOT_FOUND", "Webhook was not found")
        })
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeliveryLogQuery {
    #[serde(default = "default_delivery_limit")]
    limit: usize,
}

pub(crate) const fn default_delivery_limit() -> usize {
    100
}

pub(crate) async fn list_webhook_deliveries(
    State(state): State<AppState>,
    Query(query): Query<DeliveryLogQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<WebhookDeliveryLog>>, ApiError> {
    event_repository(&state, &request_id)?
        .list_delivery_logs(query.limit)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook delivery log query failed");
            ApiError::bad_request(
                request_id,
                "INVALID_DELIVERY_QUERY",
                "Delivery query is invalid",
            )
        })
}

pub(crate) fn event_repository<'a>(
    state: &'a AppState,
    request_id: &RequestId,
) -> Result<&'a Arc<dyn EventRepository>, ApiError> {
    state
        .events
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable(request_id.clone()))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{admin, api, call, expect_status};

    fn subscription(url: &str) -> serde_json::Value {
        json!({
            "target_url": url,
            "event_types": ["object.created", "object.deleted"],
        })
    }

    #[tokio::test]
    async fn a_webhook_is_listed_after_creation_and_gone_after_deletion() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/webhooks",
                Some(subscription("http://127.0.0.1:9/events")),
            ),
            StatusCode::CREATED,
        )
        .await;
        let id = created["subscription"]["id"]
            .as_str()
            .or_else(|| created["id"].as_str())
            .expect("webhook id")
            .to_owned();

        let listed =
            expect_status(&api, admin("GET", "/api/v1/webhooks", None), StatusCode::OK).await;
        assert_eq!(listed.as_array().expect("array").len(), 1, "{listed}");

        expect_status(
            &api,
            admin("DELETE", &format!("/api/v1/webhooks/{id}"), None),
            StatusCode::NO_CONTENT,
        )
        .await;
        let empty =
            expect_status(&api, admin("GET", "/api/v1/webhooks", None), StatusCode::OK).await;
        assert!(empty.as_array().expect("array").is_empty(), "{empty}");
    }

    /// A webhook posts Record Store's own events to a third party, so a target
    /// that is not a usable HTTPS endpoint has to be refused at creation rather
    /// than failing silently on every later delivery.
    #[tokio::test]
    async fn an_unusable_target_url_is_refused_at_creation() {
        let (_directory, api) = api().await;
        for target in [
            "",
            "not-a-url",
            "ftp://127.0.0.1/x",
            "https://user:pw@127.0.0.1/x",
        ] {
            let response = call(
                &api,
                admin("POST", "/api/v1/webhooks", Some(subscription(target))),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "accepted unusable target {target:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_webhook_can_be_switched_off_without_being_deleted() {
        let (_directory, api) = api().await;
        let created = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/webhooks",
                Some(subscription("http://127.0.0.1:9/events")),
            ),
            StatusCode::CREATED,
        )
        .await;
        let id = created["subscription"]["id"]
            .as_str()
            .or_else(|| created["id"].as_str())
            .expect("webhook id")
            .to_owned();

        let disabled = expect_status(
            &api,
            admin(
                "PUT",
                &format!("/api/v1/webhooks/{id}/status"),
                Some(json!({"enabled": false})),
            ),
            StatusCode::OK,
        )
        .await;
        assert_eq!(disabled["enabled"], false, "{disabled}");

        let listed =
            expect_status(&api, admin("GET", "/api/v1/webhooks", None), StatusCode::OK).await;
        assert_eq!(
            listed.as_array().expect("array").len(),
            1,
            "disabling must not delete the subscription: {listed}"
        );
    }

    #[tokio::test]
    async fn the_delivery_log_is_readable_and_starts_empty() {
        let (_directory, api) = api().await;
        let log = expect_status(
            &api,
            admin("GET", "/api/v1/webhook-deliveries", None),
            StatusCode::OK,
        )
        .await;
        assert!(log.as_array().expect("array").is_empty(), "{log}");
    }

    #[tokio::test]
    async fn an_unknown_webhook_identifier_is_rejected() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("DELETE", "/api/v1/webhooks/not-a-uuid", None),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
}
