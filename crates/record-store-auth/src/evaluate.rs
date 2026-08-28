use record_store_core::ServiceAccountId;
use redb::{Database, ReadableTable};
use uuid::Uuid;

use crate::keys::{prefix_successor, store_backend};
use crate::manager::EncryptedCredentialRecord;
use crate::schema::{POLICIES, POLICY_BINDINGS, ROTATED_CREDENTIALS};
use crate::*;

pub(crate) fn enrich_info(
    read: &redb::ReadTransaction,
    mut info: ServiceAccountInfo,
) -> Result<ServiceAccountInfo, CredentialStoreError> {
    info.credentials = vec![info.credential.clone()];
    let rotated = read
        .open_table(ROTATED_CREDENTIALS)
        .map_err(store_backend)?;
    for entry in rotated.iter().map_err(store_backend)? {
        let (_, value) = entry.map_err(store_backend)?;
        let record: EncryptedCredentialRecord = serde_json::from_slice(value.value())?;
        if record.info.credential.service_account_id == info.account.id {
            info.credentials.push(record.info.credential);
        }
    }
    info.credentials
        .sort_by_key(|credential| credential.created_at);
    info.policy_bindings.clear();
    let bindings = read.open_table(POLICY_BINDINGS).map_err(store_backend)?;
    let prefix = info.account.id.as_uuid().as_bytes().as_slice().to_vec();
    let end = prefix_successor(&prefix);
    for entry in bindings
        .range(prefix.as_slice()..end.as_slice())
        .map_err(store_backend)?
    {
        let (key, _) = entry.map_err(store_backend)?;
        let raw = key.value();
        if raw.len() == 32 {
            let id: [u8; 16] = raw[16..]
                .try_into()
                .map_err(|_| CredentialStoreError::CorruptIndex)?;
            info.policy_bindings.push(Uuid::from_bytes(id));
        }
    }
    Ok(info)
}

pub(crate) fn evaluate_policies(
    database: &Database,
    account_id: ServiceAccountId,
    permission: &Permission,
) -> Result<(), AuthorizationError> {
    let read = database
        .begin_read()
        .map_err(|_| AuthorizationError::BackendUnavailable)?;
    let bindings = read
        .open_table(POLICY_BINDINGS)
        .map_err(|_| AuthorizationError::BackendUnavailable)?;
    let policies = read
        .open_table(POLICIES)
        .map_err(|_| AuthorizationError::BackendUnavailable)?;
    let prefix = account_id.as_uuid().as_bytes().as_slice().to_vec();
    let end = prefix_successor(&prefix);
    let mut allowed = false;
    for entry in bindings
        .range(prefix.as_slice()..end.as_slice())
        .map_err(|_| AuthorizationError::BackendUnavailable)?
    {
        let (key, _) = entry.map_err(|_| AuthorizationError::BackendUnavailable)?;
        if key.value().len() != 32 {
            return Err(AuthorizationError::BackendUnavailable);
        }
        let encoded = policies
            .get(&key.value()[16..])
            .map_err(|_| AuthorizationError::BackendUnavailable)?
            .map(|value| value.value().to_vec())
            .ok_or(AuthorizationError::BackendUnavailable)?;
        let policy: Policy =
            serde_json::from_slice(&encoded).map_err(|_| AuthorizationError::BackendUnavailable)?;
        for statement in policy.statements {
            if statement.actions.contains(&permission.action)
                && statement
                    .resources
                    .iter()
                    .any(|resource| resource_matches(resource, &permission.resource))
            {
                if statement.effect == PolicyEffect::Deny {
                    return Err(AuthorizationError::Denied);
                }
                allowed = true;
            }
        }
    }
    if allowed {
        Ok(())
    } else {
        Err(AuthorizationError::Denied)
    }
}

pub(crate) fn resource_matches(pattern: &str, resource: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(pattern == resource, |prefix| resource.starts_with(prefix))
}
