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

use std::{net::SocketAddr, sync::Arc};

use axum::{extract::ConnectInfo, http::header};
use record_store_sharing::{CapabilityToken, SharingService};

/// The sharing dependencies an API instance needs.
#[derive(Clone)]
pub struct SharingManagement {
    pub(crate) service: Arc<SharingService>,
    pub(crate) share_base_url: Option<String>,
    pub(crate) embed_base_url: String,
    pub(crate) preview_text_limit_bytes: u64,
}

impl SharingManagement {
    /// Creates the management surface from a running sharing service.
    ///
    /// The two base addresses are separate because the two capabilities are
    /// published in different places. A share link is a page a person opens, so
    /// it lives on the console; an embed serves object bytes into somebody
    /// else's page, so it lives on the storage endpoint. Collapsing them would
    /// either route asset traffic through the administrative console or publish
    /// the console's address to every site that embeds an image.
    #[must_use]
    pub fn new(
        service: Arc<SharingService>,
        share_base_url: Option<String>,
        embed_base_url: String,
        preview_text_limit_bytes: u64,
    ) -> Self {
        Self {
            service,
            share_base_url,
            embed_base_url,
            preview_text_limit_bytes,
        }
    }

    /// Returns the capability service.
    #[must_use]
    pub fn service(&self) -> &SharingService {
        &self.service
    }

    /// Returns the configured preview slice size.
    #[must_use]
    pub const fn preview_text_limit_bytes(&self) -> u64 {
        self.preview_text_limit_bytes
    }

    /// Builds the URL a share recipient opens.
    ///
    /// Without a configured base this returns only the path. That is not a
    /// failure: the console knows its own public origin and completes the URL,
    /// and guessing an external address from a request header would be a way to
    /// hand out links pointing at somewhere Record Store was never deployed.
    pub(crate) fn share_url(&self, token: &CapabilityToken) -> String {
        match &self.share_base_url {
            Some(base) => format!("{base}/s/{}", token.expose()),
            None => format!("/s/{}", token.expose()),
        }
    }

    /// Builds the URL a website loads an embed from.
    ///
    /// Always absolute, because the browser that eventually resolves it is on a
    /// page Record Store has nothing to do with: there is no origin for it to fall back
    /// to. The address is the storage endpoint, resolved once at startup.
    pub(crate) fn embed_url(&self, token: &CapabilityToken) -> String {
        format!("{}/e/{}", self.embed_base_url, token.expose())
    }
}

/// Extracts the identity abuse controls are applied to.
///
/// `X-Forwarded-For` is honoured because public capability traffic reaches Record Store
/// through the console or a reverse proxy, and the socket address would
/// otherwise be that hop for every visitor in the world. The header is only
/// meaningful when the management listener is not itself internet-facing, which
/// is how Record Store is meant to be deployed; when it is absent the socket address is
/// used and the limits simply apply more coarsely. The value is bounded and
/// sanitised because it is attacker-influenced either way, and it is never used
/// for anything but partitioning a counter.
pub(crate) fn client_identity(
    headers: &header::HeaderMap,
    connect: Option<&ConnectInfo<SocketAddr>>,
) -> String {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".:[]-_".contains(&byte))
        });
    match forwarded {
        Some(value) => value.to_owned(),
        None => connect.map_or_else(|| "unknown".to_owned(), |info| info.0.ip().to_string()),
    }
}

// ---------------------------------------------------------------------------
// Management surface
// ---------------------------------------------------------------------------
