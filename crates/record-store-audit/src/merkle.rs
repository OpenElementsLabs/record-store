//! The Merkle tree a checkpoint commits over.
//!
//! A checkpoint has to do two jobs at once: commit to every record in its range
//! with a single digest that can be anchored externally, and let one record be
//! proved a member of that digest without handing over the rest of the log. A
//! binary hash tree does both, which is why the proof bundles in the next
//! workstream can name one object's records and nothing else.
//!
//! The construction is specified in `docs/reference/audit-chain.md` because an
//! independent verifier has to reproduce it exactly. Four details there are easy
//! to get wrong and are therefore pinned by tests here.
//!
//! Leaves, interior nodes and the root are hashed with different prefixes, so
//! none of the three can ever be presented as another. The root prefix is what
//! stops a lone leaf hash from being a valid root: without it a one-record tree
//! roots at its own leaf, and anyone holding a record hash could present it as
//! a root that an empty inclusion path verifies against.
//!
//! An odd node at any level is promoted unchanged rather than duplicated,
//! because duplicating it lets two different ranges produce the same root.
//! Promotion has a cost, which the fourth detail pays: path length varies by
//! leaf position, so a verifier that does not know how many leaves the tree had
//! cannot tell a path of the right length from one built against a tree of a
//! different size. Verification therefore takes the leaf count as an argument
//! and rejects any path that is not the length that index in a tree of that
//! size must produce. The verifier this protects runs offline, on a third
//! party's machine, with no server to disagree with it, so a wrong "verified"
//! there is silent and permanent.

use sha2::{Digest, Sha256};

use crate::chain::Digest32;

/// Domain separator for every hash in the tree.
const MERKLE_DOMAIN: &[u8] = b"record-store/audit-merkle/v1";
const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;
const ROOT_PREFIX: u8 = 0x02;

/// Returns the tree leaf for one record hash.
#[must_use]
pub fn leaf_hash(record_hash: &Digest32) -> Digest32 {
    let mut hasher = Sha256::new();
    hasher.update(MERKLE_DOMAIN);
    hasher.update([LEAF_PREFIX]);
    hasher.update(record_hash);
    hasher.finalize().into()
}

/// Returns the interior node joining two children.
#[must_use]
pub fn node_hash(left: &Digest32, right: &Digest32) -> Digest32 {
    let mut hasher = Sha256::new();
    hasher.update(MERKLE_DOMAIN);
    hasher.update([NODE_PREFIX]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// Returns the root committing to the apex of a built tree.
///
/// Applied exactly once, at the top, to whatever the pairing leaves over —
/// including the single leaf of a one-record tree. A root that was merely the
/// apex would be a leaf hash in that case, and a leaf hash is a value anyone
/// holding one record already has.
#[must_use]
pub fn root_hash(apex: &Digest32) -> Digest32 {
    let mut hasher = Sha256::new();
    hasher.update(MERKLE_DOMAIN);
    hasher.update([ROOT_PREFIX]);
    hasher.update(apex);
    hasher.finalize().into()
}

/// Which side of its parent a sibling sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The sibling is the left child, so the proved hash is the right one.
    Left,
    /// The sibling is the right child, so the proved hash is the left one.
    Right,
}

/// One step of an inclusion path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PathStep {
    /// Which side the sibling is on.
    pub side: Side,
    /// The sibling's hash.
    #[serde(with = "crate::chain::digest_hex")]
    pub hash: Digest32,
}

/// A record's path from its leaf up to a checkpoint root.
///
/// Deliberately carries no leaf count of its own. The number of leaves is a
/// property of the checkpoint, which is signed and anchored; a path that
/// asserted its own tree size would be asserting the one thing it is supposed
/// to be checked against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InclusionPath {
    /// Position of the record within the checkpoint's range.
    pub index: u64,
    /// Sibling hashes, innermost first.
    pub steps: Vec<PathStep>,
}

/// Returns the root committing to every record hash, in order.
///
/// `None` for an empty range. Zero leaves has no root and no defined apex to
/// apply the root prefix to, and a checkpoint covering no records would commit
/// to nothing while looking like it committed to something. A checkpoint over
/// an empty range is never written; see [`crate::checkpoint::Checkpoint`],
/// which refuses one.
#[must_use]
pub fn root(record_hashes: &[Digest32]) -> Option<Digest32> {
    if record_hashes.is_empty() {
        return None;
    }
    let mut level: Vec<Digest32> = record_hashes.iter().map(leaf_hash).collect();
    while level.len() > 1 {
        level = combine(&level);
    }
    level.first().map(root_hash)
}

/// Reduces one level of the tree to its parents.
fn combine(level: &[Digest32]) -> Vec<Digest32> {
    let mut next = Vec::with_capacity(level.len().div_ceil(2));
    let mut pairs = level.chunks_exact(2);
    for pair in &mut pairs {
        next.push(node_hash(&pair[0], &pair[1]));
    }
    // An odd node is carried up unchanged. Hashing it with itself instead would
    // make a range of three records produce the same root as a range of four
    // whose last entry repeats, so two different logs could agree.
    if let [odd] = pairs.remainder() {
        next.push(*odd);
    }
    next
}

/// Returns how many steps the path for `index` must have in a tree of
/// `leaf_count` leaves, or `None` if that index is not in such a tree.
///
/// Derived from the promotion rule rather than from a built tree, so a verifier
/// holding nothing but a checkpoint can compute it. Every level either pairs
/// the position with a sibling, costing a step, or promotes it because it is
/// the last node of an odd level, costing none.
#[must_use]
pub fn path_length(leaf_count: u64, index: u64) -> Option<usize> {
    if index >= leaf_count {
        return None;
    }
    let mut remaining = leaf_count;
    let mut position = index;
    let mut steps = 0_usize;
    while remaining > 1 {
        let promoted = position == remaining - 1 && !remaining.is_multiple_of(2);
        if !promoted {
            steps += 1;
        }
        position /= 2;
        remaining = remaining.div_ceil(2);
    }
    Some(steps)
}

/// Returns the path proving the record at `index` is under the root.
#[must_use]
pub fn inclusion_path(record_hashes: &[Digest32], index: usize) -> Option<InclusionPath> {
    if index >= record_hashes.len() {
        return None;
    }
    let mut level: Vec<Digest32> = record_hashes.iter().map(leaf_hash).collect();
    let mut position = index;
    let mut steps = Vec::new();
    while level.len() > 1 {
        // The promoted odd node has no sibling at this level, so it
        // contributes no step; it simply rises.
        let sibling = if position.is_multiple_of(2) {
            level.get(position + 1).map(|hash| PathStep {
                side: Side::Right,
                hash: *hash,
            })
        } else {
            level.get(position - 1).map(|hash| PathStep {
                side: Side::Left,
                hash: *hash,
            })
        };
        if let Some(step) = sibling {
            steps.push(step);
        }
        position /= 2;
        level = combine(&level);
    }
    Some(InclusionPath {
        index: index as u64,
        steps,
    })
}

/// Folds a path from its leaf to the apex, without judging its shape.
fn fold_to_apex(record_hash: &Digest32, path: &InclusionPath) -> Digest32 {
    let mut current = leaf_hash(record_hash);
    for step in &path.steps {
        current = match step.side {
            Side::Left => node_hash(&step.hash, &current),
            Side::Right => node_hash(&current, &step.hash),
        };
    }
    current
}

/// Recomputes a root from one record hash and its path, in a tree of
/// `leaf_count` leaves.
///
/// This is what an offline verifier runs: it never sees the other records, only
/// the digests on the way up. `None` means the path cannot be one this tree
/// produces — either the index is outside it, or the path is not the length
/// that index must produce — and is a rejection, not a warning. Folding a path
/// of the wrong length still yields *some* digest, so a verifier that skipped
/// this would be comparing a number it had no reason to trust.
#[must_use]
pub fn root_from_path(
    record_hash: &Digest32,
    path: &InclusionPath,
    leaf_count: u64,
) -> Option<Digest32> {
    if path_length(leaf_count, path.index)? != path.steps.len() {
        return None;
    }
    Some(root_hash(&fold_to_apex(record_hash, path)))
}

/// Returns whether a record hash is proved to sit under `expected_root` in a
/// tree of `leaf_count` leaves.
///
/// The leaf count is required rather than optional on purpose. It comes from
/// the checkpoint, which is covered by the checkpoint hash and in turn by
/// whatever anchors it, so it is a committed fact rather than something the
/// path gets to claim about itself.
#[must_use]
pub fn verify_inclusion(
    record_hash: &Digest32,
    path: &InclusionPath,
    expected_root: &Digest32,
    leaf_count: u64,
) -> bool {
    root_from_path(record_hash, path, leaf_count).is_some_and(|computed| computed == *expected_root)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::chain::parse_digest;

    fn hashes(count: usize) -> Vec<Digest32> {
        (0..count)
            .map(|index| Sha256::digest(format!("record-{index}")).into())
            .collect()
    }

    /// Sizes with a committed known-answer vector file. They cover a lone leaf,
    /// a perfect pair, the smallest tree with a promotion, a promotion that
    /// rides three levels, a perfect tree, and one leaf past a perfect tree.
    const VECTOR_SIZES: [usize; 6] = [1, 2, 3, 5, 8, 9];

    /// One committed vector file, computed by an independent implementation of
    /// the written rules rather than by this module.
    #[derive(serde::Deserialize)]
    struct Vectors {
        leaf_count: u64,
        record_hashes: Vec<String>,
        leaf_hashes: Vec<String>,
        root: String,
        inclusion_paths: Vec<InclusionPath>,
    }

    impl Vectors {
        fn load(leaf_count: usize) -> Self {
            let path: PathBuf = [
                env!("CARGO_MANIFEST_DIR"),
                "tests",
                "vectors",
                "merkle",
                &format!("leaves-{leaf_count:02}.json"),
            ]
            .iter()
            .collect();
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
        }

        fn records(&self) -> Vec<Digest32> {
            self.record_hashes
                .iter()
                .map(|value| parse_digest(value).expect("a vector record hash is 32 bytes of hex"))
                .collect()
        }
    }

    /// Zero leaves is defined, and the definition is "no root". A checkpoint
    /// covering no records is never written.
    #[test]
    fn an_empty_range_has_no_root() {
        assert!(root(&[]).is_none());
        assert_eq!(path_length(0, 0), None);
    }

    /// The whole point of the root prefix. If a one-record tree rooted at its
    /// leaf, the leaf hash and the root would be the same value, and anyone
    /// holding a record hash could present it as a root.
    #[test]
    fn a_single_record_root_is_not_its_own_leaf() {
        let records = hashes(1);
        let leaf = leaf_hash(&records[0]);
        assert_eq!(root(&records), Some(root_hash(&leaf)));
        assert_ne!(root(&records), Some(leaf));
    }

    /// A leaf hash offered as a root, with the empty path that would prove
    /// anything under it, must not verify.
    #[test]
    fn a_leaf_presented_as_a_root_with_an_empty_path_is_rejected() {
        let records = hashes(1);
        let forged_root = leaf_hash(&records[0]);
        let empty = InclusionPath {
            index: 0,
            steps: Vec::new(),
        };
        assert!(!verify_inclusion(&records[0], &empty, &forged_root, 1));

        // The honest one-leaf tree still verifies, so the rejection above is
        // the prefix doing its job rather than the empty path being refused.
        let honest = root(&records).expect("a one-record range has a root");
        assert!(verify_inclusion(&records[0], &empty, &honest, 1));
    }

    /// The three prefixes must not be interchangeable, or a proof could present
    /// an interior node as a record, or a record as a root.
    #[test]
    fn a_leaf_a_node_and_a_root_over_the_same_bytes_all_differ() {
        let value: Digest32 = Sha256::digest(b"same").into();
        let leaf = leaf_hash(&value);
        let node = node_hash(&value, &value);
        let apex = root_hash(&value);
        assert_ne!(leaf, node);
        assert_ne!(leaf, apex);
        assert_ne!(node, apex);
    }

    /// The promotion rule, pinned. Duplicating the odd node instead would let a
    /// three-record range collide with a four-record one whose tail repeats.
    #[test]
    fn an_odd_node_is_promoted_rather_than_duplicated() {
        let three = hashes(3);
        let duplicated_tail = vec![three[0], three[1], three[2], three[2]];
        assert_ne!(root(&three), root(&duplicated_tail));

        // And the promotion is what the implementation actually does.
        let leaves: Vec<Digest32> = three.iter().map(leaf_hash).collect();
        let expected = root_hash(&node_hash(&node_hash(&leaves[0], &leaves[1]), &leaves[2]));
        assert_eq!(root(&three), Some(expected));
    }

    #[test]
    fn a_balanced_tree_matches_a_hand_computed_root() {
        let four = hashes(4);
        let leaves: Vec<Digest32> = four.iter().map(leaf_hash).collect();
        let expected = root_hash(&node_hash(
            &node_hash(&leaves[0], &leaves[1]),
            &node_hash(&leaves[2], &leaves[3]),
        ));
        assert_eq!(root(&four), Some(expected));
    }

    /// Every record in every range size must prove against the root it is
    /// actually under. The odd sizes are where promotion bites.
    #[test]
    fn every_record_proves_against_its_root_at_every_range_size() {
        for count in 1..=33_usize {
            let records = hashes(count);
            let expected = root(&records).expect("a non-empty range has a root");
            for index in 0..count {
                let path = inclusion_path(&records, index).expect("in range");
                assert!(
                    verify_inclusion(&records[index], &path, &expected, count as u64),
                    "record {index} of {count} must prove against its root"
                );
            }
        }
    }

    /// The length a verifier predicts from the leaf count alone has to be the
    /// length the tree actually produces, or the check would reject honest
    /// proofs. Two derivations of the same rule, compared against each other.
    #[test]
    fn the_predicted_path_length_matches_the_path_the_tree_builds() {
        for count in 1..=33_usize {
            let records = hashes(count);
            for index in 0..count {
                let path = inclusion_path(&records, index).expect("in range");
                assert_eq!(
                    path_length(count as u64, index as u64),
                    Some(path.steps.len()),
                    "record {index} of {count}"
                );
            }
        }
    }

    /// The leaf count carries its own weight. Index 4 of a five-leaf tree is
    /// promoted at two levels and joins at the top with a single step; in an
    /// eight-leaf tree the same index needs three. Here the fold still produces
    /// the genuine five-leaf root, so the root comparison would pass — the only
    /// thing that rejects the claim "this came from an eight-leaf tree" is the
    /// leaf count.
    #[test]
    fn the_leaf_count_rejects_a_path_whose_fold_still_matches() {
        let records = hashes(5);
        let genuine = root(&records).expect("root");
        let path = inclusion_path(&records, 4).expect("in range");
        assert_eq!(path.steps.len(), 1);

        assert!(verify_inclusion(&records[4], &path, &genuine, 5));
        assert!(!verify_inclusion(&records[4], &path, &genuine, 8));

        // Stated the other way around: folding alone reaches the right answer,
        // so nothing but the leaf count stands between it and a pass.
        assert_eq!(root_hash(&fold_to_apex(&records[4], &path)), genuine);
        assert_eq!(root_from_path(&records[4], &path, 8), None);
    }

    /// The adversarial sizes are the ones where promotion makes the path
    /// lengths *coincide*, because there the length check is inert by
    /// construction and the rejection has to come from somewhere else.
    ///
    /// Index 4 of a seven-leaf tree and index 4 of an eight-leaf tree both take
    /// three steps with the same side bits; the trees differ only in that the
    /// seven-leaf one promotes leaf 6 where the eight-leaf one pairs leaves 6
    /// and 7. So an operator who checkpointed seven records and later claimed
    /// eight — or who dropped the eighth and kept the old proofs — produces a
    /// path of exactly the right shape. Index 0 of a three-leaf tree against a
    /// four-leaf one is the same trap two steps up.
    ///
    /// Both must be rejected, and both are: the promoted sibling is a leaf hash
    /// where the paired one is a node hash, so the prefixes keep the roots
    /// apart and the recomputed root does not match.
    #[test]
    fn a_path_from_a_shorter_log_is_rejected_where_the_lengths_coincide() {
        for (shorter, longer, index) in [(7_usize, 8_usize, 4_usize), (3, 4, 0), (3, 4, 1)] {
            let short_records = hashes(shorter);
            let long_records = hashes(longer);
            let path = inclusion_path(&short_records, index).expect("in range");
            let long_root = root(&long_records).expect("root");

            assert_eq!(
                path_length(shorter as u64, index as u64),
                path_length(longer as u64, index as u64),
                "sizes {shorter} and {longer} were chosen because the lengths coincide"
            );
            assert!(
                !verify_inclusion(&short_records[index], &path, &long_root, longer as u64),
                "a {shorter}-leaf path at index {index} must not prove a {longer}-leaf root"
            );
        }
    }

    /// An index the tree cannot contain is rejected before anything is hashed.
    #[test]
    fn a_path_whose_index_is_outside_the_tree_is_rejected() {
        let records = hashes(8);
        let expected = root(&records).expect("root");
        let mut path = inclusion_path(&records, 3).expect("in range");
        path.index = 8;
        assert!(!verify_inclusion(&records[3], &path, &expected, 8));
        assert_eq!(path_length(8, 8), None);
    }

    /// A proof must not transfer to a record that was never in the tree.
    #[test]
    fn a_path_does_not_prove_a_record_that_is_not_in_the_tree() {
        let records = hashes(8);
        let expected = root(&records).expect("root");
        let path = inclusion_path(&records, 3).expect("in range");
        let absent: Digest32 = Sha256::digest(b"never written").into();
        assert!(!verify_inclusion(&absent, &path, &expected, 8));
    }

    /// Tampering with any step of the path must break it.
    #[test]
    fn altering_a_path_step_breaks_the_proof() {
        let records = hashes(8);
        let expected = root(&records).expect("root");
        for step_index in 0..3 {
            let mut path = inclusion_path(&records, 5).expect("in range");
            path.steps[step_index].hash[0] ^= 0xff;
            assert!(
                !verify_inclusion(&records[5], &path, &expected, 8),
                "step {step_index} altered"
            );
        }
    }

    /// Flipping which side a sibling sits on changes the order it is hashed in,
    /// which must change the root.
    #[test]
    fn flipping_a_sibling_side_breaks_the_proof() {
        let records = hashes(8);
        let expected = root(&records).expect("root");
        let mut path = inclusion_path(&records, 5).expect("in range");
        path.steps[0].side = match path.steps[0].side {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        };
        assert!(!verify_inclusion(&records[5], &path, &expected, 8));
    }

    /// Dropping a step must not be recoverable by claiming a smaller tree,
    /// which is the shortened-path version of the same attack.
    #[test]
    fn truncating_a_path_is_rejected_at_every_leaf_count() {
        let records = hashes(8);
        let expected = root(&records).expect("root");
        let mut path = inclusion_path(&records, 5).expect("in range");
        path.steps.pop();
        for leaf_count in 1..=16_u64 {
            assert!(
                !verify_inclusion(&records[5], &path, &expected, leaf_count),
                "a truncated path must not pass as a tree of {leaf_count} leaves"
            );
        }
    }

    #[test]
    fn an_index_outside_the_range_has_no_path() {
        assert!(inclusion_path(&hashes(4), 4).is_none());
        assert!(inclusion_path(&[], 0).is_none());
    }

    /// Changing any record changes the root, so a checkpoint cannot cover an
    /// edited range and still match what was anchored.
    #[test]
    fn changing_any_record_changes_the_root() {
        let records = hashes(9);
        let original = root(&records).expect("root");
        for index in 0..records.len() {
            let mut edited = records.clone();
            edited[index][0] ^= 0xff;
            assert_ne!(root(&edited), Some(original), "record {index}");
        }
    }

    #[test]
    fn a_path_round_trips_through_serialization() {
        let records = hashes(7);
        let path = inclusion_path(&records, 6).expect("in range");
        let encoded = serde_json::to_vec(&path).expect("encode");
        let decoded: InclusionPath = serde_json::from_slice(&encoded).expect("decode");
        assert_eq!(decoded, path);
    }

    /// The committed vectors are the specification's evidence: WS3 has to be
    /// able to write a verifier from `docs/reference/audit-chain.md` alone and
    /// land on these exact digests. They were produced by a separate
    /// implementation of the written rules, so a disagreement here means the
    /// document and this module have drifted apart, and which of the two is
    /// wrong has to be decided before either is edited.
    #[test]
    fn the_committed_vectors_reproduce_exactly() {
        for count in VECTOR_SIZES {
            let vectors = Vectors::load(count);
            assert_eq!(vectors.leaf_count, count as u64);
            let records = vectors.records();
            assert_eq!(records.len(), count);

            for (index, expected) in vectors.leaf_hashes.iter().enumerate() {
                assert_eq!(
                    hex::encode(leaf_hash(&records[index])),
                    *expected,
                    "leaf {index} of {count}"
                );
            }
            assert_eq!(
                root(&records).map(hex::encode),
                Some(vectors.root.clone()),
                "root of {count}"
            );
            for (index, expected) in vectors.inclusion_paths.iter().enumerate() {
                assert_eq!(
                    inclusion_path(&records, index).as_ref(),
                    Some(expected),
                    "path {index} of {count}"
                );
            }
        }
    }

    /// Round trip against the committed answers rather than against this
    /// module's own output: every index in every vector size proves against
    /// the vector's root at the vector's leaf count.
    ///
    /// The same path must not prove any *other* size's root at that size's
    /// leaf count, which is the shape the attack actually takes — the root and
    /// the leaf count travel together inside one signed checkpoint, so an
    /// attacker substitutes both or neither. Where the two sizes disagree on
    /// path length the rejection is structural and happens before anything is
    /// compared; where promotion makes the lengths coincide it is the root
    /// that separates them.
    #[test]
    fn every_index_of_every_vector_size_round_trips() {
        let loaded: Vec<(usize, Vectors)> = VECTOR_SIZES
            .into_iter()
            .map(|count| (count, Vectors::load(count)))
            .collect();

        for (count, vectors) in &loaded {
            let records = vectors.records();
            let expected_root = parse_digest(&vectors.root).expect("a vector root");

            for path in &vectors.inclusion_paths {
                let index = usize::try_from(path.index).expect("a vector index fits");
                assert!(
                    verify_inclusion(&records[index], path, &expected_root, vectors.leaf_count),
                    "index {index} of {count} must verify against the committed root"
                );

                // A leaf count that predicts a different path length is
                // refused outright, whatever it is compared against.
                for wrong in (1..=16_u64).filter(|other| *other != vectors.leaf_count) {
                    if path_length(wrong, path.index) != Some(path.steps.len()) {
                        assert_eq!(
                            root_from_path(&records[index], path, wrong),
                            None,
                            "index {index} of {count} must not fold as a tree of {wrong} leaves"
                        );
                    }
                }

                // And no other committed tree accepts this path, whether the
                // lengths coincide or not.
                for (other_count, other) in &loaded {
                    if other_count == count || path.index >= other.leaf_count {
                        continue;
                    }
                    let other_root = parse_digest(&other.root).expect("a vector root");
                    assert!(
                        !verify_inclusion(&records[index], path, &other_root, other.leaf_count),
                        "a {count}-leaf path at index {index} must not prove \
                         the {other_count}-leaf root"
                    );
                }
            }
        }
    }
}
