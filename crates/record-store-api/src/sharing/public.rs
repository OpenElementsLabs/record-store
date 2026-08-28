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
    extract::{Extension, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use record_store_core::PreviewKind;
use record_store_sharing::{
    CapabilityToken, EmbedDisposition, RateDecision, ShareLookup, UnlockFailure,
};

use crate::dto::RequestId;
use crate::error::ApiError;
use crate::handlers::objects::insert_header;
use crate::{AppState, ClientIdentity};

use crate::sharing::dto::{PublicShareResponse, ShareContentQuery, UnlockBody, UnlockResponse};
use crate::sharing::respond::{
    Disposition, apply_cors, apply_embed_headers, byte_response, open_stream, read_metadata,
    verify_signature,
};
use crate::sharing::support::{
    denial_response, describe_share, embed_unavailable, private_json, record_public_denial,
    refusal_label, require_sharing, share_unavailable, sharing_to_api_error, throttled_response,
};

/// Describes a share to its recipient, or asks for the password first.
pub(crate) async fn public_share_descriptor(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: header::HeaderMap,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    // Proof of an earlier unlock, if the visitor has one. It is what turns the
    // challenge below into the file they were sent.
    let ticket = headers
        .get("x-record-store-share-ticket")
        .and_then(|value| value.to_str().ok());
    let Some(token) = CapabilityToken::parse(&token) else {
        // A malformed token never reaches the store, but it is still a guess.
        if let RateDecision::Throttled {
            retry_after_seconds,
        } = sharing.service().probe_allowance(client)
        {
            return Ok(throttled_response(retry_after_seconds, &request_id));
        }
        state.sharing_metrics.share_denied();
        return Err(share_unavailable(request_id));
    };
    let now = Utc::now();
    let lookup = sharing
        .service()
        .look_up_share(&token, ticket, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    match lookup {
        ShareLookup::Unavailable(refusal) => {
            record_public_denial(
                &state,
                &request_id,
                "share.denied",
                None,
                refusal_label(refusal),
            )
            .await;
            state.sharing_metrics.share_denied();
            Err(share_unavailable(request_id))
        }
        ShareLookup::PasswordRequired(_) => Ok(private_json(Json(PublicShareResponse::locked()))),
        ShareLookup::Open(link) => {
            let descriptor = describe_share(&state, &link, sharing, &request_id).await?;
            Ok(private_json(Json(descriptor)))
        }
    }
}

/// Verifies a share password and hands back a short-lived unlock proof.
pub(crate) async fn public_share_unlock(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
    Json(body): Json<UnlockBody>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    let Some(token) = CapabilityToken::parse(&token) else {
        state.sharing_metrics.share_denied();
        return Err(share_unavailable(request_id));
    };
    let now = Utc::now();
    let outcome = sharing
        .service()
        .unlock_share(&token, &body.password, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    match outcome {
        Ok(ticket) => {
            let seconds = sharing.service().policy().unlock_lifetime.num_seconds();
            Ok(private_json(Json(UnlockResponse {
                ticket: ticket.into_string(),
                expires_in_seconds: seconds,
            })))
        }
        Err(UnlockFailure::Throttled {
            retry_after_seconds,
        }) => {
            record_public_denial(
                &state,
                &request_id,
                "share.password_throttled",
                None,
                "throttled",
            )
            .await;
            state.sharing_metrics.share_denied();
            Ok(throttled_response(retry_after_seconds, &request_id))
        }
        Err(UnlockFailure::IncorrectPassword) => {
            record_public_denial(
                &state,
                &request_id,
                "share.password_failed",
                None,
                "incorrect_password",
            )
            .await;
            state.sharing_metrics.share_denied();
            Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "SHARE_PASSWORD_INCORRECT",
                "That password is not correct",
                request_id,
            ))
        }
        Err(UnlockFailure::NotPasswordProtected | UnlockFailure::Unavailable(_)) => {
            state.sharing_metrics.share_denied();
            Err(share_unavailable(request_id))
        }
    }
}

/// Streams the bytes behind a share link.
pub(crate) async fn public_share_content(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<ShareContentQuery>,
    headers: header::HeaderMap,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    let Some(token) = CapabilityToken::parse(&token) else {
        state.sharing_metrics.share_denied();
        return Err(share_unavailable(request_id));
    };
    let ticket = headers
        .get("x-record-store-share-ticket")
        .and_then(|value| value.to_str().ok());
    let now = Utc::now();
    let authorized = sharing
        .service()
        .authorize_share_access(&token, ticket, query.download, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let link = match authorized {
        Ok(link) => link,
        Err(denial) => {
            state.sharing_metrics.share_denied();
            return Ok(denial_response(&state, &request_id, "share.denied", denial).await);
        }
    };
    let metadata = read_metadata(
        &state,
        &link.target.bucket,
        &link.target.key,
        link.target.version,
        &request_id,
    )
    .await?;
    let kind = PreviewKind::classify(metadata.content_type.as_deref());

    // A share with a strict access budget ignores byte ranges and always serves
    // the whole object. That is what makes "five downloads" mean five: if a
    // client could take the file one range at a time, the counter would measure
    // requests rather than deliveries and the limit would be decorative.
    let budgeted = link.maximum_access_count.is_some();
    let range = if budgeted {
        None
    } else {
        crate::dto::parse_preview_range(&headers, metadata.size, &request_id)?
    };

    let disposition = if query.download {
        Disposition::Attachment
    } else {
        if !kind.allows_inline() {
            state.sharing_metrics.share_denied();
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "SHARE_PREVIEW_UNSUPPORTED",
                "This object cannot be shown safely in a browser. Download it instead.",
                request_id,
            ));
        }
        verify_signature(&state, &link.target, &metadata, &request_id).await?;
        Disposition::Inline
    };

    let result = open_stream(&state, &link.target, range, &request_id).await?;
    state.sharing_metrics.share_access();
    let mut response = byte_response(result, &metadata, disposition, !budgeted);
    let response_headers = response.headers_mut();
    // A share is revocable, so nothing between Record Store and the recipient may keep a
    // copy that outlives the revocation.
    insert_header(
        response_headers,
        header::CACHE_CONTROL,
        "private, no-store, max-age=0",
    );
    insert_header(
        response_headers,
        header::CONTENT_SECURITY_POLICY,
        SHARE_CONTENT_POLICY,
    );
    Ok(response)
}

/// The policy carried by share and preview bytes.
///
/// `sandbox` drops the response into an opaque origin, so a PDF viewer still
/// renders while anything the document tries to do — script, navigation, form
/// submission — has no origin to do it to. `frame-ancestors 'self'` lets the
/// console and the share page frame the viewer and stops any other site from
/// doing so.
pub(crate) const SHARE_CONTENT_POLICY: &str = "sandbox allow-downloads; default-src 'none'; frame-ancestors 'self'; base-uri 'none'; \
     form-action 'none'";

/// The policy carried by embed bytes.
///
/// Embeds are loaded by `<img>`, `<video>`, and `<audio>` on other people's
/// pages, where framing is not the risk. The bytes still get an opaque origin so
/// that a direct navigation to an embed URL cannot execute anything.
pub(crate) const EMBED_CONTENT_POLICY: &str =
    "sandbox; default-src 'none'; base-uri 'none'; form-action 'none'";

/// Streams the bytes behind an embed link.
pub(crate) async fn public_embed_content(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: header::HeaderMap,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    let presented_origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let Some(token) = CapabilityToken::parse(&token) else {
        state.sharing_metrics.embed_denied();
        return Err(embed_unavailable(request_id));
    };
    let now = Utc::now();
    let authorized = sharing
        .service()
        .authorize_embed_access(&token, presented_origin, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let (link, decision) = match authorized {
        Ok(granted) => granted,
        Err(denial) => {
            state.sharing_metrics.embed_denied();
            return Ok(denial_response(&state, &request_id, "embed.denied", denial).await);
        }
    };
    let metadata = read_metadata(
        &state,
        &link.target.bucket,
        &link.target.key,
        link.target.version,
        &request_id,
    )
    .await?;
    let kind = PreviewKind::classify(metadata.content_type.as_deref());

    let disposition = match link.disposition {
        EmbedDisposition::Attachment => Disposition::Attachment,
        EmbedDisposition::Inline => {
            // An embed that follows the current version can find that the
            // object has been replaced by something that must not be rendered.
            // The check that was made at creation is therefore made again here,
            // against the version actually about to be served.
            let current = PreviewKind::canonical_content_type(
                metadata.content_type.as_deref().unwrap_or_default(),
            );
            if !kind.allows_inline() || current != Some(link.content_type.as_str()) {
                state.sharing_metrics.embed_denied();
                record_public_denial(
                    &state,
                    &request_id,
                    "embed.denied",
                    Some(&link.target),
                    "content_type_changed",
                )
                .await;
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "EMBED_CONTENT_CHANGED",
                    "This object is no longer the media type this embed was created for",
                    request_id,
                ));
            }
            verify_signature(&state, &link.target, &metadata, &request_id).await?;
            Disposition::Inline
        }
    };

    // An embed is an asset URL that a page reloads constantly, so honouring a
    // revalidation is worth the few lines: the alternative is resending an
    // unchanged image on every visit.
    let revalidated = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|presented| {
            presented
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate.trim_matches('"') == metadata.etag.as_str())
        });
    if revalidated {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        apply_embed_headers(
            response.headers_mut(),
            &link,
            decision,
            presented_origin,
            &metadata,
        );
        state.sharing_metrics.embed_request();
        return Ok(response);
    }

    let range = crate::dto::parse_preview_range(&headers, metadata.size, &request_id)?;
    let result = open_stream(&state, &link.target, range, &request_id).await?;
    state.sharing_metrics.embed_request();
    let mut response = byte_response(result, &metadata, disposition, true);
    apply_embed_headers(
        response.headers_mut(),
        &link,
        decision,
        presented_origin,
        &metadata,
    );
    Ok(response)
}

/// Answers a browser's CORS preflight for an embed.
pub(crate) async fn public_embed_preflight(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: header::HeaderMap,
    Extension(client): Extension<ClientIdentity>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let sharing = require_sharing(&state, &request_id)?;
    let client = client.as_str();
    let presented_origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let Some(token) = CapabilityToken::parse(&token) else {
        return Err(embed_unavailable(request_id));
    };
    let now = Utc::now();
    let authorized = sharing
        .service()
        .authorize_embed_access(&token, presented_origin, client, now)
        .await
        .map_err(|error| sharing_to_api_error(&error, request_id.clone()))?;
    let Ok((link, decision)) = authorized else {
        state.sharing_metrics.embed_denied();
        return Err(embed_unavailable(request_id));
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    let response_headers = response.headers_mut();
    apply_cors(response_headers, &link, decision, presented_origin);
    insert_header(
        response_headers,
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, HEAD, OPTIONS",
    );
    insert_header(
        response_headers,
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "range, if-none-match",
    );
    insert_header(response_headers, header::ACCESS_CONTROL_MAX_AGE, "600");
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_policies_deny_script_and_keep_stored_bytes_in_an_opaque_origin() {
        for policy in [SHARE_CONTENT_POLICY, EMBED_CONTENT_POLICY] {
            assert!(policy.contains("sandbox"), "{policy}");
            assert!(policy.contains("default-src 'none'"), "{policy}");
            assert!(!policy.contains("allow-scripts"), "{policy}");
            assert!(!policy.contains("allow-same-origin"), "{policy}");
            assert!(!policy.contains("unsafe-inline"), "{policy}");
        }
        // Only the share and preview surface is framed by Record Store itself.
        assert!(SHARE_CONTENT_POLICY.contains("frame-ancestors 'self'"));
    }
}
