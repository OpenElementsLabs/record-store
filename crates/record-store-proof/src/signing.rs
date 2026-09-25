//! Deriving the bundle signing key, and signing a bundle with it.
//!
//! The key derives from the injected deployment master key under its own domain
//! separation string, so it is distinct from the credential, webhook, object,
//! and capability keys even though all of them come from the same material.
//! Record Store never stores it: it is derived when needed and dropped.
//!
//! The honest limit, repeated wherever this key is discussed: an operator
//! holding the master key can sign any bundle they like. The signature
//! establishes that a bundle came from something with the master key, which is
//! useful against tampering in transit and against a bundle fabricated by a
//! third party. It is not evidence against the deployment's own operator. Only
//! an external anchor over a checkpoint reaches that far.

use hkdf::Hkdf;
use ring::signature::{Ed25519KeyPair, KeyPair};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::ProofError;
use crate::bundle::{BUNDLE_CANONICAL_VERSION, BundleSignature, Deployment, ProofBundle, key_id};

/// Salt separating this key from every other key derived from the master key.
const SIGNING_SALT: &[u8] = b"record-store/proof-signing/v1";
/// Info string naming what the derived key is for.
const SIGNING_INFO: &[u8] = b"proof-bundle-signing-key";
/// Algorithm name written into a bundle. Format version 1 defines only this one.
pub const SIGNATURE_ALGORITHM: &str = "ed25519";

/// Signs proof bundles on behalf of one deployment.
pub struct BundleSigner {
    key_pair: Ed25519KeyPair,
}

impl BundleSigner {
    /// Derives the deployment's signing key from its master key material.
    ///
    /// The same master key always yields the same signing key, so a bundle
    /// issued today still verifies against a bundle issued next year, and a
    /// deployment restored from backup keeps its identity.
    pub fn from_master_key(master_key: &[u8]) -> Result<Self, ProofError> {
        if master_key.is_empty() {
            return Err(ProofError::MissingMasterKey);
        }
        let derivation = Hkdf::<Sha256>::new(Some(SIGNING_SALT), master_key);
        let mut seed = Zeroizing::new([0_u8; 32]);
        derivation
            .expand(SIGNING_INFO, &mut *seed)
            .map_err(|_| ProofError::KeyDerivation)?;
        let key_pair =
            Ed25519KeyPair::from_seed_unchecked(&*seed).map_err(|_| ProofError::KeyDerivation)?;
        Ok(Self { key_pair })
    }

    /// Returns the public key a verifier needs.
    #[must_use]
    pub fn public_key(&self) -> &[u8] {
        self.key_pair.public_key().as_ref()
    }

    /// Returns the short identifier naming this deployment's key.
    #[must_use]
    pub fn key_id(&self) -> String {
        key_id(self.public_key())
    }

    /// Returns the deployment block describing this signer.
    #[must_use]
    pub fn deployment(&self) -> Deployment {
        Deployment {
            algorithm: SIGNATURE_ALGORITHM.to_owned(),
            public_key: hex::encode(self.public_key()),
            key_id: self.key_id(),
        }
    }

    /// Fills in a bundle's deployment block and signs it.
    ///
    /// The deployment block is written before signing, so the key a verifier
    /// reads out of the bundle is inside what the signature covers. A key that
    /// sat outside the signed bytes could be swapped for another one.
    pub fn sign(&self, bundle: &mut ProofBundle) {
        bundle.deployment = self.deployment();
        let signature = self.key_pair.sign(&bundle.canonical_bytes());
        bundle.signature = BundleSignature {
            algorithm: SIGNATURE_ALGORITHM.to_owned(),
            canonical_version: BUNDLE_CANONICAL_VERSION,
            value: hex::encode(signature.as_ref()),
        };
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::bundle::{
        BUNDLE_FORMAT, BUNDLE_FORMAT_VERSION, History, HistoryUnavailable, ObjectIdentity,
        PayloadDigest,
    };

    fn bundle() -> ProofBundle {
        ProofBundle {
            format: BUNDLE_FORMAT.to_owned(),
            format_version: BUNDLE_FORMAT_VERSION,
            object: ObjectIdentity {
                bucket: "records".into(),
                key: "statement.pdf".into(),
                version_id: "0191d0f4-0000-7000-8000-000000000001".into(),
                size: 11,
                content_type: Some("application/pdf".into()),
                created_at: Utc::now(),
            },
            payload: PayloadDigest {
                sha256: hex::encode([3_u8; 32]),
            },
            history: History::Unavailable {
                reason: HistoryUnavailable::ChainNotEnabled,
                detail: "no chain".into(),
            },
            deployment: Deployment {
                algorithm: String::new(),
                public_key: String::new(),
                key_id: String::new(),
            },
            signature: BundleSignature {
                algorithm: String::new(),
                canonical_version: 0,
                value: String::new(),
            },
        }
    }

    #[test]
    fn a_master_key_always_derives_the_same_signing_key() {
        let first =
            BundleSigner::from_master_key(b"master-key-at-least-32-bytes-long!!").expect("derive");
        let second =
            BundleSigner::from_master_key(b"master-key-at-least-32-bytes-long!!").expect("derive");
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(first.key_id(), second.key_id());
    }

    /// Two deployments must not share a bundle identity, or one could issue
    /// bundles that verify as the other.
    #[test]
    fn different_master_keys_derive_different_signing_keys() {
        let first =
            BundleSigner::from_master_key(b"master-key-at-least-32-bytes-long-a").expect("derive");
        let second =
            BundleSigner::from_master_key(b"master-key-at-least-32-bytes-long-b").expect("derive");
        assert_ne!(first.public_key(), second.public_key());
    }

    /// The signing key must not be the credential, webhook, object, or
    /// capability key. They all come from the same master key and are separated
    /// only by their derivation strings.
    #[test]
    fn the_signing_key_is_domain_separated_from_other_derived_keys() {
        let master = b"master-key-at-least-32-bytes-long!!";
        let signer = BundleSigner::from_master_key(master).expect("derive");

        // Reproduce the derivations the other subsystems use and confirm none
        // of them lands on this key's seed.
        let mut ours = [0_u8; 32];
        Hkdf::<Sha256>::new(Some(SIGNING_SALT), master)
            .expand(SIGNING_INFO, &mut ours)
            .expect("expand");
        for (salt, info) in [
            (
                &b"credential-store-v1"[..],
                &b"service-account-encryption-key"[..],
            ),
            (b"capability-store-v1", b"capability-token-encryption-key"),
            (b"record-store-webhook-secrets-v1", b"aes-256-gcm"),
            (
                b"record-store-object-encryption-v1",
                b"object-key-encryption-key",
            ),
        ] {
            let mut other = [0_u8; 32];
            Hkdf::<Sha256>::new(Some(salt), master)
                .expand(info, &mut other)
                .expect("expand");
            assert_ne!(
                ours,
                other,
                "collision with {}",
                String::from_utf8_lossy(salt)
            );
        }
        assert!(!signer.public_key().is_empty());
    }

    #[test]
    fn signing_fills_the_deployment_block_and_produces_a_verifiable_signature() {
        let signer =
            BundleSigner::from_master_key(b"master-key-at-least-32-bytes-long!!").expect("derive");
        let mut bundle = bundle();
        signer.sign(&mut bundle);

        assert_eq!(bundle.deployment.algorithm, SIGNATURE_ALGORITHM);
        assert_eq!(
            bundle.deployment.public_key,
            hex::encode(signer.public_key())
        );
        assert_eq!(bundle.signature.canonical_version, BUNDLE_CANONICAL_VERSION);

        let public =
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, signer.public_key());
        let signature = hex::decode(&bundle.signature.value).expect("hex");
        assert!(public.verify(&bundle.canonical_bytes(), &signature).is_ok());
    }

    /// The public key is inside the signed bytes, so swapping it for another
    /// key must invalidate the signature rather than silently re-point it.
    #[test]
    fn swapping_the_public_key_invalidates_the_signature() {
        let signer =
            BundleSigner::from_master_key(b"master-key-at-least-32-bytes-long!!").expect("derive");
        let other =
            BundleSigner::from_master_key(b"master-key-at-least-32-bytes-long-b").expect("derive");
        let mut bundle = bundle();
        signer.sign(&mut bundle);
        bundle.deployment.public_key = hex::encode(other.public_key());

        let public =
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, signer.public_key());
        let signature = hex::decode(&bundle.signature.value).expect("hex");
        assert!(
            public
                .verify(&bundle.canonical_bytes(), &signature)
                .is_err()
        );
    }

    #[test]
    fn an_absent_master_key_is_refused_rather_than_signing_with_nothing() {
        assert!(matches!(
            BundleSigner::from_master_key(b""),
            Err(ProofError::MissingMasterKey)
        ));
    }
}
