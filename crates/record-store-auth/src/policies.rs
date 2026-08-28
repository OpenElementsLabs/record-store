use std::sync::Arc;

use chrono::Utc;
use record_store_core::ServiceAccountId;
use redb::ReadableTable;
use uuid::Uuid;

use crate::keys::{policy_binding_key, store_backend};
use crate::schema::{POLICIES, POLICY_BINDINGS, POLICY_NAMES};
use crate::validation::{
    validate_description, validate_policy_statements, validate_service_account_name,
};
use crate::*;

impl CredentialManager {
    /// Creates a validated durable authorization policy.
    pub async fn create_policy(
        &self,
        name: impl Into<String>,
        description: impl Into<String>,
        statements: Vec<PolicyStatement>,
    ) -> Result<Policy, CredentialStoreError> {
        let name = name.into();
        let description = description.into();
        validate_service_account_name(&name)?;
        validate_description(&description)?;
        validate_policy_statements(&statements)?;
        let now = Utc::now();
        let policy = Policy {
            id: Uuid::new_v4(),
            name,
            description,
            statements,
            created_at: now,
            updated_at: now,
        };
        let bytes = serde_json::to_vec(&policy)?;
        let database = Arc::clone(&self.database);
        let saved = policy.clone();
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let names = write.open_table(POLICY_NAMES).map_err(store_backend)?;
                if names
                    .get(saved.name.as_str())
                    .map_err(store_backend)?
                    .is_some()
                {
                    return Err(CredentialStoreError::PolicyAlreadyExists);
                }
            }
            {
                let mut policies = write.open_table(POLICIES).map_err(store_backend)?;
                policies
                    .insert(saved.id.as_bytes().as_slice(), bytes.as_slice())
                    .map_err(store_backend)?;
            }
            {
                let mut names = write.open_table(POLICY_NAMES).map_err(store_backend)?;
                names
                    .insert(saved.name.as_str(), saved.id.as_bytes().as_slice())
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await??;
        Ok(policy)
    }

    /// Lists public policies in deterministic name order.
    pub async fn list_policies(&self) -> Result<Vec<Policy>, CredentialStoreError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database.begin_read().map_err(store_backend)?;
            let names = read.open_table(POLICY_NAMES).map_err(store_backend)?;
            let policies = read.open_table(POLICIES).map_err(store_backend)?;
            let mut out = Vec::new();
            for entry in names.iter().map_err(store_backend)? {
                let (_, id) = entry.map_err(store_backend)?;
                let bytes = policies
                    .get(id.value())
                    .map_err(store_backend)?
                    .map(|value| value.value().to_vec())
                    .ok_or(CredentialStoreError::CorruptIndex)?;
                out.push(serde_json::from_slice(&bytes)?);
            }
            Ok(out)
        })
        .await?
    }

    /// Attaches a policy to a service account.
    pub async fn attach_policy(
        &self,
        account_id: ServiceAccountId,
        policy_id: Uuid,
    ) -> Result<(), CredentialStoreError> {
        self.get_service_account(account_id).await?;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let policies = write.open_table(POLICIES).map_err(store_backend)?;
                if policies
                    .get(policy_id.as_bytes().as_slice())
                    .map_err(store_backend)?
                    .is_none()
                {
                    return Err(CredentialStoreError::PolicyNotFound);
                }
            }
            {
                let mut bindings = write.open_table(POLICY_BINDINGS).map_err(store_backend)?;
                bindings
                    .insert(policy_binding_key(account_id, policy_id).as_slice(), &1)
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await?
    }

    /// Detaches a policy from a service account.
    pub async fn detach_policy(
        &self,
        account_id: ServiceAccountId,
        policy_id: Uuid,
    ) -> Result<(), CredentialStoreError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database.begin_write().map_err(store_backend)?;
            {
                let mut bindings = write.open_table(POLICY_BINDINGS).map_err(store_backend)?;
                bindings
                    .remove(policy_binding_key(account_id, policy_id).as_slice())
                    .map_err(store_backend)?;
            }
            write.commit().map_err(store_backend)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use record_store_core::OrganizationId;
    use tempfile::tempdir;

    use super::*;

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
}
