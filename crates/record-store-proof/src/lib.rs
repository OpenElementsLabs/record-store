//! Portable proof bundles: integrity a third party can check without a server.
//!
//! A bundle says what Record Store recorded about one immutable object version.
//! Somebody holding the file and the bundle, and nothing else, can recompute
//! the payload digest, check the deployment's signature, and — once the audit
//! chain exists — follow each audit record up to a checkpoint root and on to an
//! external anchor.
//!
//! What a bundle is not is a certificate of authenticity. A signature made with
//! a key derived from the deployment's own master key proves the bundle came
//! from something holding that key; it proves nothing to somebody who has not
//! obtained the public key independently. The verifier says so out loud rather
//! than reporting a green tick, because a verdict that overstates what it
//! checked is worse than no verdict.

pub mod anchor;
pub mod bundle;
pub mod signing;
pub mod verify;

pub use bundle::{
    AnchorReceipt, BUNDLE_CANONICAL_VERSION, BUNDLE_FORMAT, BUNDLE_FORMAT_VERSION, BundleSignature,
    Checkpoint, Deployment, History, HistoryRecord, HistoryUnavailable, ObjectIdentity,
    PayloadDigest, ProofBundle,
};
pub use signing::BundleSigner;
pub use verify::{Check, CheckStatus, Outcome, Verdict, verify_bundle};

use thiserror::Error;

/// Why a bundle could not be produced or checked.
#[derive(Debug, Error)]
pub enum ProofError {
    #[error("the deployment master key is required to sign a proof bundle")]
    MissingMasterKey,
    #[error("deriving the bundle signing key failed")]
    KeyDerivation,
    #[error("a digest in the bundle is not 32 bytes of hex")]
    MalformedDigest,
    #[error("the bundle is not valid JSON: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}
