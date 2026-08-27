//! Per-bucket cross-origin resource sharing.
//!
//! A browser will not let a page read another origin's response, or send it a
//! `PUT` at all, unless that origin says so first. For an S3-compatible service
//! that permission is a property of the bucket: the owner of `assets` may want a
//! single web application uploading into it, while `payroll` should never be
//! reachable from a page at all. Storing it anywhere else — one setting for the
//! whole deployment — would mean the most permissive application in the estate
//! sets the policy for every bucket in it.
//!
//! Two properties are deliberate. Nothing here ever grants
//! `Access-Control-Allow-Credentials`: S3 authorizes with a signature, not a
//! cookie, so a browser must never be invited to attach ambient credentials to a
//! storage request. And a request's `Origin` is echoed back only after it has
//! matched a stored pattern — a header that fails to match is never reflected,
//! because reflecting one is how a permissive-by-accident policy becomes a
//! universal one.

use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::CoreError;

/// The most rules one bucket may carry, matching the S3 limit.
pub const MAXIMUM_CORS_RULES: usize = 100;

/// Longest accepted origin or header pattern.
const MAXIMUM_PATTERN_LENGTH: usize = 255;

/// Longest accepted preflight cache lifetime.
///
/// A day. The rule that permitted a request is cached by the browser for this
/// long, so a policy that was tightened stays effective for at most that window;
/// a year would make a correction meaningless.
pub const MAXIMUM_CORS_MAX_AGE_SECONDS: u32 = 86_400;

/// The HTTP methods a CORS rule may permit.
///
/// Exactly the set S3 accepts. `OPTIONS` is absent on purpose: it is the
/// preflight itself, not something a rule grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CorsMethod {
    /// Read an object or list a bucket.
    Get,
    /// Read metadata without a body.
    Head,
    /// Upload or replace an object.
    Put,
    /// Complete a multipart upload, or a browser form post.
    Post,
    /// Remove an object.
    Delete,
}

impl CorsMethod {
    /// Parses a method name as it appears in a configuration document or header.
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        match value.trim().to_ascii_uppercase().as_str() {
            "GET" => Ok(Self::Get),
            "HEAD" => Ok(Self::Head),
            "PUT" => Ok(Self::Put),
            "POST" => Ok(Self::Post),
            "DELETE" => Ok(Self::Delete),
            other => Err(CoreError::InvalidCorsRule(format!(
                "{other} is not a method a CORS rule may allow"
            ))),
        }
    }

    /// Returns the canonical uppercase method name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Put => "PUT",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

impl Display for CorsMethod {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A validated origin or header pattern with at most one `*`.
///
/// S3 allows a single wildcard anywhere in the value, so `https://*.example.com`
/// and `x-amz-*` are both legal. More than one would make the pattern's meaning
/// depend on how a matcher chooses to split it, which is exactly the kind of
/// ambiguity an authorization decision must not rest on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CorsPattern(String);

impl CorsPattern {
    /// Parses an origin pattern.
    pub fn origin(value: &str) -> Result<Self, CoreError> {
        let value = Self::sanitise(value, "origin")?;
        if value == "*" {
            return Ok(Self(value));
        }
        // Anything that is not the bare wildcard has to look like an origin,
        // because a browser only ever presents one. Accepting a bare hostname
        // would produce a rule that silently matches nothing.
        if !(value.starts_with("https://") || value.starts_with("http://")) {
            return Err(CoreError::InvalidCorsRule(format!(
                "{value:?} must be * or an origin such as https://app.example.com"
            )));
        }
        let remainder = value
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        if remainder.is_empty() {
            return Err(CoreError::InvalidCorsRule(format!(
                "{value:?} must name a host"
            )));
        }
        // An origin is a scheme, host, and port. A path, query, fragment, or
        // credentials would never appear in a browser's `Origin`, so a rule
        // carrying one matches nothing and is a mistake worth reporting.
        if remainder.contains('/')
            || remainder.contains('?')
            || remainder.contains('#')
            || remainder.contains('@')
        {
            return Err(CoreError::InvalidCorsRule(format!(
                "{value:?} must carry no path, query, fragment, or credentials"
            )));
        }
        Ok(Self(value))
    }

    /// Parses a request-header pattern.
    pub fn header(value: &str) -> Result<Self, CoreError> {
        let value = Self::sanitise(value, "header")?;
        if value == "*" {
            return Ok(Self(value));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.*".contains(&byte))
        {
            return Err(CoreError::InvalidCorsRule(format!(
                "{value:?} is not a valid header name pattern"
            )));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    fn sanitise(value: &str, what: &str) -> Result<String, CoreError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidCorsRule(format!(
                "a CORS {what} must not be empty"
            )));
        }
        if trimmed.len() > MAXIMUM_PATTERN_LENGTH {
            return Err(CoreError::InvalidCorsRule(format!(
                "a CORS {what} must be at most {MAXIMUM_PATTERN_LENGTH} characters"
            )));
        }
        if trimmed
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
        {
            return Err(CoreError::InvalidCorsRule(format!(
                "a CORS {what} must not contain spaces or control characters"
            )));
        }
        if !trimmed.is_ascii() {
            return Err(CoreError::InvalidCorsRule(format!(
                "a CORS {what} must use its ASCII wire representation"
            )));
        }
        if trimmed.matches('*').count() > 1 {
            return Err(CoreError::InvalidCorsRule(format!(
                "a CORS {what} may contain at most one *"
            )));
        }
        Ok(trimmed.to_owned())
    }

    /// Returns the pattern text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this pattern is the bare wildcard.
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        self.0 == "*"
    }

    /// Whether `candidate` satisfies this pattern.
    ///
    /// Comparison is case-sensitive for origins, because a browser sends the
    /// serialized origin in lower case already and a rule that only matched a
    /// differently-cased spelling would be a rule that matched nothing.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        match self.0.split_once('*') {
            None => self.0 == candidate,
            Some((prefix, suffix)) => {
                candidate.len() >= prefix.len() + suffix.len()
                    && candidate.starts_with(prefix)
                    && candidate.ends_with(suffix)
            }
        }
    }
}

impl Display for CorsPattern {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One rule from a bucket's CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsRule {
    /// Operator-supplied identifier, echoed back unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Origins this rule speaks for.
    pub allowed_origins: Vec<CorsPattern>,
    /// Methods this rule permits.
    pub allowed_methods: Vec<CorsMethod>,
    /// Request headers a preflight may ask for.
    #[serde(default)]
    pub allowed_headers: Vec<CorsPattern>,
    /// Response headers a page is allowed to read.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// How long a browser may cache the preflight decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_seconds: Option<u32>,
}

impl CorsRule {
    /// Validates one rule.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.allowed_origins.is_empty() {
            return Err(CoreError::InvalidCorsRule(
                "a CORS rule must allow at least one origin".to_owned(),
            ));
        }
        if self.allowed_methods.is_empty() {
            return Err(CoreError::InvalidCorsRule(
                "a CORS rule must allow at least one method".to_owned(),
            ));
        }
        if self
            .id
            .as_deref()
            .is_some_and(|id| id.len() > 255 || id.chars().any(char::is_control))
        {
            return Err(CoreError::InvalidCorsRule(
                "a CORS rule identifier must be at most 255 printable characters".to_owned(),
            ));
        }
        if self
            .max_age_seconds
            .is_some_and(|age| age > MAXIMUM_CORS_MAX_AGE_SECONDS)
        {
            return Err(CoreError::InvalidCorsRule(format!(
                "max_age_seconds must be at most {MAXIMUM_CORS_MAX_AGE_SECONDS}"
            )));
        }
        for header in &self.expose_headers {
            // No wildcard: a page being told it may read every response header
            // is not something a bucket owner can have meant, and S3 refuses it
            // too.
            if header.contains('*')
                || header.is_empty()
                || header.len() > MAXIMUM_PATTERN_LENGTH
                || !header
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            {
                return Err(CoreError::InvalidCorsRule(format!(
                    "{header:?} is not a valid header to expose"
                )));
            }
        }
        Ok(())
    }

    /// Whether this rule speaks for `origin`.
    #[must_use]
    pub fn matches_origin(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|pattern| pattern.matches(origin))
    }

    /// Whether this rule permits `method`.
    #[must_use]
    pub fn permits(&self, method: CorsMethod) -> bool {
        self.allowed_methods.contains(&method)
    }

    /// Whether every requested header is permitted by this rule.
    #[must_use]
    pub fn permits_headers(&self, requested: &[String]) -> bool {
        requested.iter().all(|header| {
            let header = header.to_ascii_lowercase();
            self.allowed_headers
                .iter()
                .any(|pattern| pattern.matches(&header))
        })
    }
}

/// A bucket's complete CORS configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsConfiguration {
    /// Rules, evaluated in order. The first match wins, as in S3.
    pub rules: Vec<CorsRule>,
}

impl CorsConfiguration {
    /// Validates a whole configuration.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.rules.is_empty() {
            return Err(CoreError::InvalidCorsRule(
                "a CORS configuration must contain at least one rule".to_owned(),
            ));
        }
        if self.rules.len() > MAXIMUM_CORS_RULES {
            return Err(CoreError::InvalidCorsRule(format!(
                "a CORS configuration may contain at most {MAXIMUM_CORS_RULES} rules"
            )));
        }
        for rule in &self.rules {
            rule.validate()?;
        }
        Ok(())
    }

    /// Finds the rule that answers a preflight, if any.
    ///
    /// All three conditions have to hold in the same rule. A configuration that
    /// permits the origin in one rule and the method in another permits neither
    /// combination, which is what S3 does and what a reader of the rules would
    /// expect.
    #[must_use]
    pub fn match_preflight(
        &self,
        origin: &str,
        method: CorsMethod,
        requested_headers: &[String],
    ) -> Option<&CorsRule> {
        self.rules.iter().find(|rule| {
            rule.matches_origin(origin)
                && rule.permits(method)
                && rule.permits_headers(requested_headers)
        })
    }

    /// Finds the rule that decorates an actual request's response, if any.
    ///
    /// Headers are not consulted here. The browser already had them approved at
    /// the preflight, and a simple request never sent any.
    #[must_use]
    pub fn match_request(&self, origin: &str, method: CorsMethod) -> Option<&CorsRule> {
        self.rules
            .iter()
            .find(|rule| rule.matches_origin(origin) && rule.permits(method))
    }
}

/// What a matched rule tells the browser.
///
/// Built from the rule and the request together, so the caller emits headers
/// rather than deciding policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsGrant {
    /// Value for `Access-Control-Allow-Origin`.
    pub allow_origin: String,
    /// Value for `Access-Control-Allow-Methods`, when answering a preflight.
    pub allow_methods: Option<String>,
    /// Value for `Access-Control-Allow-Headers`, when answering a preflight.
    pub allow_headers: Option<String>,
    /// Value for `Access-Control-Expose-Headers`.
    pub expose_headers: Option<String>,
    /// Value for `Access-Control-Max-Age`.
    pub max_age_seconds: Option<u32>,
}

impl CorsGrant {
    /// Builds the grant for a preflight answer.
    #[must_use]
    pub fn preflight(rule: &CorsRule, origin: &str, requested_headers: &[String]) -> Self {
        Self {
            allow_origin: Self::allow_origin(rule, origin),
            // The rule's full method set, so one preflight covers every request
            // the page is about to make rather than one per method.
            allow_methods: Some(
                rule.allowed_methods
                    .iter()
                    .map(|method| method.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            // Echoing the requested headers, not the rule's patterns: `x-amz-*`
            // is not a header name a browser can act on, and every one of these
            // has already been checked against the patterns.
            allow_headers: (!requested_headers.is_empty()).then(|| requested_headers.join(", ")),
            expose_headers: Self::expose(rule),
            max_age_seconds: rule.max_age_seconds,
        }
    }

    /// Builds the grant that decorates an actual response.
    #[must_use]
    pub fn response(rule: &CorsRule, origin: &str) -> Self {
        Self {
            allow_origin: Self::allow_origin(rule, origin),
            allow_methods: None,
            allow_headers: None,
            expose_headers: Self::expose(rule),
            max_age_seconds: None,
        }
    }

    /// Whether the grant is the bare wildcard rather than one named origin.
    ///
    /// Callers use this to decide whether `Vary: Origin` carries information: a
    /// wildcard answer is the same for everyone, so a cache may reuse it.
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        self.allow_origin == "*"
    }

    fn allow_origin(rule: &CorsRule, origin: &str) -> String {
        // A rule written as `*` answers with `*`, which is what an SDK expects
        // and is safe here precisely because Record Store never allows credentials on a
        // cross-origin storage request. Any other rule answers with the origin
        // that matched it — never with an unmatched header.
        if rule.allowed_origins.iter().any(CorsPattern::is_wildcard) {
            "*".to_owned()
        } else {
            origin.to_owned()
        }
    }

    fn expose(rule: &CorsRule) -> Option<String> {
        (!rule.expose_headers.is_empty()).then(|| rule.expose_headers.join(", "))
    }
}

/// Splits an `Access-Control-Request-Headers` value into individual names.
#[must_use]
pub fn parse_requested_headers(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(origins: &[&str], methods: &[CorsMethod], headers: &[&str]) -> CorsRule {
        CorsRule {
            id: None,
            allowed_origins: origins
                .iter()
                .map(|origin| CorsPattern::origin(origin).expect("origin"))
                .collect(),
            allowed_methods: methods.to_vec(),
            allowed_headers: headers
                .iter()
                .map(|header| CorsPattern::header(header).expect("header"))
                .collect(),
            expose_headers: Vec::new(),
            max_age_seconds: Some(600),
        }
    }

    #[test]
    fn origins_must_look_like_origins() {
        for candidate in [
            "example.com",
            "https://example.com/path",
            "https://example.com?query",
            "https://example.com#fragment",
            "https://user@example.com",
            "https://",
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///etc/passwd",
            "",
            "   ",
            "https://a b.com",
            "https://éxample.com",
            "https://*.*.example.com",
        ] {
            assert!(
                CorsPattern::origin(candidate).is_err(),
                "accepted bad origin: {candidate}"
            );
        }
        for candidate in [
            "*",
            "https://example.com",
            "http://localhost:3000",
            "https://*.example.com",
            "https://example.com:8443",
        ] {
            assert!(
                CorsPattern::origin(candidate).is_ok(),
                "rejected good origin: {candidate}"
            );
        }
    }

    #[test]
    fn a_single_wildcard_matches_a_prefix_and_suffix_and_nothing_shorter() {
        let pattern = CorsPattern::origin("https://*.example.com").expect("origin");
        assert!(pattern.matches("https://app.example.com"));
        assert!(pattern.matches("https://a.example.com"));
        // The wildcard stands for at least nothing, but the fixed parts must not
        // be allowed to overlap and match a shorter string than they spell.
        assert!(!pattern.matches("https://example.com"));
        assert!(!pattern.matches("http://app.example.com"));
        assert!(!pattern.matches("https://app.example.com.evil.test"));
        assert!(!pattern.matches("https://app.example.co"));
    }

    #[test]
    fn a_wildcard_origin_matches_anything_and_answers_with_a_wildcard() {
        let permissive = rule(&["*"], &[CorsMethod::Get], &["*"]);
        assert!(permissive.matches_origin("https://anything.test"));
        let grant = CorsGrant::response(&permissive, "https://anything.test");
        assert_eq!(grant.allow_origin, "*");
        assert!(grant.is_wildcard());
    }

    #[test]
    fn an_unmatched_origin_is_never_reflected() {
        let strict = rule(&["https://app.example.com"], &[CorsMethod::Put], &["*"]);
        let configuration = CorsConfiguration {
            rules: vec![strict],
        };
        // No rule matches, so there is no grant at all and therefore nothing to
        // echo. Reflecting the header is how a narrow policy becomes universal.
        assert!(
            configuration
                .match_preflight("https://evil.test", CorsMethod::Put, &[])
                .is_none()
        );
        let matched = configuration
            .match_preflight("https://app.example.com", CorsMethod::Put, &[])
            .expect("rule");
        assert_eq!(
            CorsGrant::response(matched, "https://app.example.com").allow_origin,
            "https://app.example.com"
        );
    }

    #[test]
    fn origin_method_and_headers_must_all_hold_in_the_same_rule() {
        // Splitting the permission across two rules permits neither combination.
        let configuration = CorsConfiguration {
            rules: vec![
                rule(&["https://app.example.com"], &[CorsMethod::Get], &["*"]),
                rule(&["https://other.example.com"], &[CorsMethod::Put], &["*"]),
            ],
        };
        assert!(
            configuration
                .match_preflight("https://app.example.com", CorsMethod::Put, &[])
                .is_none()
        );
        assert!(
            configuration
                .match_preflight("https://app.example.com", CorsMethod::Get, &[])
                .is_some()
        );
    }

    #[test]
    fn a_preflight_asking_for_an_unlisted_header_does_not_match() {
        let configuration = CorsConfiguration {
            rules: vec![rule(
                &["https://app.example.com"],
                &[CorsMethod::Put],
                &["content-type", "x-amz-*"],
            )],
        };
        let permitted = parse_requested_headers("content-type, x-amz-date, X-Amz-Content-Sha256");
        assert!(
            configuration
                .match_preflight("https://app.example.com", CorsMethod::Put, &permitted)
                .is_some()
        );
        let refused = parse_requested_headers("content-type, authorization");
        assert!(
            configuration
                .match_preflight("https://app.example.com", CorsMethod::Put, &refused)
                .is_none()
        );
    }

    #[test]
    fn a_preflight_grant_echoes_the_requested_headers_not_the_patterns() {
        let matched = rule(
            &["https://app.example.com"],
            &[CorsMethod::Put, CorsMethod::Get],
            &["x-amz-*"],
        );
        let requested = parse_requested_headers("x-amz-date, x-amz-content-sha256");
        let grant = CorsGrant::preflight(&matched, "https://app.example.com", &requested);
        // `x-amz-*` is not a header a browser can act on.
        assert_eq!(
            grant.allow_headers.as_deref(),
            Some("x-amz-date, x-amz-content-sha256")
        );
        assert_eq!(grant.allow_methods.as_deref(), Some("PUT, GET"));
        assert_eq!(grant.max_age_seconds, Some(600));
    }

    #[test]
    fn an_actual_response_grant_carries_no_preflight_only_headers() {
        let matched = rule(&["https://app.example.com"], &[CorsMethod::Get], &["*"]);
        let grant = CorsGrant::response(&matched, "https://app.example.com");
        assert!(grant.allow_methods.is_none());
        assert!(grant.allow_headers.is_none());
        assert!(grant.max_age_seconds.is_none());
    }

    #[test]
    fn exposed_headers_are_validated_and_never_wildcarded() {
        let mut candidate = rule(&["*"], &[CorsMethod::Get], &["*"]);
        candidate.expose_headers = vec!["etag".to_owned(), "x-amz-version-id".to_owned()];
        assert!(candidate.validate().is_ok());
        assert_eq!(
            CorsGrant::response(&candidate, "https://a.test").expose_headers,
            Some("etag, x-amz-version-id".to_owned())
        );

        for bad in ["*", "", "has space", "with\u{0}control"] {
            let mut broken = rule(&["*"], &[CorsMethod::Get], &["*"]);
            broken.expose_headers = vec![bad.to_owned()];
            assert!(
                broken.validate().is_err(),
                "accepted exposed header {bad:?}"
            );
        }
    }

    #[test]
    fn configurations_are_bounded_and_must_say_something() {
        assert!(CorsConfiguration::default().validate().is_err());
        let oversized = CorsConfiguration {
            rules: vec![rule(&["*"], &[CorsMethod::Get], &["*"]); MAXIMUM_CORS_RULES + 1],
        };
        assert!(oversized.validate().is_err());

        let mut empty_methods = rule(&["*"], &[], &["*"]);
        empty_methods.allowed_methods.clear();
        assert!(empty_methods.validate().is_err());

        let mut empty_origins = rule(&["*"], &[CorsMethod::Get], &["*"]);
        empty_origins.allowed_origins.clear();
        assert!(empty_origins.validate().is_err());
    }

    #[test]
    fn a_preflight_lifetime_is_bounded_so_a_tightened_policy_takes_effect() {
        let mut long = rule(&["*"], &[CorsMethod::Get], &["*"]);
        long.max_age_seconds = Some(MAXIMUM_CORS_MAX_AGE_SECONDS);
        assert!(long.validate().is_ok());
        long.max_age_seconds = Some(MAXIMUM_CORS_MAX_AGE_SECONDS + 1);
        assert!(long.validate().is_err());
    }

    #[test]
    fn methods_are_the_s3_set_and_never_the_preflight_itself() {
        for name in ["get", "Head", "PUT", "post", "DELETE"] {
            assert!(CorsMethod::parse(name).is_ok(), "rejected {name}");
        }
        for name in ["OPTIONS", "PATCH", "TRACE", "CONNECT", ""] {
            assert!(CorsMethod::parse(name).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn header_patterns_are_matched_without_regard_to_case() {
        let configuration = CorsConfiguration {
            rules: vec![rule(&["*"], &[CorsMethod::Put], &["Content-Type"])],
        };
        assert!(
            configuration
                .match_preflight(
                    "https://a.test",
                    CorsMethod::Put,
                    &["content-type".to_owned()]
                )
                .is_some()
        );
        assert!(
            configuration
                .match_preflight(
                    "https://a.test",
                    CorsMethod::Put,
                    &["CONTENT-TYPE".to_owned()]
                )
                .is_some()
        );
    }
}
