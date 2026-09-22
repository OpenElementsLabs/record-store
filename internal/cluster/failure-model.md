# Record Store cluster: failure model and guarantees

Internal. Not published. See [`../README.md`](../README.md).

This document states what the cluster assumes, what it tolerates, and what an
acknowledged operation actually promises. Anything not stated here is *not*
guaranteed, and code that appears to provide more than this is not a guarantee
until it appears here with a test behind it.

## 1. Assumptions

These are assumptions, not guarantees. If one is violated, the guarantees in
section 4 do not hold.

| Area | Assumption |
| --- | --- |
| Filesystem durability | `fsync` on a file, and on its containing directory, makes the bytes and the directory entry survive a power loss. Record Store stages payloads, `sync_all`s them, renames, and then `fsync`s the parent directory (`crates/record-store-storage/src/replica.rs`). A filesystem or virtual disk that lies about `fsync` invalidates every durability claim below. |
| Metadata durability | redb's commit is durable before it returns. The consensus log and the replicated state share this property. |
| Clocks | Clocks are used for *leases, heartbeats, expiry and retention windows*, never to order committed state. Ordering comes from the Raft log. Bounded skew degrades liveness (a lease reclaimed early or late); it must never grant authority or bypass retention — see §5. |
| Network | Fair-loss links: messages may be dropped, delayed, duplicated, or reordered, but a link that works repeatedly eventually delivers. Partitions are arbitrary, including asymmetric ones. |
| Network authentication | Internal RPC is authenticated with per-node credentials issued through consensus, over TLS where configured. Peers are **trusted once authenticated**: a node that holds a valid credential is assumed to report its own storage honestly. |
| Operators | Operators can run destructive commands. Destructive commands must require explicit intent and must state the durability cost. |

### Explicitly out of scope

- **Byzantine faults.** An authenticated node that lies about what it stored is
  not tolerated. Record Store verifies checksums *at the receiving node* rather
  than trusting the sender, which catches corruption and truncation but not a
  deliberately malicious peer.
- **Unlimited failure tolerance.** Every guarantee is bounded by the configured
  replication factor and the metadata voter count.
- **Correlated loss beyond the configured failure domain.** See §3.

## 2. Failure classes

Each class is distinct and is handled differently.

| Class | Definition | Expected handling |
| --- | --- | --- |
| **Process crash** | The process stops; local disk survives. | Restart replays the consensus log and reconciles local payloads. No data loss. |
| **Permanent node loss** | Node and its disks are gone forever. | Repair rebuilds replicas elsewhere; the member is removed from the voter set. |
| **Disk loss** | One device is gone; the node survives. | The device is marked `Failed`; its replicas are rebuilt from other holders. |
| **Corrupted payload** | Bytes on disk no longer match the committed checksum. | Detected on read and on verification; the replica is marked `Corrupt` and rebuilt from a validated source. Corrupt bytes are never promoted. |
| **Slow node** | Reachable but late. | Bounded per-chunk deadlines on writes; bounded leases on movement tasks. A slow node fails a write rather than stalling it. |
| **Network partition** | Members cannot all reach each other; possibly asymmetric. | The majority side keeps metadata authority; the minority side refuses authoritative writes. |
| **Loss of metadata quorum** | Fewer than a majority of voters survive. | The cluster is **unavailable for writes**, by design. Recovery is an explicit, operator-driven procedure — never automatic. See §7. |

**Metadata quorum and payload durability are separate.** Metadata quorum is a
majority of Raft voters and decides *what is true*. Payload durability is the
number of independent devices holding the bytes and decides *what is readable*.
A cluster can have metadata quorum and unreadable objects, or readable objects
and no metadata quorum. Neither implies the other, and the code must never treat
one as evidence of the other.

## 3. Failure domains

Protection against **node** failure is not protection against **host, rack, or
site** failure. Replicas are placed across failure domains
(`crates/record-store-cluster/src/placement.rs`); two replicas in the same domain
count as one unit of protection against that domain failing. Placement refuses
to satisfy a durability requirement by stacking replicas in one domain unless the
operator has configured a scope that permits it.

## 4. Guarantees

### 4.1 What an acknowledged write guarantees

A `200`/`201` for `PUT`, a completed multipart upload, or an acknowledged delete
means **both** of the following happened before the response was produced:

1. **Payload durability.** At least `required_acknowledgements` distinct devices
   independently received the bytes, recomputed the SHA-256 over what they wrote,
   matched it against the commitment, `fsync`ed the file, published it by rename,
   and `fsync`ed the directory. The *receiving* node computes the checksum; the
   sender's claim is never sufficient.
2. **Metadata commit.** The object version *and* its replica placement were
   committed as **one atomic Raft entry** (`ClusterWrite::Batch`), replicated to a
   majority of voters and applied to the leader's state machine.

It therefore survives, without operator action:

- the loss of any `replicas - required_acknowledgements` payload holders, and
- the loss of a minority of metadata voters, and
- a crash of any node at any point, including the coordinator, and
- a leader change at any point in the sequence.

It does **not** survive the simultaneous loss of every device holding the payload,
nor the loss of a metadata voter majority. Neither is claimed.

### 4.2 Visibility

An object is never visible before both conditions hold. Payloads written but not
committed are invisible and are collected as orphans only after a grace period.
Metadata is never committed for a payload that did not reach its durability
requirement.

### 4.3 Read consistency

**Linearizable, on every read path.** Reads of buckets, objects, versions and
multipart state go through a read barrier before the local applied state is read
(`ReplicatedMetadataRepository::barrier`). On the leader this is a quorum
leadership check; on a follower it is a read-index obtained from the leader,
followed by waiting for the local state machine to reach that index. There are no
stale follower reads on the object path.

During a partition:

- **Majority side:** reads and writes both work.
- **Minority side:** the barrier cannot be established, so reads and writes both
  fail with an explicit unavailability error. A minority node does **not** serve a
  possibly-stale answer.

Background scans (repair, reconcile, rebalance) read *unbarriered* local state on
purpose — they are advisory and re-run. Because that state can be stale, no
background scan may take a destructive action on the strength of an unbarriered
read. Destructive background actions establish a barrier first.

### 4.4 Authority

- Exactly one coordinator runs at a time: whichever member holds metadata
  leadership. Coordination is **fenced to the leadership term** it started in; a
  former leader's in-flight pass aborts rather than forwarding its decisions to
  the new leader.

  To be precise about what this does and does not prevent: an entry a leader
  appended *while it still held leadership* may still commit afterwards, under
  ordinary Raft rules — the new leader has it in its log and commits it. That is
  consistent, not split-brain. What the fence prevents is the unbounded case: a
  deposed coordinator continuing to make *new* scheduling decisions and having
  them laundered through the node that replaced it.
- Every replica movement carries a **fence token** from its claim. A worker whose
  lease expired and whose task was reassigned cannot commit a result, release a
  source replica, or complete the task.
- Membership changes use openraft's own joint-consensus protocol. Record Store
  never edits the voter set by hand.
- **Redirection costs one hop.** A client write may be forwarded to the leader
  once. The node receiving a forwarded write proposes it locally or refuses,
  naming the leader it knows; it never relays onward. This is what stops leader
  churn from turning redirects into cycles that each spend a request timeout.

### 4.5 Ambiguous outcomes

A client that times out has three distinguishable outcomes, and the code must not
collapse them:

| Outcome | Meaning | Cluster behaviour |
| --- | --- | --- |
| **Unavailable** | Definitely not committed; nothing changed. | Payloads rolled back. Safe to retry. |
| **Failed** | Definitely rejected by application rules. | Payloads rolled back. Retry will fail the same way. |
| **Ambiguous** | The commit may or may not have landed. | Payloads are **not** rolled back. The write is reported as ambiguous; a retry is idempotent because the operation identity is stable. |

Rolling back on an ambiguous outcome is a data-loss bug, not a cleanup: it would
delete the payload of an object that is visible in committed metadata.

## 5. Clocks and retention

Retention and object-lock windows are evaluated against the timestamp carried
**inside the committed command**, which every member applies identically, rather
than against each member's local clock at apply time. A node with a fast clock
cannot expire a locked version early, and a node with a slow clock cannot keep
one past its window on the authoritative path. Leases and heartbeats use local
clocks and therefore only affect liveness.

## 6. Standalone mode

Standalone is unchanged and is not built on any of the above: one process, one
disk, no consensus, no replication. Cluster work must not regress it, and every
change to shared crates is validated against the standalone regression suite.

## 7. Recovery

Recovery is the only thing here a human starts, because the judgement it encodes
— "a majority of the voters is never coming back" — cannot be made from inside a
partition. A cluster that concluded that for itself would promote a minority
every time a switch rebooted.

### 7.1 What the cluster refuses to do on its own

- It never promotes a minority to restore availability.
- It never picks "the newest-looking copy" as authoritative. Log length is not
  authority.
- A node that belongs to a cluster, still holds its data, and has lost its
  consensus state **refuses to start** rather than forming a second cluster
  around that data. A node with `cluster.seeds` configured is exempt: it has
  somewhere to learn the truth from, so rejoining is recovery, not invention.
- A node whose identity file and replicated state name different clusters
  refuses to start rather than serving one under the other's name.
- A node with durable cluster state and no identity file refuses to mint one.

### 7.2 What the supported procedure does

`rs cluster recover` runs **offline, against one stopped survivor**. It rebuilds
the consensus membership around that member and leaves the state machine
untouched: object history, versions, retention and Object Lock, credentials,
placement, and the cluster's own identity all survive. It never merges divergent
state and never invents an entry that was not already committed on that member.

It is refused unless the operator names the cluster the member actually belongs
to, names a member the group actually had, and explicitly acknowledges the loss.
A member that has applied nothing cannot be recovered from: that would produce an
empty cluster wearing the old cluster's name, which is worse than failing because
it looks like it worked.

### 7.3 What it costs, stated rather than implied

- Metadata the lost quorum committed but never replicated to this member is
  **gone**. The report says how many log entries were discarded.
- Object versions with no replica on the survivor are **unreadable** until one of
  their other holders returns. The report counts them.
- The cluster usually comes back **readable but not writable**: one member cannot
  satisfy a policy requiring two acknowledgements, and it refuses rather than
  acknowledging below the policy. Restoring capacity or lowering the policy are
  both explicit operator decisions.
- Recovery rebuilds authority, not the data plane's view of who exists. The lost
  nodes remain recorded as members until an operator retires them.

### 7.4 The mistake that cannot be undone

Recovering two survivors separately produces two clusters holding one
identifier. A single node cannot prevent this, so every recovery stamps a
distinct lineage (`recovery_generation` and `recovery_id`) into the cluster
identity. That makes the split **detectable**. It does not make it repairable.

### 7.5 Interrupted snapshots

A snapshot is an optimization for catching a peer up, not the source of truth: a
member restarts from its state machine. A snapshot that was interrupted
mid-publication or mid-transfer is therefore reported and ignored, and a new one
is built — rather than stopping the member, which would turn a half-written file
into a node that cannot start.
