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

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};

    use crate::test_support::{admin, api, call, expect_status};

    fn anonymous(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("request")
    }

    /// Orchestrators poll these before any credential exists, so they must stay
    /// reachable without one.
    #[tokio::test]
    async fn the_health_and_readiness_probes_need_no_credential() {
        let (_directory, api) = api().await;
        for uri in ["/health", "/ready"] {
            let response = call(&api, anonymous(uri)).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
    }

    #[tokio::test]
    async fn system_info_reports_the_deployment_mode_and_capabilities() {
        let (_directory, api) = api().await;
        let info = expect_status(
            &api,
            admin("GET", "/api/v1/system/info", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(info["mode"], "standalone", "{info}");
        assert!(info["version"].as_str().is_some(), "{info}");
        assert!(info["capabilities"].is_object(), "{info}");
    }

    /// Capabilities describe what this deployment actually wired up. A feature
    /// reported as available but not configured would send the console down a
    /// path that refuses every call.
    #[tokio::test]
    async fn capabilities_distinguish_what_is_wired_from_what_is_not() {
        let (_directory, api) = api().await;
        let info = expect_status(
            &api,
            admin("GET", "/api/v1/system/info", None),
            StatusCode::OK,
        )
        .await;
        let capabilities = &info["capabilities"];

        assert_eq!(
            capabilities["cluster"], false,
            "this fixture is standalone: {info}"
        );
        assert_eq!(
            capabilities["erasure_coding"], false,
            "replication is the durability model: {info}"
        );
        for wired in [
            "versioning",
            "webhooks",
            "events",
            "lifecycle",
            "object_browser",
        ] {
            assert_eq!(
                capabilities[wired], true,
                "{wired} is wired in this fixture: {info}"
            );
        }
    }

    #[tokio::test]
    async fn an_unrouted_path_reports_a_stable_error_code() {
        let (_directory, api) = api().await;
        let body = expect_status(
            &api,
            admin("GET", "/api/v1/nothing-here", None),
            StatusCode::NOT_FOUND,
        )
        .await;
        assert_eq!(body["error"]["code"], "ROUTE_NOT_FOUND", "{body}");
    }
}
