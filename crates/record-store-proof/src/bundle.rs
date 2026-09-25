//! The portable proof bundle: what it contains and how it is signed.
//!
//! A bundle exists to be checked by somebody who has the file and the bundle
//! and nothing else — no server, no network, no credential. Everything here
//! follows from that: the format is plain JSON so it can be read by hand, every
//! digest is hex so it survives being pasted into a ticket, and the bytes that
//! get signed are produced by an explicit encoder rather than by serializing
//! the JSON, because two JSON writers will not agree on key order or escaping
//! and a signature that depends on which one ran is not a signature.
//!
//! The format is specified in `docs/reference/proof-bundle.md`. That document
//! is the contract; this module is one implementation of it.

use chrono::{DateTime, Utc};
use record_store_audit::chain::Digest32;
use record_store_audit::merkle::InclusionPath;
use serde::{Deserialize, Serialize};

/// Identifies this document as a proof bundle to anything that reads it.
pub const BUNDLE_FORMAT: &str = "record-store.proof-bundle";
/// Version of the bundle format this crate produces.
pub const BUNDLE_FORMAT_VERSION: u16 = 1;
/// Version of the canonical encoding that signatures are computed over.
pub const BUNDLE_CANONICAL_VERSION: u16 = 1;
/// Domain separator for a bundle signature.
pub const BUNDLE_DOMAIN: &[u8] = b"record-store/proof-bundle/v1";

/// A self-contained statement about one immutable object version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofBundle {
    /// Always [`BUNDLE_FORMAT`].
    pub format: String,
    /// Always [`BUNDLE_FORMAT_VERSION`] for bundles this release writes.
    pub format_version: u16,
    /// Which object version the bundle is about.
    pub object: ObjectIdentity,
    /// The payload digest recorded when the object was written.
    pub payload: PayloadDigest,
    /// The audit history covering this version, when there is one.
    pub history: History,
    /// The deployment that produced and signed the bundle.
    pub deployment: Deployment,
    /// Signature over every field above.
    pub signature: BundleSignature,
}

/// Object identity. Deliberately no payload and no capability token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectIdentity {
    pub bucket: String,
    pub key: String,
    pub version_id: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// The payload digest as recorded at write time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadDigest {
    /// Lowercase hex SHA-256 of the object's bytes.
    pub sha256: String,
}

/// The audit history covering an object version.
///
/// Modelled as a tagged union with an explicit `unavailable` arm rather than an
/// optional field. An absent section and a section that failed to be produced
/// look identical once a field is merely missing, and a verifier that treats
/// "no history" as "history checked out" is worse than one that has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum History {
    /// No audit history is included, and why.
    Unavailable {
        /// Machine-readable reason.
        reason: HistoryUnavailable,
        /// Sentence an operator can act on.
        detail: String,
    },
    /// Audit records covering this version, and their place in a checkpoint.
    Present {
        /// Records touching this object version, in sequence order.
        records: Vec<HistoryRecord>,
        /// The checkpoint whose Merkle root covers those records.
        checkpoint: Checkpoint,
        /// The external anchor over that checkpoint, when anchoring is on.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<AnchorReceipt>,
    },
}

/// Why a bundle carries no audit history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryUnavailable {
    /// This deployment does not maintain a tamper-evident audit chain.
    ChainNotEnabled,
    /// The chain exists, but this version predates it.
    VersionPredatesChain,
    /// The records exist but are not yet covered by a checkpoint.
    NotYetCheckpointed,
}

impl HistoryUnavailable {
    /// Returns a stable label for reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ChainNotEnabled => "chain not enabled",
            Self::VersionPredatesChain => "version predates the chain",
            Self::NotYetCheckpointed => "not yet covered by a checkpoint",
        }
    }
}

/// One audit record, with what is needed to prove it sits under a root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecord {
    /// Position of the record in the audit log.
    pub sequence: u64,
    /// When it was recorded.
    pub timestamp: DateTime<Utc>,
    /// What happened, for example `s3:PUT`.
    pub operation: String,
    /// Who did it, by stable non-secret name.
    pub principal: String,
    /// Outcome, as the audit log recorded it.
    pub result: String,
    /// Hash of the preceding record.
    pub previous_hash: String,
    /// Hash of this record.
    pub record_hash: String,
    /// Path from this record's leaf to the checkpoint root.
    pub inclusion_path: InclusionPath,
}

/// The checkpoint covering a bundle's records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Position of the checkpoint in the checkpoint chain.
    pub sequence: u64,
    /// First audit sequence the checkpoint covers.
    pub from_sequence: u64,
    /// Last audit sequence the checkpoint covers.
    pub to_sequence: u64,
    /// How many leaves the Merkle tree was built from.
    ///
    /// Not decoration and not derivable from the range by a verifier that is
    /// entitled to distrust it. The tree promotes odd nodes rather than
    /// duplicating them, so path length varies by leaf position; without a
    /// count that the signature covers, a path of the wrong length for a tree
    /// of a different size cannot be told from a legitimate one. It is inside
    /// the signed bytes below for exactly that reason.
    pub leaf_count: u64,
    /// Merkle root over every covered record.
    pub root: String,
    /// Hash of the preceding checkpoint.
    pub previous_checkpoint_hash: String,
}

/// A durable receipt from an external anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorReceipt {
    /// Which anchor produced it, for example `rfc3161`.
    pub kind: String,
    /// The receipt itself, base64. For RFC 3161 this is the token verbatim.
    pub receipt: String,
    /// The time the anchor asserts, when it asserts one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asserted_at: Option<DateTime<Utc>>,
}

/// The deployment that produced a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    /// Signature algorithm. Always `ed25519` for format version 1.
    pub algorithm: String,
    /// Public key, hex, matching the signature below.
    pub public_key: String,
    /// Short stable identifier for the key, so two deployments are
    /// distinguishable at a glance without comparing full keys.
    pub key_id: String,
}

/// The signature over a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleSignature {
    /// Always `ed25519` for format version 1.
    pub algorithm: String,
    /// Which canonical encoding the signature was computed over.
    pub canonical_version: u16,
    /// The signature, hex.
    pub value: String,
}

/// Appends a length-prefixed byte string.
fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(value);
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_bytes(out, value.as_bytes());
}

fn put_optional_str(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            out.push(1);
            put_str(out, value);
        }
        None => out.push(0),
    }
}

fn put_time(out: &mut Vec<u8>, value: DateTime<Utc>) {
    out.extend_from_slice(&value.timestamp_micros().to_be_bytes());
}

impl ProofBundle {
    /// Returns the bytes a signature is computed over.
    ///
    /// Every field except the signature itself, in a fixed order, with explicit
    /// lengths. Deliberately not the serialized JSON: key order, whitespace and
    /// string escaping vary between writers, and a signature that only verifies
    /// under the writer that produced it is no use to the third party this
    /// whole format exists for.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        out.extend_from_slice(BUNDLE_DOMAIN);
        out.extend_from_slice(&BUNDLE_CANONICAL_VERSION.to_be_bytes());
        put_str(&mut out, &self.format);
        out.extend_from_slice(&self.format_version.to_be_bytes());

        put_str(&mut out, &self.object.bucket);
        put_str(&mut out, &self.object.key);
        put_str(&mut out, &self.object.version_id);
        out.extend_from_slice(&self.object.size.to_be_bytes());
        put_optional_str(&mut out, self.object.content_type.as_deref());
        put_time(&mut out, self.object.created_at);

        put_str(&mut out, &self.payload.sha256);

        match &self.history {
            History::Unavailable { reason, detail } => {
                out.push(0);
                put_str(&mut out, reason.label());
                put_str(&mut out, detail);
            }
            History::Present {
                records,
                checkpoint,
                anchor,
            } => {
                out.push(1);
                let count = u32::try_from(records.len()).unwrap_or(u32::MAX);
                out.extend_from_slice(&count.to_be_bytes());
                for record in records {
                    out.extend_from_slice(&record.sequence.to_be_bytes());
                    put_time(&mut out, record.timestamp);
                    put_str(&mut out, &record.operation);
                    put_str(&mut out, &record.principal);
                    put_str(&mut out, &record.result);
                    put_str(&mut out, &record.previous_hash);
                    put_str(&mut out, &record.record_hash);
                    out.extend_from_slice(&record.inclusion_path.index.to_be_bytes());
                    let steps =
                        u32::try_from(record.inclusion_path.steps.len()).unwrap_or(u32::MAX);
                    out.extend_from_slice(&steps.to_be_bytes());
                    for step in &record.inclusion_path.steps {
                        out.push(match step.side {
                            record_store_audit::merkle::Side::Left => 0,
                            record_store_audit::merkle::Side::Right => 1,
                        });
                        put_bytes(&mut out, &step.hash);
                    }
                }
                out.extend_from_slice(&checkpoint.sequence.to_be_bytes());
                out.extend_from_slice(&checkpoint.from_sequence.to_be_bytes());
                out.extend_from_slice(&checkpoint.to_sequence.to_be_bytes());
                out.extend_from_slice(&checkpoint.leaf_count.to_be_bytes());
                put_str(&mut out, &checkpoint.root);
                put_str(&mut out, &checkpoint.previous_checkpoint_hash);
                match anchor {
                    Some(anchor) => {
                        out.push(1);
                        put_str(&mut out, &anchor.kind);
                        put_str(&mut out, &anchor.receipt);
                        match anchor.asserted_at {
                            Some(time) => {
                                out.push(1);
                                put_time(&mut out, time);
                            }
                            None => out.push(0),
                        }
                    }
                    None => out.push(0),
                }
            }
        }

        put_str(&mut out, &self.deployment.algorithm);
        put_str(&mut out, &self.deployment.public_key);
        put_str(&mut out, &self.deployment.key_id);
        out
    }

    /// Returns the decoded payload digest, or an error if it is malformed.
    pub fn payload_digest(&self) -> Result<Digest32, crate::ProofError> {
        let bytes =
            hex::decode(&self.payload.sha256).map_err(|_| crate::ProofError::MalformedDigest)?;
        Digest32::try_from(bytes.as_slice()).map_err(|_| crate::ProofError::MalformedDigest)
    }
}

/// Returns the short identifier used to name a public key.
#[must_use]
pub fn key_id(public_key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(public_key)[..8])
}
