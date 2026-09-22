# Cluster release gates

Internal. Not published. See [`../README.md`](../README.md).

Clustering ships when these are met — not when the code exists and not when the
unit suite is green. A green suite over in-process tests says the logic is
self-consistent; it says nothing about machines, disks, and networks failing.

Each gate is either met, partly met, or not met. Nothing here is aspirational:
if a gate is not met, the corresponding public claim is not made.

## G1. The failure model is written down and enforced — **met**

[`failure-model.md`](failure-model.md) states the assumptions, the failure
classes, what an acknowledged write guarantees, the read consistency model, and
what is explicitly out of scope. Every guarantee in it has a test in
[`failure-matrix.md`](failure-matrix.md).

## G2. Visibility and durability agree — **met**

An object is never visible before its payload durability and its atomic metadata
commit are both satisfied, and an ambiguous commit outcome never destroys a
committed object's payload. Tested.

## G3. Authority changes safely — **met**

Leader election, term fencing for the coordinator, and claim fencing for replica
movement are implemented and tested. No two nodes can independently authorize
conflicting committed state through the coordination path.

## G4. Snapshots carry the whole state machine — **met**

Consensus snapshots carry object catalog, cluster catalog, Object Lock state,
the event journal, and the clock high-water mark. The state machine is
deterministic: two members applying one log entry produce byte-identical durable
state. Tested.

## G5. Node removal is safe and reversible until it is not — **met**

Removal evacuates data, verifies replacement replicas, commits placement, and
only then retires consensus membership — and refuses to remove the last voter.
Forced removal reports its deficit and is never labelled safe. A removed node
cannot rejoin on its old identity. Tested.

## G6. Partition behaviour is deliberate — **partly met**

Majority/minority behaviour is implemented and tested: a minority cannot accept
writes, and a member that cannot confirm current cluster state deletes nothing.
**Not met:** asymmetric partitions, intermittent links, and delayed messages from
former leaders have no test coverage ([L3](limitations.md#l3-asymmetric-partitions-are-untested)).

## G7. Repair is trustworthy and bounded — **partly met**

Detection, validated source selection, idempotent and resumable tasks, fencing
against stale workers, and bandwidth/concurrency bounds are implemented and
tested. **Not met:** the bounds have not been measured against a slow or failing
device, and there is no soak evidence
([L5](limitations.md#l5-storage-fault-injection-is-missing),
[L6](limitations.md#l6-no-soak-or-long-outage-testing)).

## G8. Recovery paths are tested against isolated clusters — **not met**

Ordinary restart and single-node recovery from durable local state work and are
tested. **Not met:** replacing a permanently lost node, recovering from
interrupted snapshots and incomplete transfers, and above all recovery when
payloads survive but metadata quorum is lost
([L2](limitations.md#l2-no-tested-recovery-from-loss-of-metadata-quorum)). Until
G8 is met, clustering must not be offered for data anyone would mind losing.

## G9. Multi-process, multi-host evidence exists — **not met**

Every cluster test is in-process ([L1](limitations.md#l1-no-multi-process-or-multi-host-test-coverage)).
This gate requires at minimum: a multi-process cluster started by the real
binary, surviving process kills at each stage of a write, a real network
partition between hosts, and a verified end state — bytes, versions, metadata,
placement, and event state, not HTTP status codes.

## G10. Concurrent histories are checked against the model — **not met**

See [L4](limitations.md#l4-no-concurrent-history-linearizability-checking).

## G11. Standalone is unaffected — **met**

The standalone regression suite passes unchanged after every shared-crate change.
Standalone remains one process, one disk, no consensus, no replication.

## G12. Public documentation makes no cluster claims — **met**

The documentation build (`docs/`, `mkdocs.yml`) contains no clustering feature
documentation, and `docs/getting-started/migrating-from-minio.md` continues to
state plainly that there is no clustering, replication, or erasure coding.
`internal/` is outside `docs_dir` and is never built or published.

---

## Summary

**Met:** G1, G2, G3, G4, G5, G11, G12.
**Partly met:** G6, G7.
**Not met:** G8, G9, G10.

**G8, G9, and G10 are release blockers.** Clustering stays internal until they
are met.
