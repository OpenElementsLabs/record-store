//! Checking a bundle offline, against a file and nothing else.
//!
//! This runs with no server, no network and no credential. It reads the bundle,
//! streams the file past a hasher, and reports every check it performed and
//! every one it could not.
//!
//! The reporting matters as much as the checking. A verifier that prints
//! "verified" after confirming a digest, while silently skipping the parts it
//! had no data for, teaches an operator to trust a word that did not mean what
//! they thought. So each check carries its own status, the ones that could not
//! be performed are listed as explicitly *not proved* rather than omitted, and
//! the overall verdict never claims more than the sum of its parts.

use std::path::Path;

use record_store_audit::chain::Digest32;
use record_store_audit::merkle;
use ring::signature::{ED25519, UnparsedPublicKey};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::ProofError;
use crate::bundle::{BUNDLE_FORMAT, BUNDLE_FORMAT_VERSION, History, ProofBundle};

/// Bytes read per chunk while digesting the object.
///
/// The payload is never held in memory: a proof bundle has to work for an
/// object far larger than the machine checking it.
const CHUNK_BYTES: usize = 64 * 1024;

/// What one check established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// The check was performed and succeeded.
    Passed,
    /// The check was performed and failed.
    Failed,
    /// The check could not be performed, so nothing was established.
    NotProved,
}

impl CheckStatus {
    /// Returns the marker used when a verdict is printed.
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Passed => "ok",
            Self::Failed => "FAILED",
            Self::NotProved => "not proved",
        }
    }
}

/// One named check and what it established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: &'static str,
    /// What the check established.
    pub status: CheckStatus,
    /// A sentence explaining the status, including why, when it failed.
    pub detail: String,
}

impl Check {
    fn passed(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Passed,
            detail: detail.into(),
        }
    }

    fn failed(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Failed,
            detail: detail.into(),
        }
    }

    fn not_proved(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::NotProved,
            detail: detail.into(),
        }
    }
}

/// The overall result of checking a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every check that could be performed succeeded.
    Verified,
    /// At least one check failed.
    Failed,
}

/// The full result: an outcome plus every check behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Whether anything failed.
    pub outcome: Outcome,
    /// Every check, in the order performed.
    pub checks: Vec<Check>,
}

impl Verdict {
    /// Returns the checks that could not be performed.
    #[must_use]
    pub fn unproved(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| check.status == CheckStatus::NotProved)
            .collect()
    }

    /// Returns whether every check that ran succeeded.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self.outcome, Outcome::Verified)
    }
}

/// Streams a file past SHA-256 without holding it in memory.
pub async fn digest_file(path: &Path) -> Result<Digest32, ProofError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|source| ProofError::Io {
            path: path.display().to_string(),
            source,
        })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|source| ProofError::Io {
                path: path.display().to_string(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

/// Checks a bundle against an object file, offline.
///
/// `expected_public_key` is the deployment key obtained from somewhere other
/// than the bundle. Without it the signature can still be checked for internal
/// consistency, but nothing establishes *which* deployment signed, and the
/// verdict says exactly that.
pub async fn verify_bundle(
    bundle: &ProofBundle,
    object_path: &Path,
    expected_public_key: Option<&[u8]>,
) -> Result<Verdict, ProofError> {
    let mut checks = Vec::new();

    checks.push(if bundle.format == BUNDLE_FORMAT {
        Check::passed("bundle format", BUNDLE_FORMAT.to_owned())
    } else {
        Check::failed(
            "bundle format",
            format!("expected {BUNDLE_FORMAT}, found {}", bundle.format),
        )
    });

    // A newer bundle may contain fields this build does not know how to check.
    // Saying so is the difference between "checked" and "checked what I could".
    checks.push(match bundle.format_version {
        BUNDLE_FORMAT_VERSION => {
            Check::passed("format version", format!("version {BUNDLE_FORMAT_VERSION}"))
        }
        version if version > BUNDLE_FORMAT_VERSION => Check::not_proved(
            "format version",
            format!(
                "bundle is version {version}; this build understands {BUNDLE_FORMAT_VERSION} \
                 and cannot check anything it added"
            ),
        ),
        version => Check::failed(
            "format version",
            format!("version {version} is older than this build understands"),
        ),
    });

    checks.push(verify_signature(bundle));
    checks.push(verify_key_identity(bundle, expected_public_key));
    checks.push(verify_payload(bundle, object_path).await?);
    checks.extend(verify_history(bundle));

    let outcome = if checks
        .iter()
        .any(|check| check.status == CheckStatus::Failed)
    {
        Outcome::Failed
    } else {
        Outcome::Verified
    };
    Ok(Verdict { outcome, checks })
}

/// Checks the bundle's signature against the key the bundle carries.
fn verify_signature(bundle: &ProofBundle) -> Check {
    if bundle.signature.algorithm != crate::signing::SIGNATURE_ALGORITHM {
        return Check::failed(
            "bundle signature",
            format!("unsupported algorithm {}", bundle.signature.algorithm),
        );
    }
    if bundle.signature.canonical_version != crate::bundle::BUNDLE_CANONICAL_VERSION {
        return Check::not_proved(
            "bundle signature",
            format!(
                "signature covers canonical encoding {}; this build produces {}",
                bundle.signature.canonical_version,
                crate::bundle::BUNDLE_CANONICAL_VERSION
            ),
        );
    }
    let (Ok(public_key), Ok(signature)) = (
        hex::decode(&bundle.deployment.public_key),
        hex::decode(&bundle.signature.value),
    ) else {
        return Check::failed("bundle signature", "the key or signature is not valid hex");
    };
    let verifier = UnparsedPublicKey::new(&ED25519, &public_key);
    if verifier
        .verify(&bundle.canonical_bytes(), &signature)
        .is_ok()
    {
        Check::passed(
            "bundle signature",
            "the bundle has not been altered since it was signed",
        )
    } else {
        Check::failed(
            "bundle signature",
            "the signature does not match the bundle contents",
        )
    }
}

/// Checks whether the signing key is one the caller already trusted.
fn verify_key_identity(bundle: &ProofBundle, expected_public_key: Option<&[u8]>) -> Check {
    let Some(expected) = expected_public_key else {
        // The crucial caveat. A key carried by the document it authenticates
        // establishes nothing about origin on its own, and a verifier that
        // glossed over that would be the weakest link in the whole format.
        return Check::not_proved(
            "deployment identity",
            format!(
                "the signature was checked against the key inside the bundle (id {}). \
                 No expected key was supplied, so this does not establish which deployment \
                 produced it. Re-run with the deployment's published key to establish that.",
                bundle.deployment.key_id
            ),
        );
    };
    match hex::decode(&bundle.deployment.public_key) {
        Ok(actual) if actual == expected => Check::passed(
            "deployment identity",
            format!(
                "signed by the expected deployment key (id {})",
                bundle.deployment.key_id
            ),
        ),
        Ok(_) => Check::failed(
            "deployment identity",
            format!(
                "the bundle was signed by key id {}, which is not the expected key",
                bundle.deployment.key_id
            ),
        ),
        Err(_) => Check::failed("deployment identity", "the bundle's key is not valid hex"),
    }
}

/// Recomputes the object's digest and compares it with the recorded one.
async fn verify_payload(bundle: &ProofBundle, object_path: &Path) -> Result<Check, ProofError> {
    let recorded = match bundle.payload_digest() {
        Ok(digest) => digest,
        Err(_) => {
            return Ok(Check::failed(
                "payload digest",
                "the recorded digest is not 32 bytes of hex",
            ));
        }
    };
    let actual = digest_file(object_path).await?;
    Ok(if actual == recorded {
        Check::passed(
            "payload digest",
            format!(
                "the file matches the SHA-256 recorded at write time ({})",
                hex::encode(recorded)
            ),
        )
    } else {
        Check::failed(
            "payload digest",
            format!(
                "the file is not the object this bundle describes: recorded {}, computed {}",
                hex::encode(recorded),
                hex::encode(actual)
            ),
        )
    })
}

/// Checks the audit history, when the bundle carries one.
fn verify_history(bundle: &ProofBundle) -> Vec<Check> {
    match &bundle.history {
        History::Unavailable { reason, detail } => vec![
            Check::not_proved(
                "audit history",
                format!(
                    "this bundle carries no audit history ({}): {detail}. \
                     Nothing here establishes what happened to the object, only what it contains.",
                    reason.label()
                ),
            ),
            Check::not_proved(
                "external anchor",
                "without audit history there is no checkpoint to anchor, so nothing \
                 establishes that this state existed at a particular time.",
            ),
        ],
        History::Present {
            records,
            checkpoint,
            anchor,
        } => {
            let mut checks = Vec::new();
            checks.push(verify_checkpoint_size(checkpoint));
            checks.push(verify_inclusion(records, checkpoint));
            checks.push(verify_links(records));
            checks.push(match anchor {
                Some(anchor) => Check::not_proved(
                    "external anchor",
                    format!(
                        "a {} receipt is present and is reproduced verbatim, but this build \
                         does not validate it. Verify it with the anchor's own tooling.",
                        anchor.kind
                    ),
                ),
                None => Check::not_proved(
                    "external anchor",
                    "no anchor receipt is present, so nothing establishes that this state \
                     existed before now other than the deployment's own assertion.",
                ),
            });
            checks
        }
    }
}

/// Checks that the checkpoint's leaf count agrees with the range it claims.
///
/// A checkpoint over `from..=to` was built from exactly that many records. The
/// two disagreeing is what a record quietly dropped from the tree looks like
/// from outside, and every proof for the records that remain would still fold
/// to the published root. So the disagreement is the finding.
fn verify_checkpoint_size(checkpoint: &crate::bundle::Checkpoint) -> Check {
    if checkpoint.to_sequence < checkpoint.from_sequence {
        return Check::failed(
            "checkpoint range",
            format!(
                "checkpoint {} claims to cover sequences {}..={}, which is not a range",
                checkpoint.sequence, checkpoint.from_sequence, checkpoint.to_sequence
            ),
        );
    }
    let covered = checkpoint.to_sequence - checkpoint.from_sequence + 1;
    if covered == checkpoint.leaf_count {
        Check::passed(
            "checkpoint range",
            format!(
                "checkpoint {} covers sequences {}..={}, which is the {} records its \
                 Merkle tree was built from",
                checkpoint.sequence,
                checkpoint.from_sequence,
                checkpoint.to_sequence,
                checkpoint.leaf_count
            ),
        )
    } else {
        Check::failed(
            "checkpoint range",
            format!(
                "checkpoint {} claims sequences {}..={}, which is {covered} records, but its \
                 Merkle tree was built from {}. Records covered by this checkpoint are \
                 missing from the tree that commits to them.",
                checkpoint.sequence,
                checkpoint.from_sequence,
                checkpoint.to_sequence,
                checkpoint.leaf_count
            ),
        )
    }
}

/// Recomputes each record's path up to the checkpoint root.
fn verify_inclusion(
    records: &[crate::bundle::HistoryRecord],
    checkpoint: &crate::bundle::Checkpoint,
) -> Check {
    let Ok(root) = decode_digest(&checkpoint.root) else {
        return Check::failed("audit inclusion", "the checkpoint root is not valid hex");
    };
    for record in records {
        let Ok(record_hash) = decode_digest(&record.record_hash) else {
            return Check::failed(
                "audit inclusion",
                format!("record {} has a malformed hash", record.sequence),
            );
        };
        // The leaf count comes from the checkpoint rather than from the path,
        // and the signature covers it. A path that is not the length that
        // index in a tree of that size must produce is rejected before its
        // digests are compared with anything: folding it would still yield
        // some root, and comparing that would be comparing a number there is
        // no reason to trust.
        let Some(computed) =
            merkle::root_from_path(&record_hash, &record.inclusion_path, checkpoint.leaf_count)
        else {
            return Check::failed(
                "audit inclusion",
                format!(
                    "record {} carries an inclusion path of {} step(s), which is not the path \
                     index {} takes in a tree of {} leaves",
                    record.sequence,
                    record.inclusion_path.steps.len(),
                    record.inclusion_path.index,
                    checkpoint.leaf_count
                ),
            );
        };
        if computed != root {
            return Check::failed(
                "audit inclusion",
                format!(
                    "record {} does not sit under the checkpoint root it claims",
                    record.sequence
                ),
            );
        }
    }
    Check::passed(
        "audit inclusion",
        format!(
            "all {} records sit under checkpoint {} covering sequences {}..={} \
             ({} leaves)",
            records.len(),
            checkpoint.sequence,
            checkpoint.from_sequence,
            checkpoint.to_sequence,
            checkpoint.leaf_count
        ),
    )
}

/// Checks that consecutive records in the bundle link to one another.
///
/// A bundle carries only the records touching one object version, so it holds a
/// subset of the log and consecutive entries are only expected to link when
/// their sequence numbers are adjacent. Claiming to have verified the chain
/// across a gap would be claiming to have seen records that are not here.
fn verify_links(records: &[crate::bundle::HistoryRecord]) -> Check {
    let mut linked = 0_usize;
    for pair in records.windows(2) {
        let (earlier, later) = (&pair[0], &pair[1]);
        if later.sequence != earlier.sequence + 1 {
            continue;
        }
        if later.previous_hash != earlier.record_hash {
            return Check::failed(
                "audit chain links",
                format!(
                    "record {} does not link to record {}",
                    later.sequence, earlier.sequence
                ),
            );
        }
        linked += 1;
    }
    if linked == 0 {
        Check::not_proved(
            "audit chain links",
            "the records in this bundle are not adjacent in the log, so their links \
             cannot be checked against one another here. Verify the full chain on the \
             deployment with `record-store audit verify`.",
        )
    } else {
        Check::passed(
            "audit chain links",
            format!("{linked} adjacent record link(s) check out"),
        )
    }
}

fn decode_digest(value: &str) -> Result<Digest32, ProofError> {
    let bytes = hex::decode(value).map_err(|_| ProofError::MalformedDigest)?;
    Digest32::try_from(bytes.as_slice()).map_err(|_| ProofError::MalformedDigest)
}
