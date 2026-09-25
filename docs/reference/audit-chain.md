# Audit Chain and Checkpoints

The tamper-evident audit log has three layers: a **hash chain** linking records
to one another, a **Merkle tree** over each checkpoint's range, and a
**checkpoint** that names the range, the tree, and the checkpoint before it.

This page is the contract for all three. It is written so that a verifier can
be built from it alone — no server, no network, and no reading of Record
Store's source. That matters because the verifier the format exists for runs
offline, on someone else's machine, with nothing to disagree with it. A wrong
"verified" there is silent and permanent.

Related: [Proof Bundle Format](proof-bundle.md), which carries these structures
to a third party, and [Object Lock and Trust](../security/object-lock.md) for
what the chain does and does not establish.

## What each layer establishes

| | Established |
| --- | --- |
| A record was not edited, removed, or reordered | By the chain, against anyone who cannot rewrite the whole store |
| A record is one of the records a root commits to | By the Merkle tree, given the root |
| The root commits to every record in its range, and no fewer | By the checkpoint's leaf count |
| That state existed at a particular time | Only by an external anchor over the checkpoint |

The chain alone does not detect an operator who can rewrite every record and
every hash. Only the anchor reaches that far.

## Record hashes

All digests are SHA-256 and 32 bytes. All integers are **big-endian**.

Each record commits to its position and to its predecessor:

```text
record_hash = SHA-256(
    "record-store/audit-record/v1"    28 bytes, no length prefix
  ‖ u64  sequence
  ‖ [32] previous_hash
  ‖ canonical_bytes(event) )
```

The sequence number is inside the digest rather than beside it, so a record
cannot be moved to another position and still verify.

The first record's `previous_hash` is the genesis value:

```text
genesis = SHA-256("record-store/audit-genesis/v1")
        = a83f84c628e7c27f38e6b6bf31214b50545c08a20da69fb50c50728ef0f8f0a3
```

Deriving the genesis from its own separator, rather than starting from zeroes,
keeps the first link as distinguishable as every later one: a chain beginning
at zeroes could be confused with one that happens to contain a zero hash, and
two deployments would start identically even for different purposes.

`canonical_bytes(event)` is the versioned encoding in
[`canonical.rs`](https://github.com/OpenElementsLabs/record-store/blob/main/crates/record-store-audit/src/canonical.rs),
pinned byte-for-byte by its own tests. A verifier that is handed `record_hash`
— as a proof bundle hands it — does not need to reproduce it; a verifier
recomputing a record from its event does.

One part of that encoding is worth restating here, because it is the only field
whose wire form is not obvious from the event's JSON: the result is a single octet.

| Result | Octet |
| --- | --- |
| `success` | `0x00` |
| `denied` | `0x01` |
| `failure` | `0x02` |
| `attempted` | `0x03` |

The values are appended, never renumbered. Renumbering one would silently change the
hash of every record already written with it.

## Where the chain begins

Records written before a deployment maintained a chain carry no links at all. They
remain in the log and remain queryable, and they are reported as `unchained` rather
than as verified: they are not evidence of anything, and presenting them as verified
would be manufacturing evidence they never had.

Positions are gapless. A verifier that finds a sequence missing below the head has
found a deletion — the record that would have carried the broken link is the one that
is gone, so the sequence is what catches it rather than the links.

## Merkle construction

The tree is built over the record hashes the checkpoint covers, **in sequence
order**. Its leaves are record hashes, not events.

### The three prefixes

Every hash in the tree is SHA-256 over the domain string
`record-store/audit-merkle/v1` (28 bytes, no length prefix) followed by exactly
one prefix byte:

```text
leaf(record_hash) = SHA-256( "record-store/audit-merkle/v1" ‖ 0x00 ‖ record_hash )
node(left, right) = SHA-256( "record-store/audit-merkle/v1" ‖ 0x01 ‖ left ‖ right )
root(apex)        = SHA-256( "record-store/audit-merkle/v1" ‖ 0x02 ‖ apex )
```

Where each applies:

| Prefix | Applied to | How often |
| --- | --- | --- |
| `0x00` | Each record hash, turning it into a leaf | Once per record |
| `0x01` | Each pair of adjacent nodes at a level | Once per pair, at every level |
| `0x02` | The single node left at the top | Exactly once per tree |

The three are distinct so that none can be presented as another. Without
`0x00`, an interior node could be offered as a record. Without `0x02`, a
one-record tree would root at its own leaf, and anyone holding a record hash
could present it as a root that an empty inclusion path verifies against.

### Building the tree

Start with the leaves, in order. At each level, pair nodes left to right and
replace each pair with its `node(...)`. **A trailing odd node is promoted to
the next level unchanged, not duplicated.** Repeat until one node remains; that
node is the *apex*. The root is `root(apex)`.

Promotion rather than duplication, because hashing an odd node with itself
would make a three-record range produce the same root as a four-record range
whose last entry repeats — two different logs that agree.

### The single-leaf case

A one-record tree has no pairing to do. Its apex is the record's leaf, and the
root prefix is still applied:

```text
root = root(leaf(record_hash))
```

The root is therefore never equal to the leaf, and an empty inclusion path
proves membership only in a tree that really has one leaf.

### The zero-leaf case

**A tree over zero leaves has no root, and a checkpoint covering no records is
never written.** There is no apex to apply the root prefix to, and any constant
chosen to stand in for one would be a value committing to nothing while looking
like it committed to something. A deployment with nothing new to checkpoint
writes no checkpoint. A verifier that encounters a checkpoint whose
`leaf_count` is zero must reject it.

## Inclusion paths

An inclusion path proves one record hash sits under a root without revealing
the other records.

```json
{
  "index": 0,
  "steps": [
    { "side": "right", "hash": "64 hex characters" },
    { "side": "right", "hash": "64 hex characters" }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `index` | The record's position in the checkpoint's range, `sequence - from_sequence`. Zero-based. |
| `steps` | The sibling at each level, **innermost first** — the leaf's sibling comes first, the apex's last. |
| `steps[].side` | Which side the **sibling** is on: `left` means the sibling is the left child and the value being carried up is the right one. |
| `steps[].hash` | The sibling's digest, 32 bytes, lowercase hex. |

In the canonical byte encoding a signature covers, `side` is a single octet:
**`0x00` for `left`, `0x01` for `right`**.

A promoted node has no sibling at its level, so it **contributes no step**. Path
length therefore varies by position, which is what makes the next section
necessary.

### Checking a path

Given the record hash, the path, the root, and the checkpoint's `leaf_count`:

1. Reject if `index >= leaf_count`.
2. Compute the length the path must have (below). **Reject if `steps` is not
   exactly that long.** This is a rejection, not a warning.
3. Fold: `current = leaf(record_hash)`, then for each step in order,

    ```text
    side = left   →  current = node(step.hash, current)
    side = right  →  current = node(current, step.hash)
    ```

4. The result of `root(current)` must equal the checkpoint's root.

### The required path length

Derived from the promotion rule alone, without building a tree:

```text
length(leaf_count, index):
    if index >= leaf_count: reject
    n, p, steps = leaf_count, index, 0
    while n > 1:
        promoted = (p == n - 1) and (n is odd)
        if not promoted: steps += 1
        p = p // 2
        n = ceil(n / 2)
    return steps
```

Step 2 is not belt-and-braces. Because promotion makes length depend on
position, the lengths for one index coincide across different tree sizes —
index 4 takes three steps in both a seven-leaf and an eight-leaf tree, index 0
takes two in both a three-leaf and a four-leaf tree — while index 4 takes one
step in a five-leaf tree and three in an eight-leaf one. A verifier that folds
whatever it is given and compares the result is comparing a digest it has no
reason to believe came from the tree the checkpoint describes.

The check is only worth something because `leaf_count` is **covered by the
checkpoint hash**, and so by whatever signs or anchors it. A leaf count read
from anywhere the signature does not reach establishes nothing.

## Checkpoints

A checkpoint names a contiguous range of the log, the Merkle root over it, and
the checkpoint before it.

| Field | Type | Notes |
| --- | --- | --- |
| `sequence` | u64 | Position in the checkpoint chain, from 0 |
| `from_sequence` | u64 | First audit sequence covered, inclusive |
| `to_sequence` | u64 | Last audit sequence covered, inclusive |
| `leaf_count` | u64 | How many leaves the tree was built from |
| `root` | 32 bytes | Merkle root over the covered records |
| `previous_checkpoint_hash` | 32 bytes | The preceding checkpoint's hash |

`leaf_count` must equal `to_sequence - from_sequence + 1`, and must be at least
1. The two disagreeing is what a record quietly dropped from the tree looks
like from outside: every proof for the records that remain still folds to the
published root, and only the count says that something covered by the
checkpoint is missing from the tree committing to it. A checkpoint is refused
at construction if they disagree, and a verifier reports the disagreement as a
failure.

### Checkpoint hash

```text
checkpoint_hash = SHA-256(
    "record-store/audit-checkpoint/v1"   32 bytes, no length prefix
  ‖ u16  checkpoint version              0x0001
  ‖ u64  sequence
  ‖ u64  from_sequence
  ‖ u64  to_sequence
  ‖ u64  leaf_count
  ‖ [32] root
  ‖ [32] previous_checkpoint_hash )
```

Every field is fixed width and big-endian, so no length prefixes are needed and
no two field layouts can produce the same bytes. The preimage is 130 bytes.

Changing this preimage means a **new checkpoint version**, never an edit to
version 1. Once a checkpoint has been signed and anchored, its bytes are fixed
forever, and a second version costs a second migration.

The first checkpoint links back to:

```text
checkpoint genesis = SHA-256("record-store/audit-checkpoint-genesis/v1")
                   = 7a7d5b27de847a1ca7f054632ee02aa59ede76a9ddb4b564017a9a147fae5a46
```

which is deliberately not the record chain's genesis, so a value from one chain
can never be spliced into the other.

## Known-answer vectors

Committed under
[`crates/record-store-audit/tests/vectors/merkle/`](https://github.com/OpenElementsLabs/record-store/tree/main/crates/record-store-audit/tests/vectors/merkle),
one file per tree size, for **1, 2, 3, 5, 8 and 9 leaves**. Each file carries
the record hashes, the leaf hashes, the root, and the inclusion path for every
index.

The sizes are chosen rather than round: 1 is the lone leaf the root prefix
exists for, 2 is a perfect pair, 3 is the smallest tree with a promotion, 5
promotes a leaf across three levels, 8 is perfect, and 9 is one leaf past
perfect — the size where index 8 rides all the way up and reaches the apex in a
single step.

The inputs are reproducible: `record_hashes[i] = SHA-256("record-{i}")` for `i`
in `0..leaf_count`. An independent implementation should read this page, write
the tree, and land on these exact digests. The vectors were computed by a
separate implementation of the rules above, not printed from the Rust encoder,
so a disagreement means the document and the code have drifted and which is
wrong has to be decided before either is changed.

### Worked example: three leaves

```text
record_hashes[0] = SHA-256("record-0") = b512b2dd10a5444b38811e7eb9487baaadad0e9da59ef29a0c34c24603e349df
record_hashes[1] = SHA-256("record-1") = b7462d6ced2c15add3dbe47755277fa9618aceb17c68f507128896d622d4c5ee
record_hashes[2] = SHA-256("record-2") = 7bd87ca67f07e7904cc69653a6b4b41af5951dff5eabece3ef3553a034c592a4

leaf(0) = acfd05fadf5193b0dac9abca287c0aada5aabbbed6682e306624d76fa2279f28
leaf(1) = 5bc5223b321bf98c9db18bacc93bb56e2e7ec392bdf779ba36909ced33d39656
leaf(2) = f9b5d7f663d488e97c08944a6558a51822bdddfcb641d7afc644c11d43edbce2

level 1: node(leaf(0), leaf(1)) = 3e90be0f97167205612c86d3efea4f18744748204c8d8c8c4dce9d969ed3b0fa
         leaf(2) promoted unchanged
apex:    node(3e90be0f…, leaf(2))  = 5667e34caa150a6a1f4d94e15c74772eb173cd4130f5b241d96ca8329bcb199b
root:    root(5667e34c…)           = 47fa39c6153969ee2a062be28a9768fcc9db8418eb5d04a8b04a0adaff362c36
```

Paths, with the lengths the rule above predicts:

| Index | Length | Steps |
| --- | --- | --- |
| 0 | 2 | `right leaf(1)`, `right leaf(2)` |
| 1 | 2 | `left leaf(0)`, `right leaf(2)` |
| 2 | 1 | `left 3e90be0f…` — leaf 2 was promoted at level 1, so that level costs no step |

And the one-leaf tree, where the root prefix is the whole difference:

```text
leaf(0) = acfd05fadf5193b0dac9abca287c0aada5aabbbed6682e306624d76fa2279f28
root    = e2d68c42b513a4fe6f404bbaa7837b0862b51f56c7907786e1185587544f6a1c
```

## Dependency decisions

Recorded here with their removal conditions, in the same spirit as the
`cargo audit --deny warnings` policy described in the
[README](https://github.com/OpenElementsLabs/record-store/blob/main/README.md#build-and-test).

### `der 0.7` and `cms 0.2` for RFC 3161 anchoring

**Decided 2026-09-19**, ahead of the parser, because the choice shapes what the
parser may assume. Reading an RFC 3161 timestamp token needs an ASN.1 stack.
`cms 0.3` exists only as `0.3.0-pre.2`, and a pre-release is not acceptable in
a project that builds with `--deny warnings` and audits with `cargo audit
--deny warnings`: it makes no compatibility promise and can be yanked or re-cut
under the same version. So `der 0.7` and `cms 0.2` are the choice.

**Remove this decision when `cms 0.3` reaches a stable release**, and move to it
then.

Neither crate is in `Cargo.lock` yet — they arrive with the anchoring work.
When they do, they bring a duplicate `const-oid`: `0.9.6` through `der 0.7`
alongside the `0.10.2` already in the tree through `digest 0.11`. That was
weighed and accepted. There is no advisory against either version, and `rsa` —
the crate that would otherwise make an ASN.1 stack an audit problem — stays out
of the tree. The duplication is a build cost and a type-confusion hazard, not a
security finding.

### The SHA-256 identifier is defined locally

The two `const-oid` versions above give two unrelated `ObjectIdentifier` types.
Comparing one with the other does not fail cleanly; it fails as a type error in
the middle of `TSTInfo` parsing, or gets worked around by converting through a
string. So SHA-256's identifier is defined in
[`anchor.rs`](https://github.com/OpenElementsLabs/record-store/blob/main/crates/record-store-proof/src/anchor.rs)
as bytes belonging to neither crate:

```text
dotted   2.16.840.1.101.3.4.2.1                  (RFC 5754)
content  60 86 48 01 65 03 04 02 01              (9 octets)
DER      06 09 60 86 48 01 65 03 04 02 01        (tag, length, content)
```

A test derives the content octets from the dotted arcs per X.690 clause 8.19,
so the constant is checked against the encoding rule rather than against
another copy of itself. That is the stronger check of the two available: the
encoding rule is what *both* `const-oid` versions must satisfy, and it does not
go stale when one of them moves. When `der` and `cms` land, add an assertion
against each version's own value as well — until then there is only one in the
tree, and pulling in the other purely to compare it would be adding a
dependency to test a constant.
