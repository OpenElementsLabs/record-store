use tempfile::tempdir;

use chrono::Utc;
use record_store_core::{OrganizationId, ServiceAccountId};
use uuid::Uuid;

use super::*;

#[test]
fn credential_debug_output_contains_no_secret_field() {
    let credential = Credential {
        id: Uuid::new_v4(),
        service_account_id: ServiceAccountId::new(),
        key_id: "record_store_test_public".into(),
        disabled: false,
        created_at: Utc::now(),
        expires_at: None,
    };
    let debug = format!("{credential:?}");
    assert!(debug.contains("record_store_test_public"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("hash"));
}

#[tokio::test]
async fn service_account_secrets_are_encrypted_persistent_and_revocable() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("credentials.redb");
    let organization = OrganizationId::new();
    let manager = CredentialManager::open(
        &path,
        "root-access",
        b"root-secret-at-least-sixteen",
        Some(b"dedicated-master-key-at-least-thirty-two-bytes"),
    )
    .await
    .expect("credential manager");
    let issued = manager
        .create_service_account("backup-agent", organization)
        .await
        .expect("create account");
    let access_key = issued.info.credential.key_id.clone();
    let account_id = issued.info.account.id;
    let secret = issued.secret.expose().to_vec();
    assert!(!format!("{issued:?}").contains(String::from_utf8_lossy(&secret).as_ref()));
    drop(manager);

    let database_bytes = std::fs::read(&path).expect("credential database bytes");
    assert!(
        !database_bytes
            .windows(secret.len())
            .any(|window| window == secret)
    );

    let manager = CredentialManager::open(
        &path,
        "root-access",
        b"root-secret-at-least-sixteen",
        Some(b"dedicated-master-key-at-least-thirty-two-bytes"),
    )
    .await
    .expect("reopen credential manager");
    let (_, resolved) = manager
        .signing_secret(&access_key)
        .await
        .expect("resolve signing secret");
    assert_eq!(resolved.expose(), secret);
    manager
        .revoke_service_account(account_id)
        .await
        .expect("revoke account");
    assert!(matches!(
        manager.signing_secret(&access_key).await,
        Err(CredentialLookupError::Inactive)
    ));
}

#[tokio::test]
async fn policies_default_deny_scope_prefixes_and_prioritize_explicit_deny() {
    let directory = tempdir().expect("temporary directory");
    let manager = CredentialManager::open(
        directory.path().join("credentials.redb"),
        "root-access",
        b"root-secret-at-least-sixteen",
        Some(b"dedicated-master-key-at-least-thirty-two-bytes"),
    )
    .await
    .expect("manager");
    let issued = manager
        .create_service_account("customer-app", OrganizationId::new())
        .await
        .expect("account");
    let principal = Principal::ServiceAccount {
        id: issued.info.account.id,
        organization_id: issued.info.account.organization_id,
        credential_id: Some(issued.info.credential.id),
    };
    let read = Permission {
        action: Action::GetObject,
        resource: "bucket:customers/customer-123/report.pdf".into(),
    };
    assert!(matches!(
        manager
            .authorize(AuthorizationContext {
                principal: &principal,
                permission: &read,
            })
            .await,
        Err(AuthorizationError::Denied)
    ));
    let policy = manager
        .create_policy(
            "customer-read",
            "prefix allow with a narrower deny",
            vec![
                PolicyStatement {
                    effect: PolicyEffect::Allow,
                    actions: vec![Action::GetObject],
                    resources: vec!["bucket:customers/customer-123/*".into()],
                },
                PolicyStatement {
                    effect: PolicyEffect::Deny,
                    actions: vec![Action::GetObject],
                    resources: vec!["bucket:customers/customer-123/private/*".into()],
                },
            ],
        )
        .await
        .expect("policy");
    manager
        .attach_policy(issued.info.account.id, policy.id)
        .await
        .expect("binding");
    assert!(
        manager
            .authorize(AuthorizationContext {
                principal: &principal,
                permission: &read,
            })
            .await
            .is_ok()
    );
    let denied = Permission {
        action: Action::GetObject,
        resource: "bucket:customers/customer-123/private/secret.pdf".into(),
    };
    assert!(matches!(
        manager
            .authorize(AuthorizationContext {
                principal: &principal,
                permission: &denied,
            })
            .await,
        Err(AuthorizationError::Denied)
    ));
    let escaped = Permission {
        action: Action::GetObject,
        resource: "bucket:customers/customer-124/report.pdf".into(),
    };
    assert!(matches!(
        manager
            .authorize(AuthorizationContext {
                principal: &principal,
                permission: &escaped,
            })
            .await,
        Err(AuthorizationError::Denied)
    ));
}

#[tokio::test]
async fn expired_rotated_credentials_fail_without_cleanup() {
    let directory = tempdir().expect("temporary directory");
    let manager = CredentialManager::open(
        directory.path().join("credentials.redb"),
        "root-access",
        b"root-secret-at-least-sixteen",
        Some(b"dedicated-master-key-at-least-thirty-two-bytes"),
    )
    .await
    .expect("manager");
    let issued = manager
        .create_service_account("temporary-app", OrganizationId::new())
        .await
        .expect("account");
    let temporary = manager
        .rotate_credential(
            issued.info.account.id,
            Some(Utc::now() - chrono::Duration::seconds(1)),
        )
        .await
        .expect("temporary credential");
    assert!(matches!(
        manager
            .signing_secret(&temporary.info.credential.key_id)
            .await,
        Err(CredentialLookupError::Inactive)
    ));
}

#[tokio::test]
async fn service_account_creation_requires_an_explicit_master_key() {
    let directory = tempdir().expect("temporary directory");
    let manager = CredentialManager::open(
        directory.path().join("credentials.redb"),
        "root-access",
        b"root-secret-at-least-sixteen",
        None,
    )
    .await
    .expect("root-only manager");
    assert!(matches!(
        manager
            .create_service_account_with_description(
                "unsafe-account",
                "must not be encrypted with an implicit key",
                OrganizationId::new(),
            )
            .await,
        Err(CredentialStoreError::MasterKeyRequired)
    ));
}
