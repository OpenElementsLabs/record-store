use std::sync::Arc;

use axum::{
    Json,
    extract::{Extension, Request},
    http::header,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use record_store_auth::CredentialManager;
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The authenticated management identity, returned to clients after sign-in.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionResponse {
    role: ManagementRole,
    /// Coarse permissions the role grants.
    ///
    /// Clients use these to hide actions that would be refused. They are a
    /// usability aid only: the API enforces every permission independently.
    permissions: RolePermissions,
}

/// What a management role is allowed to do.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct RolePermissions {
    manage_buckets: bool,
    manage_objects: bool,
    manage_service_accounts: bool,
    manage_policies: bool,
    manage_webhooks: bool,
    read_audit: bool,
    manage_cluster: bool,
    manage_storage: bool,
    /// Whether this role may create and withdraw share and embed links.
    ///
    /// Separate from `manage_objects` because the two are different authorities:
    /// one changes what Record Store stores, the other decides who outside Record Store can read
    /// it. A role can reasonably have either without the other.
    manage_sharing: bool,
}

impl RolePermissions {
    pub(crate) const fn of(role: ManagementRole) -> Self {
        match role {
            ManagementRole::SystemAdministrator => Self {
                manage_buckets: true,
                manage_objects: true,
                manage_service_accounts: true,
                manage_policies: true,
                manage_webhooks: true,
                read_audit: true,
                manage_cluster: true,
                manage_storage: true,
                manage_sharing: true,
            },
            ManagementRole::StorageAdministrator => Self {
                manage_buckets: true,
                manage_objects: true,
                manage_service_accounts: false,
                manage_policies: false,
                manage_webhooks: false,
                read_audit: false,
                manage_cluster: false,
                manage_storage: true,
                manage_sharing: true,
            },
            ManagementRole::Auditor => Self {
                manage_buckets: false,
                manage_objects: false,
                manage_service_accounts: false,
                manage_policies: false,
                manage_webhooks: false,
                read_audit: true,
                manage_cluster: false,
                manage_storage: false,
                manage_sharing: false,
            },
        }
    }
}

/// Coarse management roles kept separate from S3 policy actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementRole {
    /// Full credential, policy, storage, and audit administration.
    SystemAdministrator,
    /// Bucket, object, quota, repair, and integrity administration.
    StorageAdministrator,
    /// Read-only access to operational metadata and the audit trail.
    Auditor,
}

#[derive(Clone)]
pub(crate) struct ManagementToken {
    digest: [u8; 32],
    role: ManagementRole,
}

/// Dedicated bearer-token authentication for the native management plane.
#[derive(Clone)]
pub struct ManagementAuth {
    tokens: Arc<[ManagementToken]>,
    legacy_root: Option<Arc<CredentialManager>>,
}

/// Authentication dedicated to the Prometheus scrape endpoint.
///
/// Metrics are closed when no token is configured. The scrape credential has
/// no authority on management routes.
#[derive(Clone)]
pub struct MetricsAuth {
    digest: Option<[u8; 32]>,
}

impl MetricsAuth {
    /// Creates an enabled metrics authenticator from one bearer token.
    #[must_use]
    pub fn bearer_token(token: &[u8]) -> Self {
        Self {
            digest: Some(Sha256::digest(token).into()),
        }
    }

    pub(crate) const fn disabled() -> Self {
        Self { digest: None }
    }

    pub(crate) fn authenticate(&self, request: &Request) -> bool {
        let Some(expected) = self.digest else {
            return false;
        };
        let Some(token) = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        bool::from(expected.ct_eq(&actual))
    }
}

impl ManagementAuth {
    /// Creates a token set. At least the system-administrator token is expected
    /// for a production deployment; optional role tokens can be omitted.
    #[must_use]
    pub fn bearer_tokens(
        system_administrator: &[u8],
        storage_administrator: Option<&[u8]>,
        auditor: Option<&[u8]>,
    ) -> Self {
        let mut tokens = vec![ManagementToken::new(
            system_administrator,
            ManagementRole::SystemAdministrator,
        )];
        if let Some(token) = storage_administrator {
            tokens.push(ManagementToken::new(
                token,
                ManagementRole::StorageAdministrator,
            ));
        }
        if let Some(token) = auditor {
            tokens.push(ManagementToken::new(token, ManagementRole::Auditor));
        }
        Self {
            tokens: tokens.into(),
            legacy_root: None,
        }
    }

    pub(crate) fn legacy_root(credentials: Arc<CredentialManager>) -> Self {
        Self {
            tokens: Arc::from([]),
            legacy_root: Some(credentials),
        }
    }

    pub(crate) fn authenticate(&self, request: &Request) -> Option<ManagementPrincipal> {
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?;
        if let Some(token) = authorization.strip_prefix("Bearer ") {
            let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            return self.tokens.iter().find_map(|candidate| {
                bool::from(candidate.digest.ct_eq(&digest)).then_some(ManagementPrincipal {
                    role: candidate.role,
                })
            });
        }
        let credentials = self.legacy_root.as_ref()?;
        let encoded = authorization.strip_prefix("Basic ")?;
        let decoded = STANDARD.decode(encoded).ok()?;
        let delimiter = decoded.iter().position(|byte| *byte == b':')?;
        let access = std::str::from_utf8(&decoded[..delimiter]).ok()?;
        credentials
            .verify_root(access, &decoded[delimiter + 1..])
            .then_some(ManagementPrincipal {
                role: ManagementRole::SystemAdministrator,
            })
    }
}

impl ManagementToken {
    pub(crate) fn new(token: &[u8], role: ManagementRole) -> Self {
        Self {
            digest: Sha256::digest(token).into(),
            role,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ManagementPrincipal {
    role: ManagementRole,
}

impl ManagementPrincipal {
    pub(crate) fn permits(self, request: &Request) -> bool {
        let path = request.uri().path();
        match self.role {
            ManagementRole::SystemAdministrator => true,
            ManagementRole::StorageAdministrator => {
                let cluster_mutation = request.method() != axum::http::Method::GET
                    && (path.starts_with("/api/v1/cluster")
                        || path.starts_with("/api/v1/nodes")
                        || path.starts_with("/api/v1/rebalance")
                        || path.starts_with("/api/v1/repair"));
                !cluster_mutation
                    && !path.starts_with("/api/v1/service-accounts")
                    && !path.starts_with("/api/v1/policies")
                    && !path.starts_with("/api/v1/audit")
                    && !path.starts_with("/api/v1/webhooks")
                    && !path.starts_with("/api/v1/webhook-deliveries")
            }
            ManagementRole::Auditor => {
                // An auditor may see that a capability exists and what it
                // grants, but never its URL: the URL is the capability, and
                // reading one would be an escalation dressed as a report.
                let capability_metadata = (path.starts_with("/api/v1/shares")
                    || path.starts_with("/api/v1/embeds")
                    || path.contains("/object-shares/")
                    || path.contains("/object-embeds/"))
                    && !path.ends_with("/url");
                request.method() == axum::http::Method::GET
                    && (capability_metadata
                        || path == "/api/v1/sharing/settings"
                        || path == "/api/v1/auth/session"
                        || path == "/api/v1/system/info"
                        || path == "/api/v1/events"
                        || path == "/api/v1/audit/events"
                        || path == "/api/v1/storage/status"
                        || path == "/api/v1/storage/usage"
                        || path == "/api/v1/storage/inspect"
                        || path == "/api/v1/buckets"
                        || path == "/api/v1/webhooks"
                        || path == "/api/v1/webhook-deliveries"
                        || path.starts_with("/api/v1/cluster")
                        || path.starts_with("/api/v1/nodes")
                        || path.starts_with("/api/v1/repair")
                        || path.starts_with("/api/v1/rebalance"))
            }
        }
    }

    pub(crate) const fn audit_name(self) -> &'static str {
        match self.role {
            ManagementRole::SystemAdministrator => "management:system-administrator",
            ManagementRole::StorageAdministrator => "management:storage-administrator",
            ManagementRole::Auditor => "management:auditor",
        }
    }
}

/// Returns the identity behind the presented management credential.
///
/// A console calls this immediately after sign-in: a `401` means the credential
/// is not usable, and a success tells it which actions to offer.
pub(crate) async fn auth_session(
    Extension(principal): Extension<ManagementPrincipal>,
) -> Json<SessionResponse> {
    Json(SessionResponse {
        role: principal.role,
        permissions: RolePermissions::of(principal.role),
    })
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::test_support::{
        AUDITOR_TOKEN, STORAGE_TOKEN, SYSTEM_TOKEN, api, call, expect_status, signed,
    };

    /// The three roles exist to separate who may change credentials from who may
    /// move data from who may only read the trail. If any role gained another's
    /// authority the separation would be decorative.
    #[test]
    fn each_role_grants_exactly_its_own_authority() {
        let system = RolePermissions::of(ManagementRole::SystemAdministrator);
        assert!(system.manage_service_accounts && system.manage_policies);
        assert!(system.manage_cluster && system.read_audit);

        let storage = RolePermissions::of(ManagementRole::StorageAdministrator);
        assert!(
            storage.manage_buckets && storage.manage_objects && storage.manage_storage,
            "a storage administrator moves data"
        );
        assert!(
            !storage.manage_service_accounts && !storage.manage_policies,
            "but must not change who may reach it"
        );
        assert!(!storage.manage_cluster, "nor reshape the cluster");

        let auditor = RolePermissions::of(ManagementRole::Auditor);
        assert!(auditor.read_audit, "an auditor reads the trail");
        assert!(
            !auditor.manage_buckets
                && !auditor.manage_objects
                && !auditor.manage_service_accounts
                && !auditor.manage_policies
                && !auditor.manage_webhooks
                && !auditor.manage_cluster
                && !auditor.manage_storage,
            "and changes nothing"
        );
    }

    /// The session endpoint is how a console learns what to offer. Reporting the
    /// wrong role would show an operator buttons every click of which is refused.
    #[tokio::test]
    async fn the_session_endpoint_reports_the_presented_roles_authority() {
        let (_directory, api) = api().await;
        for (token, expects_accounts, expects_audit) in [
            (SYSTEM_TOKEN, true, true),
            (STORAGE_TOKEN, false, false),
            (AUDITOR_TOKEN, false, true),
        ] {
            let response = call(&api, signed("GET", "/api/v1/auth/session", token, None)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = crate::test_support::json_body(response).await;
            assert_eq!(
                body["permissions"]["manage_service_accounts"], expects_accounts,
                "{body}"
            );
            assert_eq!(body["permissions"]["read_audit"], expects_audit, "{body}");
        }
    }

    /// A credential that was never issued must be refused, and the refusal must
    /// not reveal whether the token merely had the wrong role.
    #[tokio::test]
    async fn an_unknown_credential_is_refused_without_disclosing_why() {
        let (_directory, api) = api().await;
        let response = call(
            &api,
            signed("GET", "/api/v1/auth/session", "not-a-real-token", None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let body = crate::test_support::json_body(response).await;
        let rendered = body.to_string();
        assert!(
            !rendered.contains("role") && !rendered.contains("permission"),
            "the refusal must not describe what was missing: {rendered}"
        );
    }

    /// A malformed authorization header is a client error, not a crash, and must
    /// be refused the same way an absent one is.
    #[tokio::test]
    async fn a_malformed_authorization_header_is_refused() {
        let (_directory, api) = api().await;
        for header in ["", "Bearer", "Basic abc", "Bearer  ", "Token abc"] {
            let request = axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/auth/session")
                .header("authorization", header)
                .body(axum::body::Body::empty())
                .expect("request");
            let response = call(&api, request).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "accepted header {header:?}"
            );
        }
    }

    /// A storage administrator may move data but must not be able to mint a
    /// credential, which is the boundary the two roles exist to draw.
    #[tokio::test]
    async fn a_storage_administrator_cannot_mint_credentials() {
        let (_directory, api) = api().await;

        let allowed = call(
            &api,
            signed(
                "POST",
                "/api/v1/buckets",
                STORAGE_TOKEN,
                Some(json!({"name": "photos"})),
            ),
        )
        .await;
        assert_eq!(allowed.status(), StatusCode::CREATED);

        let refused = call(
            &api,
            signed(
                "POST",
                "/api/v1/service-accounts",
                STORAGE_TOKEN,
                Some(json!({"name": "escalated"})),
            ),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_request_without_any_credential_is_unauthorized() {
        let (_directory, api) = api().await;
        expect_status(
            &api,
            axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/auth/session")
                .body(axum::body::Body::empty())
                .expect("request"),
            StatusCode::UNAUTHORIZED,
        )
        .await;
    }
}
