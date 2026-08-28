//! Management and public HTTP surfaces for share and embed capabilities.
//!
//! Two surfaces live here and they are deliberately kept apart. The management
//! routes sit under `/api/v1`, behind the same bearer authentication as every
//! other administrative operation, and are where capabilities are created,
//! inspected, and withdrawn. The public routes — `/s/{token}` and `/e/{token}` —
//! carry no session at all: the token in the path *is* the authorization, and it
//! is re-checked against durable state on every single request so that a
//! revocation takes effect on the next one.
//!
//! Nothing on the public surface can reach anything but the one object its
//! capability names, and nothing on it discloses a bucket, a key path, a version
//! identifier, a node, or any other internal fact about how Record Store stores things.

use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use chrono::Utc;
use record_store_audit::AuditResult;
use record_store_sharing::CreateShareRequest;

use crate::AppState;
use crate::dto::RequestId;
use crate::error::ApiError;

use crate::sharing::dto::{
    CapabilityUrlResponse, CreateShareBody, IssuedShareResponse, ShareResponse,
};
use crate::sharing::respond::read_metadata;
use crate::sharing::support::{
    describe_content_type, parse_share_id, record_capability_audit, require_sharing,
    resolve_target, share_not_found, sharing_to_api_error, version_mode,
};

pub(crate) async fn list_object_shares(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<ShareResponse>>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let (bucket_id, _, key) = resolve_target(&state, &bucket, &key, &request_id).await?;
    let now = Utc::now();
    sharing
        .service()
        .store()
        .list_shares_for_object(bucket_id, &key)
        .await
        .map(|links| {
            Json(
                links
                    .iter()
                    .map(|link| ShareResponse::of(link, now))
                    .collect(),
            )
        })
        .map_err(|error| sharing_to_api_error(&error, request_id))
}

pub(crate) async fn create_object_share(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
    Json(body): Json<CreateShareBody>,
) -> Result<(StatusCode, Json<IssuedShareResponse>), ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let (bucket_id, bucket_name, object_key) =
        resolve_target(&state, &bucket, &key, &request_id).await?;
    let version = version_mode(body.version_id);
    // The target must exist before a capability is minted for it, and the exact
    // version has to be the one that was asked for: a share pinned to a version
    // that never existed is a link that fails only once someone opens it.
    let metadata = read_metadata(&state, &bucket_name, &object_key, version, &request_id).await?;
    let now = Utc::now();
    let issued = sharing
        .service()
        .create_share(
            CreateShareRequest {
                label: body.label,
                bucket_id,
                bucket: bucket_name,
                key: object_key,
                version,
                permission: body.permission,
                expires_at: body.expires_at,
                password: body.password,
                maximum_access_count: body.maximum_access_count,
                created_by: principal.audit_name().to_owned(),
            },
            now,
        )
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let url = sharing.share_url(&issued.token);
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "share.created",
        &issued.link.target,
        AuditResult::Success,
        [
            ("share_id", issued.link.id.to_string()),
            ("permission", issued.link.permission.label().to_owned()),
            ("version_mode", version.label().to_owned()),
            (
                "expires_at",
                issued
                    .link
                    .expires_at
                    .map_or_else(|| "never".to_owned(), |at| at.to_rfc3339()),
            ),
            (
                "password_protected",
                issued.link.password_protected().to_string(),
            ),
            ("content_type", describe_content_type(&metadata)),
        ],
    )
    .await;
    state.sharing_metrics.shares_created();
    Ok((
        StatusCode::CREATED,
        Json(IssuedShareResponse {
            share: ShareResponse::of(&issued.link, now),
            url,
        }),
    ))
}

pub(crate) async fn get_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ShareResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_share_id(&id, &request_id)?;
    let link = sharing
        .service()
        .store()
        .get_share(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| share_not_found(request_id))?;
    Ok(Json(ShareResponse::of(&link, Utc::now())))
}

pub(crate) async fn get_share_url(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<CapabilityUrlResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_share_id(&id, &request_id)?;
    if sharing
        .service()
        .store()
        .get_share(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .is_none()
    {
        return Err(share_not_found(request_id));
    }
    let token = sharing
        .service()
        .store()
        .reveal_share_token(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id))?;
    Ok(Json(CapabilityUrlResponse {
        url: token.as_ref().map(|token| sharing.share_url(token)),
        available: token.is_some(),
    }))
}

pub(crate) async fn revoke_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
) -> Result<Json<ShareResponse>, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_share_id(&id, &request_id)?;
    let now = Utc::now();
    let link = sharing
        .service()
        .revoke_share(id, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| share_not_found(request_id.clone()))?;
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "share.revoked",
        &link.target,
        AuditResult::Success,
        [
            ("share_id", link.id.to_string()),
            ("access_count", link.access_count.to_string()),
        ],
    )
    .await;
    Ok(Json(ShareResponse::of(&link, now)))
}

pub(crate) async fn delete_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<crate::ManagementPrincipal>,
) -> Result<StatusCode, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let id = parse_share_id(&id, &request_id)?;
    let now = Utc::now();
    let link = sharing
        .service()
        .store()
        .get_share(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?
        .ok_or_else(|| share_not_found(request_id.clone()))?;
    // Deleting the record deletes the evidence that the link existed, so it is
    // only offered once the link is already inert. An operator who wants a live
    // share gone revokes it, which is authoritative immediately; tidying the
    // history afterwards is a separate, weaker action.
    if link.status(now).usable() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "SHARE_STILL_ACTIVE",
            "Revoke this share before deleting its record",
            request_id,
        ));
    }
    sharing
        .service()
        .store()
        .delete_share(id)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    record_capability_audit(
        &state,
        &request_id,
        principal,
        "share.deleted",
        &link.target,
        AuditResult::Success,
        [("share_id", link.id.to_string())],
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{admin, api, call, expect_status, make_bucket, put_object};

    async fn shared(router: &axum::Router) -> serde_json::Value {
        make_bucket(router, "photos").await;
        put_object(router, "photos", "report.pdf", b"contents").await;
        expect_status(
            router,
            admin(
                "POST",
                "/api/v1/buckets/photos/object-shares/report.pdf",
                Some(json!({"label": "Board review"})),
            ),
            StatusCode::CREATED,
        )
        .await
    }

    /// A share's URL is disclosed once at creation. The management view must
    /// never carry the token again, or a screenshot of the console would leak
    /// the capability.
    #[tokio::test]
    async fn creating_a_share_returns_its_url_but_later_reads_never_do() {
        let (_directory, api) = api().await;
        let created = shared(&api).await;
        let id = created["share"]["id"]
            .as_str()
            .expect("share id")
            .to_owned();
        let url = created["url"].as_str().expect("url").to_owned();
        assert!(url.starts_with("https://share.example"), "{url}");

        let fetched = expect_status(
            &api,
            admin("GET", &format!("/api/v1/shares/{id}"), None),
            StatusCode::OK,
        )
        .await;
        assert!(
            !fetched.to_string().contains("https://share.example/"),
            "a later read must not carry the token: {fetched}"
        );
    }

    /// Copying the link again is a deliberate management action and must return
    /// the same URL the recipient already holds.
    #[tokio::test]
    async fn the_url_can_be_requested_again_on_its_own_route() {
        let (_directory, api) = api().await;
        let created = shared(&api).await;
        let id = created["share"]["id"]
            .as_str()
            .expect("share id")
            .to_owned();

        let again = expect_status(
            &api,
            admin("GET", &format!("/api/v1/shares/{id}/url"), None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(again["url"], created["url"], "{again}");
    }

    #[tokio::test]
    async fn shares_are_listed_against_the_object_they_target() {
        let (_directory, api) = api().await;
        shared(&api).await;
        put_object(&api, "photos", "other.pdf", b"other").await;

        let listed = expect_status(
            &api,
            admin(
                "GET",
                "/api/v1/buckets/photos/object-shares/report.pdf",
                None,
            ),
            StatusCode::OK,
        )
        .await;
        assert_eq!(listed.as_array().expect("array").len(), 1, "{listed}");

        let elsewhere = expect_status(
            &api,
            admin(
                "GET",
                "/api/v1/buckets/photos/object-shares/other.pdf",
                None,
            ),
            StatusCode::OK,
        )
        .await;
        assert!(
            elsewhere.as_array().expect("array").is_empty(),
            "{elsewhere}"
        );
    }

    /// Revocation withdraws the capability but keeps the record; deletion
    /// removes it entirely. An operator needs both, and they are not the same.
    #[tokio::test]
    async fn a_share_can_be_revoked_and_then_deleted() {
        let (_directory, api) = api().await;
        let created = shared(&api).await;
        let id = created["share"]["id"]
            .as_str()
            .expect("share id")
            .to_owned();

        expect_status(
            &api,
            admin("POST", &format!("/api/v1/shares/{id}/revoke"), None),
            StatusCode::OK,
        )
        .await;
        let revoked = expect_status(
            &api,
            admin("GET", &format!("/api/v1/shares/{id}"), None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(revoked["status"], "revoked", "{revoked}");

        expect_status(
            &api,
            admin("DELETE", &format!("/api/v1/shares/{id}"), None),
            StatusCode::NO_CONTENT,
        )
        .await;
        expect_status(
            &api,
            admin("GET", &format!("/api/v1/shares/{id}"), None),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    #[tokio::test]
    async fn a_share_for_an_object_that_does_not_exist_is_refused() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/buckets/photos/object-shares/absent.pdf",
                Some(json!({"label": "Nothing"})),
            ),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    /// The label is operator-supplied text that appears in the console, so it is
    /// validated rather than stored as typed.
    #[tokio::test]
    async fn an_unusable_label_is_refused() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        put_object(&api, "photos", "report.pdf", b"contents").await;

        for label in ["", "   "] {
            let response = call(
                &api,
                admin(
                    "POST",
                    "/api/v1/buckets/photos/object-shares/report.pdf",
                    Some(json!({"label": label})),
                ),
            )
            .await;
            assert!(
                response.status().is_client_error(),
                "accepted label {label:?}: {}",
                response.status()
            );
        }
    }

    #[tokio::test]
    async fn a_malformed_share_identifier_is_refused() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            admin("GET", "/api/v1/shares/not-a-uuid", None),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
}
