//! Checkpoints: the digest a range of audit records is anchored by.
//!
//! A checkpoint names a contiguous range of the log, the Merkle root over the
//! records in it, and the checkpoint before it. Anchoring that one digest
//! externally is what makes a past state provable against an operator who can
//! rewrite every record and every hash, which the chain on its own cannot do.
//!
//! The field that is easy to leave out is `leaf_count`. The tree promotes odd
//! nodes rather than duplicating them, so inclusion-path length varies by leaf
//! position and a verifier that does not know how many leaves the tree had
//! cannot judge whether a path is the right shape. Worse, without a committed
//! count an operator could publish a root over a range with one record quietly
//! dropped, and every proof for the records that remain would still verify.
//! The count is therefore inside the checkpoint hash, not merely next to it,
//! and the range it claims to cover has to agree with it.
//!
//! The preimage is specified in `docs/reference/audit-chain.md`. Changing it
//! means a new checkpoint version, never an edit to version 1: once a
//! checkpoint has been signed and anchored, its bytes are fixed forever.

use sha2::{Digest, Sha256};

use crate::AuditError;
use crate::chain::{Digest32, digest_hex};

/// Encoding this module currently produces.
pub const CHECKPOINT_VERSION: u16 = 1;

/// Domain separator for a checkpoint hash.
///
/// Distinct from the record, genesis, and tree separators so a digest computed
/// for one purpose can never be replayed as a digest for another.
pub const CHECKPOINT_DOMAIN: &[u8] = b"record-store/audit-checkpoint/v1";

/// Domain separator for the value the checkpoint chain starts from.
///
/// Derived rather than a constant of zeroes, for the same reason
/// [`crate::chain::genesis_hash`] is: a chain beginning at zeroes could be
/// confused with one that happens to contain a zero hash, and two deployments
/// would start identically even for different purposes.
const CHECKPOINT_GENESIS_DOMAIN: &[u8] = b"record-store/audit-checkpoint-genesis/v1";

/// Returns the value the first checkpoint links back to.
#[must_use]
pub fn genesis_hash() -> Digest32 {
    Sha256::digest(CHECKPOINT_GENESIS_DOMAIN).into()
}

/// One checkpoint over a contiguous range of the audit log.
///
/// Constructed through [`Checkpoint::new`] so that it cannot exist in a state
/// that would be hashed and written: a range covering no records, or a leaf
/// count that disagrees with the range it claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    /// Position of this checkpoint in the checkpoint chain.
    pub sequence: u64,
    /// First audit sequence covered, inclusive.
    pub from_sequence: u64,
    /// Last audit sequence covered, inclusive.
    pub to_sequence: u64,
    /// How many leaves the tree was built from.
    pub leaf_count: u64,
    /// Merkle root over every covered record.
    #[serde(with = "digest_hex")]
    pub root: Digest32,
    /// Hash of the preceding checkpoint, or [`genesis_hash`] for the first.
    #[serde(with = "digest_hex")]
    pub previous_checkpoint_hash: Digest32,
}

impl Checkpoint {
    /// Returns a checkpoint over `from_sequence..=to_sequence`.
    ///
    /// Refuses an empty range. A checkpoint covering no records has no Merkle
    /// root — [`crate::merkle::root`] returns `None` for an empty range — so
    /// one is never written; a deployment with nothing new to checkpoint writes
    /// no checkpoint rather than an empty one.
    ///
    /// Refuses a leaf count that disagrees with the range, because the two
    /// disagreeing is exactly what a dropped record looks like.
    pub fn new(
        sequence: u64,
        from_sequence: u64,
        to_sequence: u64,
        leaf_count: u64,
        root: Digest32,
        previous_checkpoint_hash: Digest32,
    ) -> Result<Self, AuditError> {
        if leaf_count == 0 || to_sequence < from_sequence {
            return Err(AuditError::EmptyCheckpoint);
        }
        let covered = to_sequence - from_sequence + 1;
        if covered != leaf_count {
            return Err(AuditError::CheckpointLeafCount {
                from: from_sequence,
                to: to_sequence,
                covered,
                leaf_count,
            });
        }
        Ok(Self {
            sequence,
            from_sequence,
            to_sequence,
            leaf_count,
            root,
            previous_checkpoint_hash,
        })
    }

    /// Returns the bytes the checkpoint hash is computed over.
    ///
    /// Every field is fixed width and big-endian, so no length prefixes are
    /// needed and no two field layouts can produce the same bytes.
    #[must_use]
    pub fn preimage(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CHECKPOINT_DOMAIN.len() + 2 + 32 + 64);
        out.extend_from_slice(CHECKPOINT_DOMAIN);
        out.extend_from_slice(&CHECKPOINT_VERSION.to_be_bytes());
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.from_sequence.to_be_bytes());
        out.extend_from_slice(&self.to_sequence.to_be_bytes());
        out.extend_from_slice(&self.leaf_count.to_be_bytes());
        out.extend_from_slice(&self.root);
        out.extend_from_slice(&self.previous_checkpoint_hash);
        out
    }

    /// Returns this checkpoint's hash, which the next one links back to.
    #[must_use]
    pub fn checkpoint_hash(&self) -> Digest32 {
        Sha256::digest(self.preimage()).into()
    }

    /// Returns whether `sequence` falls inside the range this checkpoint covers.
    #[must_use]
    pub const fn covers(&self, sequence: u64) -> bool {
        self.from_sequence <= sequence && sequence <= self.to_sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint() -> Checkpoint {
        Checkpoint::new(7, 100, 140, 41, [0x11; 32], [0x22; 32]).expect("a consistent checkpoint")
    }

    /// The preimage is a wire format. Pinned as bytes, derived by reading the
    /// field table rather than by printing what this module produced.
    #[test]
    fn version_one_hashes_over_pinned_bytes() {
        assert_eq!(
            hex::encode(checkpoint().preimage()),
            concat!(
                "7265636f72642d73746f72652f61756469742d636865636b706f696e742f7631",
                "0001",
                "0000000000000007",
                "0000000000000064",
                "000000000000008c",
                "0000000000000029",
                "1111111111111111111111111111111111111111111111111111111111111111",
                "2222222222222222222222222222222222222222222222222222222222222222",
            )
        );
        assert_eq!(
            hex::encode(checkpoint().checkpoint_hash()),
            "b54282c04897373b5897283c79f910fcdc6f8aefa936a6178c02328365ddd431"
        );
    }

    /// The reason the field exists. Two checkpoints identical but for how many
    /// leaves they were built from must not hash the same, or the count could
    /// be changed after the fact without breaking the chain.
    #[test]
    fn the_leaf_count_reaches_the_hash() {
        let base = checkpoint();
        let mut fewer = base;
        fewer.leaf_count = 40;
        assert_ne!(base.checkpoint_hash(), fewer.checkpoint_hash());
    }

    /// Fixed-width fields in a declared order means a value cannot be moved
    /// from one field to another and hash the same.
    #[test]
    fn the_fields_cannot_be_swapped_between_positions() {
        let base = checkpoint();
        let mut swapped = base;
        swapped.to_sequence = base.leaf_count;
        swapped.leaf_count = base.to_sequence;
        assert_ne!(base.checkpoint_hash(), swapped.checkpoint_hash());

        let mut linked_elsewhere = base;
        linked_elsewhere.previous_checkpoint_hash = base.root;
        linked_elsewhere.root = base.previous_checkpoint_hash;
        assert_ne!(base.checkpoint_hash(), linked_elsewhere.checkpoint_hash());
    }

    /// Every field has to reach the bytes, or it is a field an editor can
    /// change without breaking anything.
    #[test]
    fn changing_any_single_field_changes_the_hash() {
        let base = checkpoint().checkpoint_hash();

        let mut moved = checkpoint();
        moved.sequence = 8;
        assert_ne!(moved.checkpoint_hash(), base, "sequence");

        let mut shifted = checkpoint();
        shifted.from_sequence = 101;
        assert_ne!(shifted.checkpoint_hash(), base, "from_sequence");

        let mut extended = checkpoint();
        extended.to_sequence = 141;
        assert_ne!(extended.checkpoint_hash(), base, "to_sequence");

        let mut recounted = checkpoint();
        recounted.leaf_count = 42;
        assert_ne!(recounted.checkpoint_hash(), base, "leaf_count");

        let mut rerooted = checkpoint();
        rerooted.root[0] ^= 0xff;
        assert_ne!(rerooted.checkpoint_hash(), base, "root");

        let mut respliced = checkpoint();
        respliced.previous_checkpoint_hash[0] ^= 0xff;
        assert_ne!(
            respliced.checkpoint_hash(),
            base,
            "previous_checkpoint_hash"
        );
    }

    /// The zero-leaf case, decided rather than left to whatever falls out: a
    /// checkpoint covering no records is refused, so none is ever written.
    #[test]
    fn a_checkpoint_over_no_records_is_refused() {
        assert!(matches!(
            Checkpoint::new(0, 100, 99, 0, [0; 32], genesis_hash()),
            Err(AuditError::EmptyCheckpoint)
        ));
        assert!(matches!(
            Checkpoint::new(0, 100, 100, 0, [0; 32], genesis_hash()),
            Err(AuditError::EmptyCheckpoint)
        ));
        // And the tree agrees: an empty range has no root to put in one.
        assert!(crate::merkle::root(&[]).is_none());
    }

    /// A leaf count that disagrees with the range is what a dropped record
    /// looks like, so it is refused at construction rather than written and
    /// left for a verifier to notice.
    #[test]
    fn a_leaf_count_that_disagrees_with_the_range_is_refused() {
        assert!(matches!(
            Checkpoint::new(7, 100, 140, 40, [0x11; 32], [0x22; 32]),
            Err(AuditError::CheckpointLeafCount {
                from: 100,
                to: 140,
                covered: 41,
                leaf_count: 40,
            })
        ));
        assert!(Checkpoint::new(7, 100, 100, 1, [0x11; 32], [0x22; 32]).is_ok());
    }

    /// The genesis value is a constant of this format, pinned like any other
    /// part of the wire format.
    #[test]
    fn the_checkpoint_genesis_hash_is_pinned_and_is_not_zero() {
        assert_eq!(
            hex::encode(genesis_hash()),
            hex::encode(Sha256::digest(b"record-store/audit-checkpoint-genesis/v1"))
        );
        assert_ne!(genesis_hash(), [0_u8; 32]);
        // And it is not the record chain's genesis, which starts a different
        // chain and must not be interchangeable with this one.
        assert_ne!(genesis_hash(), crate::chain::genesis_hash());
    }

    #[test]
    fn a_checkpoint_covers_exactly_its_range() {
        let checkpoint = checkpoint();
        assert!(!checkpoint.covers(99));
        assert!(checkpoint.covers(100));
        assert!(checkpoint.covers(140));
        assert!(!checkpoint.covers(141));
    }

    #[test]
    fn a_checkpoint_round_trips_through_serialization() {
        let checkpoint = checkpoint();
        let encoded = serde_json::to_vec(&checkpoint).expect("encode");
        let decoded: Checkpoint = serde_json::from_slice(&encoded).expect("decode");
        assert_eq!(decoded, checkpoint);
    }
}
