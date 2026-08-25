//! Validated web origins for embed restrictions.
//!
//! An origin allowlist is a useful narrowing, not a security boundary: `Origin`
//! is a browser-supplied header and a non-browser client simply omits it. The
//! unguessable, revocable token remains the capability. What the allowlist does
//! buy is real: it stops a leaked embed URL from rendering on someone else's
//! site, and it keeps CORS grants deliberate instead of wildcarded.

use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::SharingError;

/// The most origins one embed may list, so a record stays bounded.
pub const MAXIMUM_ALLOWED_ORIGINS: usize = 20;

/// A normalized `scheme://host[:port]` origin.
///
/// Stored normalized rather than as typed, because comparison against a
/// browser's `Origin` header is exact-match: `https://Example.com:443` and
/// `https://example.com` are the same origin, and only one of them would ever
/// arrive in a header.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AllowedOrigin(String);

impl AllowedOrigin {
    /// Parses and normalizes one operator-supplied origin.
    ///
    /// Only `http` and `https` are accepted. Schemes such as `javascript:`,
    /// `data:`, and `file:` are not merely unusual here — they are the exact
    /// values an attacker would supply to turn an allowlist entry into a
    /// reflected script or a same-origin-with-everything grant.
    pub fn parse(value: &str) -> Result<Self, SharingError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(invalid(value, "an origin must not be empty"));
        }
        if trimmed.len() > 255 {
            return Err(invalid(value, "an origin must be at most 255 characters"));
        }
        if trimmed
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
        {
            return Err(invalid(
                value,
                "an origin must not contain control characters",
            ));
        }
        let Some((scheme, remainder)) = trimmed.split_once("://") else {
            return Err(invalid(
                value,
                "an origin must be written as https://host or http://host",
            ));
        };
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && scheme != "http" {
            return Err(invalid(value, "only http and https origins are supported"));
        }
        // An origin is a scheme, host, and port. Anything after them — a path,
        // a query, a fragment, or credentials — means the operator has supplied
        // a URL and expects path-level behaviour OES will not provide.
        if remainder.contains('/')
            || remainder.contains('?')
            || remainder.contains('#')
            || remainder.contains('@')
            || remainder.contains('\\')
        {
            return Err(invalid(
                value,
                "an origin must carry no path, query, fragment, or credentials",
            ));
        }
        if remainder.is_empty() {
            return Err(invalid(value, "an origin must name a host"));
        }
        let (host, port) = split_host_and_port(remainder)
            .ok_or_else(|| invalid(value, "the origin's port is not a number"))?;
        validate_host(&host).map_err(|reason| invalid(value, &reason))?;
        let normalized = match port {
            Some(port) if !is_default_port(&scheme, port) => {
                format!("{scheme}://{host}:{port}")
            }
            _ => format!("{scheme}://{host}"),
        };
        Ok(Self(normalized))
    }

    /// Returns the normalized origin text, safe to place in a CORS header.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns whether a browser-supplied `Origin` header matches this entry.
    ///
    /// The candidate is normalized the same way before comparison so that a
    /// browser sending an explicit default port still matches.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        Self::parse(candidate).is_ok_and(|parsed| parsed == *self)
    }
}

impl Display for AllowedOrigin {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn invalid(value: &str, reason: &str) -> SharingError {
    // The operator's own input is echoed back because they need to see which
    // entry was rejected; it is bounded above and control characters are
    // refused before this point.
    SharingError::InvalidOrigin(format!("{value:?}: {reason}"))
}

/// Splits `host[:port]`, handling bracketed IPv6 literals.
fn split_host_and_port(value: &str) -> Option<(String, Option<u16>)> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, remainder) = rest.split_once(']')?;
        let host = format!("[{}]", host.to_ascii_lowercase());
        return match remainder {
            "" => Some((host, None)),
            other => {
                let port = other.strip_prefix(':')?.parse::<u16>().ok()?;
                Some((host, Some(port)))
            }
        };
    }
    match value.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse::<u16>().ok()?;
            Some((host.to_ascii_lowercase(), Some(port)))
        }
        None => Some((value.to_ascii_lowercase(), None)),
    }
}

fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("an origin must name a host".to_owned());
    }
    if let Some(literal) = host.strip_prefix('[') {
        let literal = literal.strip_suffix(']').unwrap_or(literal);
        return literal
            .parse::<std::net::Ipv6Addr>()
            .map(|_| ())
            .map_err(|_| "the origin's IPv6 literal is not valid".to_owned());
    }
    if host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        return Err("the origin's host has an empty label".to_owned());
    }
    if !host
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
    {
        return Err(
            "the origin's host may contain only letters, digits, dots, and hyphens".to_owned(),
        );
    }
    Ok(())
}

fn is_default_port(scheme: &str, port: u16) -> bool {
    matches!((scheme, port), ("https", 443) | ("http", 80))
}

/// The outcome of checking a request's `Origin` against an embed's allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginDecision {
    /// The embed lists no origins, and the request carried none.
    Unrestricted,
    /// The request's origin is on the allowlist and may be echoed back.
    Allowed,
    /// The embed restricts origins and this request carried none. Bytes are
    /// still served — a non-browser client is a legitimate consumer — but no
    /// CORS grant is issued.
    NoOriginPresented,
    /// The request's origin is not on the allowlist.
    Denied,
}

impl OriginDecision {
    /// Whether the bytes may be served at all.
    #[must_use]
    pub const fn permits_delivery(self) -> bool {
        !matches!(self, Self::Denied)
    }

    /// A stable low-cardinality label safe for metrics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::Allowed => "allowed",
            Self::NoOriginPresented => "no_origin",
            Self::Denied => "denied",
        }
    }
}

/// Decides whether a request's `Origin` header satisfies an embed's allowlist.
///
/// Never reflects an arbitrary origin: the value echoed into
/// `Access-Control-Allow-Origin` is always the stored, normalized entry, not the
/// header the caller sent.
#[must_use]
pub fn evaluate_origin(allowed: &[AllowedOrigin], presented: Option<&str>) -> OriginDecision {
    match (allowed.is_empty(), presented) {
        (true, _) => OriginDecision::Unrestricted,
        (false, None) => OriginDecision::NoOriginPresented,
        (false, Some(origin)) => {
            if allowed.iter().any(|entry| entry.matches(origin)) {
                OriginDecision::Allowed
            } else {
                OriginDecision::Denied
            }
        }
    }
}

/// Returns the stored entry that matches a presented origin, if any.
#[must_use]
pub fn matching_origin<'a>(
    allowed: &'a [AllowedOrigin],
    presented: &str,
) -> Option<&'a AllowedOrigin> {
    allowed.iter().find(|entry| entry.matches(presented))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dangerous_and_malformed_schemes_are_refused() {
        for candidate in [
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "ftp://example.com",
            "example.com",
            "//example.com",
            "https://",
            "https://example.com/path",
            "https://example.com?query=1",
            "https://example.com#fragment",
            "https://user:pass@example.com",
            "https://exam ple.com",
            "https://example.com:notaport",
            "https://.example.com",
            "https://example..com",
            "https://ex*ample.com",
            "",
            "   ",
        ] {
            assert!(
                AllowedOrigin::parse(candidate).is_err(),
                "accepted dangerous origin: {candidate}"
            );
        }
    }

    #[test]
    fn origins_are_normalized_so_comparison_is_exact() {
        assert_eq!(
            AllowedOrigin::parse("HTTPS://Example.COM")
                .expect("origin")
                .as_str(),
            "https://example.com"
        );
        assert_eq!(
            AllowedOrigin::parse("https://example.com:443")
                .expect("origin")
                .as_str(),
            "https://example.com"
        );
        assert_eq!(
            AllowedOrigin::parse("http://example.com:80")
                .expect("origin")
                .as_str(),
            "http://example.com"
        );
        assert_eq!(
            AllowedOrigin::parse("https://example.com:8443")
                .expect("origin")
                .as_str(),
            "https://example.com:8443"
        );
        assert_eq!(
            AllowedOrigin::parse("https://[2001:DB8::1]:8443")
                .expect("origin")
                .as_str(),
            "https://[2001:db8::1]:8443"
        );
    }

    #[test]
    fn a_browser_origin_matches_its_normalized_stored_form() {
        let origin = AllowedOrigin::parse("https://app.example.com").expect("origin");
        assert!(origin.matches("https://app.example.com"));
        assert!(origin.matches("https://app.example.com:443"));
        assert!(!origin.matches("http://app.example.com"));
        assert!(!origin.matches("https://evil.example.com"));
        assert!(!origin.matches("https://app.example.com.evil.test"));
        assert!(!origin.matches("null"));
    }

    #[test]
    fn origin_decisions_cover_absent_allowed_and_denied_explicitly() {
        let allowed = vec![AllowedOrigin::parse("https://example.com").expect("origin")];
        assert_eq!(evaluate_origin(&[], None), OriginDecision::Unrestricted);
        assert_eq!(
            evaluate_origin(&[], Some("https://anything.test")),
            OriginDecision::Unrestricted
        );
        assert_eq!(
            evaluate_origin(&allowed, None),
            OriginDecision::NoOriginPresented
        );
        assert_eq!(
            evaluate_origin(&allowed, Some("https://example.com")),
            OriginDecision::Allowed
        );
        assert_eq!(
            evaluate_origin(&allowed, Some("https://evil.test")),
            OriginDecision::Denied
        );
        assert!(!OriginDecision::Denied.permits_delivery());
        assert!(OriginDecision::NoOriginPresented.permits_delivery());
    }

    #[test]
    fn the_echoed_origin_is_the_stored_entry_never_the_presented_header() {
        let allowed = vec![AllowedOrigin::parse("https://example.com").expect("origin")];
        let matched = matching_origin(&allowed, "https://example.com:443").expect("match");
        assert_eq!(matched.as_str(), "https://example.com");
    }
}
