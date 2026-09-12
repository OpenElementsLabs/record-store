use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::manager::EncryptedCredentialRecord;
use crate::*;

pub(crate) fn derive_encryption_key(material: &[u8]) -> Result<[u8; 32], CredentialStoreError> {
    let derivation = hkdf::Hkdf::<Sha256>::new(Some(b"credential-store-v1"), material);
    let mut key = [0_u8; 32];
    derivation
        .expand(b"service-account-encryption-key", &mut key)
        .map_err(|_| CredentialStoreError::Cryptography)?;
    Ok(key)
}

pub(crate) fn random_secret_bytes() -> Result<[u8; 48], CredentialStoreError> {
    let mut bytes = [0_u8; 48];
    getrandom::fill(&mut bytes).map_err(|_| CredentialStoreError::Cryptography)?;
    Ok(bytes)
}

pub(crate) fn encrypt_record(
    info: ServiceAccountInfo,
    secret: &[u8],
    key: &[u8; 32],
) -> Result<EncryptedCredentialRecord, CredentialStoreError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CredentialStoreError::Cryptography)?;
    // A full-entropy nonce: a UUIDv4 spends four of these bits on its version
    // nibble, and AES-GCM's birthday bound is tight enough not to donate them.
    let mut nonce = [0_u8; 12];
    getrandom::fill(&mut nonce).map_err(|_| CredentialStoreError::Cryptography)?;
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: secret,
                aad: info.credential.key_id.as_bytes(),
            },
        )
        .map_err(|_| CredentialStoreError::Cryptography)?;
    Ok(EncryptedCredentialRecord {
        info,
        encryption_version: 1,
        nonce,
        ciphertext,
    })
}

pub(crate) fn decrypt_record(
    record: &EncryptedCredentialRecord,
    key: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, CredentialStoreError> {
    if record.encryption_version != 1 {
        return Err(CredentialStoreError::Cryptography);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CredentialStoreError::Cryptography)?;
    let plaintext = cipher
        .decrypt(
            &Nonce::from(record.nonce),
            Payload {
                msg: &record.ciphertext,
                aad: record.info.credential.key_id.as_bytes(),
            },
        )
        .map_err(|_| CredentialStoreError::Cryptography)?;
    Ok(Zeroizing::new(plaintext))
}
