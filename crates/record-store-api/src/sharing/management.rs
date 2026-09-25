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
use record_store_core::TrustedProxies;
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

/// Extracts the identity abuse controls and audit records are applied to.
///
/// `X-Forwarded-For` is believed only when the request actually arrived from a
/// hop the operator named in `server.trusted_proxies`. Without that condition
/// the header is a value the caller chooses, and every per-client limit becomes
/// a limit on something the attacker can rotate at will — which is not a limit.
///
/// With no trusted hop configured the socket address is used, which is correct
/// for a direct deployment and coarse behind a proxy: every visitor shares the
/// proxy's identity until the operator names it.
pub(crate) fn client_identity(
    trusted: &TrustedProxies,
    headers: &header::HeaderMap,
    connect: Option<&ConnectInfo<SocketAddr>>,
) -> String {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    trusted
        .client_address(connect.map(|info| info.0.ip()), forwarded)
        .map_or_else(|| "unknown".to_owned(), |address| address.to_string())
}

// ---------------------------------------------------------------------------
// Management surface
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, header};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::*;

    fn connect(address: &str) -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::new(
            address.parse::<IpAddr>().expect("address"),
            51_234,
        ))
    }

    fn forwarded(value: &str) -> header::HeaderMap {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_str(value).expect("header"),
        );
        headers
    }

    /// The default refuses to let a caller name itself. Without this, a single
    /// attacker rotates the header and every per-client limit on the public
    /// surface stops applying to them.
    #[test]
    fn a_forwarded_header_is_ignored_until_a_proxy_is_trusted() {
        let untrusted = TrustedProxies::default();
        assert_eq!(
            client_identity(
                &untrusted,
                &forwarded("203.0.113.7"),
                Some(&connect("198.51.100.4"))
            ),
            "198.51.100.4"
        );
    }

    #[test]
    fn a_forwarded_header_from_a_trusted_proxy_names_the_visitor() {
        let trusted = TrustedProxies::parse(&["10.0.0.0/8"]).expect("policy");
        assert_eq!(
            client_identity(
                &trusted,
                &forwarded("203.0.113.7, 10.0.0.9"),
                Some(&connect("10.0.0.9"))
            ),
            "203.0.113.7"
        );
    }

    /// The same header arriving directly, not through the proxy, must not work.
    #[test]
    fn the_same_header_from_somewhere_else_is_still_ignored() {
        let trusted = TrustedProxies::parse(&["10.0.0.0/8"]).expect("policy");
        assert_eq!(
            client_identity(
                &trusted,
                &forwarded("203.0.113.7"),
                Some(&connect("198.51.100.4"))
            ),
            "198.51.100.4"
        );
    }

    #[test]
    fn a_request_with_no_socket_address_is_attributed_to_nobody() {
        assert_eq!(
            client_identity(&TrustedProxies::default(), &header::HeaderMap::new(), None),
            "unknown"
        );
        let _ = Ipv4Addr::LOCALHOST;
    }
}
