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
