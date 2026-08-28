use record_store_core::ServiceAccountId;
use uuid::Uuid;

use crate::*;

pub(crate) fn account_credential_key(account: ServiceAccountId, credential: Uuid) -> Vec<u8> {
    let mut key = account.as_uuid().as_bytes().as_slice().to_vec();
    key.extend_from_slice(credential.as_bytes());
    key
}

pub(crate) fn policy_binding_key(account: ServiceAccountId, policy: Uuid) -> Vec<u8> {
    let mut key = account.as_uuid().as_bytes().as_slice().to_vec();
    key.extend_from_slice(policy.as_bytes());
    key
}

pub(crate) fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut successor = prefix.to_vec();
    for index in (0..successor.len()).rev() {
        if successor[index] != u8::MAX {
            successor[index] += 1;
            successor.truncate(index + 1);
            return successor;
        }
    }
    successor.push(u8::MAX);
    successor
}

pub(crate) fn store_backend(error: impl std::fmt::Display) -> CredentialStoreError {
    CredentialStoreError::Database(error.to_string())
}
