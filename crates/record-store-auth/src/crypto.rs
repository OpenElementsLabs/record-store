use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use sha2::Sha256;
use uuid::Uuid;
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

pub(crate) fn random_secret_bytes() -> [u8; 48] {
    let mut bytes = [0_u8; 48];
    for (chunk, uuid) in
        bytes
            .chunks_exact_mut(16)
            .zip([Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()])
    {
        chunk.copy_from_slice(uuid.as_bytes());
    }
    bytes
}

pub(crate) fn encrypt_record(
    info: ServiceAccountInfo,
    secret: &[u8],
    key: &[u8; 32],
) -> Result<EncryptedCredentialRecord, CredentialStoreError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CredentialStoreError::Cryptography)?;
    let uuid = Uuid::new_v4();
    let mut nonce = [0_u8; 12];
    nonce.copy_from_slice(&uuid.as_bytes()[..12]);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
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
            Nonce::from_slice(&record.nonce),
            Payload {
                msg: &record.ciphertext,
                aad: record.info.credential.key_id.as_bytes(),
            },
        )
        .map_err(|_| CredentialStoreError::Cryptography)?;
    Ok(Zeroizing::new(plaintext))
}
