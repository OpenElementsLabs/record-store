# Durability

Durability is the question "if this write returned success, what have I actually been
promised?" Record Store answers it differently depending on how you run it.

## Modes

| Profile | Where | Redundancy | Survives |
| --- | --- | --- | --- |
| `Single` | Standalone default | One local copy | Nothing beyond what your disk survives |
| `Replicated` | Cluster | N full copies on N nodes | Losing nodes, up to the policy |

!!! info "Erasure coding is not implemented"
    The domain model reserves an erasure-coded profile and the repository contains an
    unused `record-store-erasure` crate, but no code path produces or reads erasure
    stripes. Cluster durability today is replication. Do not plan capacity around
    erasure coding.

## What a successful write means

**Standalone.** The payload was streamed to a temporary file, checksummed, `fsync`ed,
and atomically renamed into place, and metadata was published afterwards. The write
survives process crash and power loss to the extent your filesystem and disk honour
`fsync`. It does not survive losing the disk.

**Cluster.** The ingress node fans one bounded stream out to the selected replica
nodes. Each destination independently verifies the bytes it received against the
expected size and checksum, and publishes them durably on its own storage. Metadata
and replica placement become visible **atomically, and only after the acknowledgement
policy has been satisfied**.

That last point is the whole guarantee: an object is not visible until the configured
number of replicas hold verified bytes. An ingress node holding the bytes is not
durability.

## Acknowledgement policy

The cluster's write-acknowledgement policy decides how many replicas must confirm
before a write is acknowledged:

| Policy | Meaning |
| --- | --- |
| `Quorum` | A strict majority of the desired replicas must be durable |
| `All` | Every desired replica must be durable |
| `Count(n)` | Exactly `n` replicas must be durable |

The default is `Quorum`. It is part of the replicated cluster configuration rather
than a startup setting, so there is no environment variable or TOML field for it and
no CLI command that changes it today.

Stricter policies cost latency and availability: `All` means one slow node slows every
write, and one unavailable node fails them. `Quorum` tolerates a minority being down
while still refusing to acknowledge a write that only one node holds.

If the policy cannot be satisfied, the write fails with a durability error naming how
many acknowledgements were required and how many succeeded. It is not quietly
downgraded.

## Replication factor

`cluster.replication_factor` is how many copies the cluster wants. It defaults to 3
and must be between 1 and 3; a larger value is rejected at startup. Placement
is deterministic, capacity-aware, storage-class-aware, and
[failure-domain](../cluster/replication.md#failure-domains)-aware.

A payload with fewer healthy replicas than desired is *under-replicated*. The
coordinator notices and queues [repair](../cluster/repair-and-rebalance.md).

## Integrity

Every payload carries a SHA-256 checksum computed while the bytes streamed in. Reads
verify what they return, and
[integrity verification](../operations/integrity-verification.md) can recompute a
whole bucket on demand. A replica that fails verification is recorded as corrupt and
scheduled for repair rather than being served.

## What durability does not cover

!!! danger "Replication is not backup"
    Replication faithfully reproduces whatever you asked for, including a deletion or
    a mistaken overwrite. It protects against hardware failure, not against you.
    Take [backups](../operations/backup-and-restore.md), and consider
    [versioning](versioning.md) so an overwrite is recoverable.
