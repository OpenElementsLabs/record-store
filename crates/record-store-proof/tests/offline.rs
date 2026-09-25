//! Checking a bundle the way a third party would: a file, a JSON document, and
//! nothing else running.
//!
//! These tests deliberately go through serialization every time. A bundle that
//! only verifies in the process that built it would be useless — the whole
//! point is that it survives being written to disk, emailed, and checked
//! somewhere else months later.

use std::path::Path;

use chrono::{TimeZone, Utc};
use record_store_audit::chain::Digest32;
use record_store_audit::merkle;
use record_store_proof::bundle::{Checkpoint, HistoryRecord, key_id};
use record_store_proof::{
    BUNDLE_FORMAT, BUNDLE_FORMAT_VERSION, BundleSigner, CheckStatus, History, HistoryUnavailable,
    ObjectIdentity, PayloadDigest, ProofBundle, Verdict, verify_bundle,
};
use sha2::{Digest, Sha256};

const MASTER_KEY: &[u8] = b"proof-test-master-key-at-least-32-bytes";

fn signer() -> BundleSigner {
    BundleSigner::from_master_key(MASTER_KEY).expect("derive signing key")
}

/// Builds and signs a bundle for the given payload, as the server would.
fn bundle_for(payload: &[u8], history: History) -> ProofBundle {
    let digest: Digest32 = Sha256::digest(payload).into();
    let mut bundle = ProofBundle {
        format: BUNDLE_FORMAT.to_owned(),
        format_version: BUNDLE_FORMAT_VERSION,
        object: ObjectIdentity {
            bucket: "records".into(),
            key: "reports/2026-q1.pdf".into(),
            version_id: "0191d0f4-0000-7000-8000-000000000001".into(),
            size: payload.len() as u64,
            content_type: Some("application/pdf".into()),
            created_at: Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap(),
        },
        payload: PayloadDigest {
            sha256: hex::encode(digest),
        },
        history,
        deployment: record_store_proof::Deployment {
            algorithm: String::new(),
            public_key: String::new(),
            key_id: String::new(),
        },
        signature: record_store_proof::BundleSignature {
            algorithm: String::new(),
            canonical_version: 0,
            value: String::new(),
        },
    };
    signer().sign(&mut bundle);
    bundle
}

fn no_history() -> History {
    History::Unavailable {
        reason: HistoryUnavailable::ChainNotEnabled,
        detail: "this deployment does not maintain a tamper-evident audit chain".into(),
    }
}

/// Round-trips a bundle through JSON, as a third party receives it.
fn through_json(bundle: &ProofBundle) -> ProofBundle {
    let text = serde_json::to_string_pretty(bundle).expect("serialize");
    serde_json::from_str(&text).expect("deserialize")
}

async fn write_file(directory: &Path, name: &str, contents: &[u8]) -> std::path::PathBuf {
    let path = directory.join(name);
    tokio::fs::write(&path, contents).await.expect("write file");
    path
}

fn status_of<'a>(verdict: &'a Verdict, name: &str) -> &'a record_store_proof::Check {
    verdict
        .checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("verdict has no check named {name}"))
}

#[tokio::test]
async fn a_bundle_verifies_offline_against_the_file_it_describes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let bundle = through_json(&bundle_for(payload, no_history()));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    assert!(verdict.is_verified(), "{:#?}", verdict.checks);
    assert_eq!(
        status_of(&verdict, "payload digest").status,
        CheckStatus::Passed
    );
    assert_eq!(
        status_of(&verdict, "bundle signature").status,
        CheckStatus::Passed
    );
}

/// The headline case: the file is not what the bundle describes.
#[tokio::test]
async fn a_file_that_does_not_match_the_recorded_digest_fails_and_says_so() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let bundle = through_json(&bundle_for(b"the authoritative copy", no_history()));
    let path = write_file(directory.path(), "object.bin", b"a substituted copy").await;

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    assert!(!verdict.is_verified());
    let check = status_of(&verdict, "payload digest");
    assert_eq!(check.status, CheckStatus::Failed);
    assert!(
        check
            .detail
            .contains("not the object this bundle describes"),
        "{}",
        check.detail
    );
}

/// Every field is inside the signature, so editing any of them must be caught
/// rather than producing a bundle that describes a different object.
#[tokio::test]
async fn editing_any_field_of_a_signed_bundle_breaks_its_signature() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let original = bundle_for(payload, no_history());

    /// One named edit to a signed bundle.
    type Edit = (&'static str, Box<dyn Fn(&mut ProofBundle)>);

    let mut edits: Vec<Edit> = Vec::new();
    edits.push((
        "bucket",
        Box::new(|b: &mut ProofBundle| b.object.bucket = "other".into()),
    ));
    edits.push((
        "key",
        Box::new(|b: &mut ProofBundle| b.object.key = "other.pdf".into()),
    ));
    edits.push((
        "version id",
        Box::new(|b: &mut ProofBundle| b.object.version_id = "changed".into()),
    ));
    edits.push(("size", Box::new(|b: &mut ProofBundle| b.object.size += 1)));
    edits.push((
        "content type",
        Box::new(|b: &mut ProofBundle| b.object.content_type = Some("text/plain".into())),
    ));
    edits.push((
        "created at",
        Box::new(|b: &mut ProofBundle| {
            b.object.created_at += chrono::Duration::seconds(1);
        }),
    ));
    edits.push((
        "payload digest",
        Box::new(|b: &mut ProofBundle| b.payload.sha256 = hex::encode([9_u8; 32])),
    ));
    edits.push((
        "history reason",
        Box::new(|b: &mut ProofBundle| {
            b.history = History::Unavailable {
                reason: HistoryUnavailable::VersionPredatesChain,
                detail: "different".into(),
            };
        }),
    ));

    for (name, edit) in edits {
        let mut tampered = original.clone();
        edit(&mut tampered);
        let tampered = through_json(&tampered);
        let verdict = verify_bundle(&tampered, &path, None).await.expect("verify");
        assert_eq!(
            status_of(&verdict, "bundle signature").status,
            CheckStatus::Failed,
            "editing {name} must break the signature"
        );
        assert!(!verdict.is_verified(), "editing {name}");
    }
}

/// Without an independently supplied key, a signature says the bundle is
/// internally consistent and nothing about who produced it. The verifier has to
/// say that rather than implying provenance it has not established.
#[tokio::test]
async fn an_unpinned_key_is_reported_as_not_establishing_the_deployment() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let bundle = through_json(&bundle_for(payload, no_history()));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    let identity = status_of(&verdict, "deployment identity");
    assert_eq!(identity.status, CheckStatus::NotProved);
    assert!(
        identity
            .detail
            .contains("does not establish which deployment"),
        "{}",
        identity.detail
    );
    // It is reported among the unproved checks, not buried.
    assert!(
        verdict
            .unproved()
            .iter()
            .any(|check| check.name == "deployment identity")
    );
}

#[tokio::test]
async fn pinning_the_expected_key_establishes_the_deployment_or_rejects_it() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let bundle = through_json(&bundle_for(payload, no_history()));

    let correct = signer();
    let verdict = verify_bundle(&bundle, &path, Some(correct.public_key()))
        .await
        .expect("verify");
    assert_eq!(
        status_of(&verdict, "deployment identity").status,
        CheckStatus::Passed
    );
    assert!(verdict.is_verified());

    let other =
        BundleSigner::from_master_key(b"a-different-master-key-32-bytes-long").expect("derive");
    let verdict = verify_bundle(&bundle, &path, Some(other.public_key()))
        .await
        .expect("verify");
    assert_eq!(
        status_of(&verdict, "deployment identity").status,
        CheckStatus::Failed
    );
    assert!(!verdict.is_verified());
}

/// A bundle with no history must not read as though history were checked.
#[tokio::test]
async fn a_bundle_without_history_reports_history_and_anchor_as_not_proved() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let bundle = through_json(&bundle_for(payload, no_history()));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    let history = status_of(&verdict, "audit history");
    assert_eq!(history.status, CheckStatus::NotProved);
    assert!(
        history.detail.contains("only what it contains"),
        "{}",
        history.detail
    );
    assert_eq!(
        status_of(&verdict, "external anchor").status,
        CheckStatus::NotProved
    );
    // The bundle still verifies what it can, and the limits are listed.
    assert!(verdict.is_verified());
    assert!(verdict.unproved().len() >= 3);
}

/// Builds a history section over a synthetic checkpoint, the way the audit
/// store will once it maintains one.
///
/// The checkpoint covers `leaf_count` consecutive records starting at
/// `first_sequence`, and its Merkle tree is built over all of them. A bundle
/// carries only the records touching one object version, so `included` names
/// which of those positions appear in the section — but a record's inclusion
/// path is its path in the *checkpoint's* tree, at index
/// `sequence - from_sequence`, not its position in the excerpt. Building the
/// tree over the excerpt instead would produce a checkpoint whose leaf count
/// disagreed with the range it claims, which is now itself a finding.
fn history_over(
    first_sequence: u64,
    leaf_count: usize,
    included: &[usize],
) -> (History, Vec<Digest32>) {
    let record_hashes: Vec<Digest32> = (0..leaf_count)
        .map(|index| Sha256::digest(format!("audit-record-{index}")).into())
        .collect();
    let root = merkle::root(&record_hashes).expect("root");
    let records = included
        .iter()
        .map(|&index| HistoryRecord {
            sequence: first_sequence + index as u64,
            timestamp: Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap()
                + chrono::Duration::minutes(index as i64),
            operation: "s3:PUT".into(),
            principal: "service_account:app".into(),
            result: "success".into(),
            previous_hash: hex::encode(if index == 0 {
                record_store_audit::chain::genesis_hash()
            } else {
                record_hashes[index - 1]
            }),
            record_hash: hex::encode(record_hashes[index]),
            inclusion_path: merkle::inclusion_path(&record_hashes, index).expect("path"),
        })
        .collect();
    (
        History::Present {
            records,
            checkpoint: Checkpoint {
                sequence: 7,
                from_sequence: first_sequence,
                to_sequence: first_sequence + leaf_count as u64 - 1,
                leaf_count: leaf_count as u64,
                root: hex::encode(root),
                previous_checkpoint_hash: hex::encode([1_u8; 32]),
            },
            anchor: None,
        },
        record_hashes,
    )
}

/// A checkpoint whose every record is in the bundle.
fn history_with_records(count: usize, first_sequence: u64) -> (History, Vec<Digest32>) {
    let included: Vec<usize> = (0..count).collect();
    history_over(first_sequence, count, &included)
}

#[tokio::test]
async fn a_bundle_with_history_verifies_inclusion_and_links_offline() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let (history, _) = history_with_records(5, 100);
    let bundle = through_json(&bundle_for(payload, history));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    assert_eq!(
        status_of(&verdict, "audit inclusion").status,
        CheckStatus::Passed
    );
    assert_eq!(
        status_of(&verdict, "audit chain links").status,
        CheckStatus::Passed
    );
    assert!(verdict.is_verified(), "{:#?}", verdict.checks);
}

/// A record that does not sit under the root it claims must be caught, which is
/// what stops a bundle quoting a record that was never in the log.
#[tokio::test]
async fn a_record_that_is_not_under_the_checkpoint_root_fails_inclusion() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let (mut history, _) = history_with_records(5, 100);
    if let History::Present { records, .. } = &mut history {
        records[2].record_hash = hex::encode([0xaa_u8; 32]);
    }
    let bundle = through_json(&bundle_for(payload, history));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    let inclusion = status_of(&verdict, "audit inclusion");
    assert_eq!(inclusion.status, CheckStatus::Failed);
    assert!(inclusion.detail.contains("102"), "{}", inclusion.detail);
    assert!(!verdict.is_verified());
}

/// The leaf count is inside the signed bytes, not merely beside them. If it
/// were not, an operator could publish a tree of one size and relabel it later
/// without breaking anything a verifier checks.
#[tokio::test]
async fn changing_only_the_checkpoint_leaf_count_breaks_the_signature() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let (history, _) = history_with_records(5, 100);
    let mut bundle = bundle_for(payload, history);
    if let History::Present { checkpoint, .. } = &mut bundle.history {
        checkpoint.leaf_count += 1;
    }
    let bundle = through_json(&bundle);

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    assert_eq!(
        status_of(&verdict, "bundle signature").status,
        CheckStatus::Failed
    );
    assert!(!verdict.is_verified());
}

/// The attack the leaf count exists to catch: a checkpoint that claims a range
/// of 41 records but whose tree was built from 40. Every proof for the 40 that
/// remain still folds to the published root, so inclusion passes — and the
/// bundle is signed, so the signature passes too. Only the count disagreeing
/// with the range says that a record covered by this checkpoint is missing
/// from the tree that commits to it.
#[tokio::test]
async fn a_checkpoint_whose_tree_is_short_of_its_range_is_reported() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let (mut history, _) = history_with_records(40, 100);
    if let History::Present { checkpoint, .. } = &mut history {
        checkpoint.to_sequence = 140;
    }
    let bundle = through_json(&bundle_for(payload, history));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    assert_eq!(
        status_of(&verdict, "bundle signature").status,
        CheckStatus::Passed
    );
    assert_eq!(
        status_of(&verdict, "audit inclusion").status,
        CheckStatus::Passed
    );
    let range = status_of(&verdict, "checkpoint range");
    assert_eq!(range.status, CheckStatus::Failed);
    assert!(range.detail.contains("41 records"), "{}", range.detail);
    assert!(range.detail.contains("missing"), "{}", range.detail);
    assert!(!verdict.is_verified());
}

/// A path whose length is not the one its index takes in a tree of the claimed
/// size is rejected on its shape, before its digests are compared with
/// anything. Index 4 of a five-leaf tree is promoted twice and joins at the top
/// in a single step; in the eight-leaf tree this checkpoint claims, the same
/// index would take three.
#[tokio::test]
async fn a_path_of_the_wrong_length_for_the_claimed_tree_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let (mut history, _) = history_over(100, 5, &[4]);
    if let History::Present { checkpoint, .. } = &mut history {
        // Claim an eight-leaf tree, and stretch the range to match so that the
        // range check has nothing to say and the path length stands alone.
        checkpoint.leaf_count = 8;
        checkpoint.to_sequence = 107;
    }
    let bundle = through_json(&bundle_for(payload, history));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    assert_eq!(
        status_of(&verdict, "checkpoint range").status,
        CheckStatus::Passed
    );
    let inclusion = status_of(&verdict, "audit inclusion");
    assert_eq!(inclusion.status, CheckStatus::Failed);
    assert!(
        inclusion.detail.contains("8 leaves"),
        "{}",
        inclusion.detail
    );
    assert!(!verdict.is_verified());
}

#[tokio::test]
async fn a_broken_link_between_adjacent_records_is_reported() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let (mut history, hashes) = history_with_records(5, 100);
    if let History::Present {
        records,
        checkpoint,
        ..
    } = &mut history
    {
        // Point record 3 at the wrong predecessor while keeping its own hash
        // and path valid, so only the link check can catch it.
        records[3].previous_hash = hex::encode([0x55_u8; 32]);
        checkpoint.root = hex::encode(merkle::root(&hashes).expect("root"));
    }
    let bundle = through_json(&bundle_for(payload, history));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    let links = status_of(&verdict, "audit chain links");
    assert_eq!(links.status, CheckStatus::Failed);
    assert!(links.detail.contains("103"), "{}", links.detail);
}

/// A bundle holds only the records touching one object, so non-adjacent
/// sequences cannot be linked here. Claiming otherwise would assert something
/// about records the bundle does not contain.
#[tokio::test]
async fn non_adjacent_records_report_links_as_not_proved_rather_than_passed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    // A checkpoint over 81 records, of which this object touched three. The
    // gaps are real rather than faked by renumbering, so the inclusion paths
    // are the ones the checkpoint's own tree produces.
    let (history, _) = history_over(10, 81, &[0, 30, 80]);
    let bundle = through_json(&bundle_for(payload, history));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    assert_eq!(
        status_of(&verdict, "audit chain links").status,
        CheckStatus::NotProved
    );
    assert_eq!(
        status_of(&verdict, "audit inclusion").status,
        CheckStatus::Passed
    );
    assert_eq!(
        status_of(&verdict, "checkpoint range").status,
        CheckStatus::Passed
    );
}

/// A payload larger than one read buffer must digest correctly, because a
/// bundle has to work for objects far bigger than the machine checking them.
#[tokio::test]
async fn a_payload_spanning_many_chunks_digests_correctly() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload: Vec<u8> = (0..(64 * 1024 * 3 + 517))
        .map(|i| (i % 251) as u8)
        .collect();
    let path = write_file(directory.path(), "large.bin", &payload).await;
    let bundle = through_json(&bundle_for(&payload, no_history()));

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    assert_eq!(
        status_of(&verdict, "payload digest").status,
        CheckStatus::Passed
    );
}

/// A bundle must never carry credentials, tokens, or the payload itself.
#[tokio::test]
async fn a_bundle_carries_no_secrets_and_no_payload() {
    let payload = b"the authoritative copy and its secret contents";
    let (history, _) = history_with_records(3, 100);
    let bundle = bundle_for(payload, history);
    let text = serde_json::to_string(&bundle).expect("serialize");

    assert!(
        !text.contains("authoritative copy"),
        "the payload must not appear in the bundle"
    );
    for forbidden in ["secret", "token", "password", "credential", "access_key"] {
        assert!(
            !text.to_lowercase().contains(forbidden),
            "a bundle must not contain {forbidden}"
        );
    }
}

/// The key id is a convenience for humans comparing bundles; it has to be
/// derived from the key rather than supplied alongside it.
#[tokio::test]
async fn the_key_id_is_derived_from_the_public_key() {
    let signer = signer();
    assert_eq!(signer.key_id(), key_id(signer.public_key()));
    assert_eq!(signer.key_id().len(), 16);
}

/// A bundle from a future release may contain fields this build cannot check.
/// Reporting that is the difference between "checked" and "checked what I knew".
#[tokio::test]
async fn a_newer_format_version_is_reported_as_unverifiable_rather_than_accepted() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let payload = b"the authoritative copy";
    let path = write_file(directory.path(), "object.bin", payload).await;
    let mut bundle = bundle_for(payload, no_history());
    bundle.format_version = BUNDLE_FORMAT_VERSION + 1;
    signer().sign(&mut bundle);
    let bundle = through_json(&bundle);

    let verdict = verify_bundle(&bundle, &path, None).await.expect("verify");

    let version = status_of(&verdict, "format version");
    assert_eq!(version.status, CheckStatus::NotProved);
    assert!(
        version.detail.contains("cannot check"),
        "{}",
        version.detail
    );
}
