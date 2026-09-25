# Cluster release gates

Internal. Not published. See [`../README.md`](../README.md).

Clustering ships when these are met — not when the code exists and not when the
unit suite is green. A green suite over in-process tests says the logic is
self-consistent; it says nothing about machines, disks, and networks failing.

Each gate is either met, partly met, or not met. Nothing here is aspirational:
if a gate is not met, the corresponding public claim is not made.

The executable form is [`gates.toml`](gates.toml), evaluated with
`tests/gates/evaluate.py --matrix internal/cluster/gates.toml`. CL-SUITES runs
the in-process suites; CL-EVIDENCE fails if any test cited in
[`failure-matrix.md`](failure-matrix.md), this file or
[`test-evidence.md`](test-evidence.md) is missing or did not pass in the same
candidate's run; G6, G7, G9 and G10 are listed as gates whose command exits
"skipped", so the cluster decision reads BLOCKED until they exist. G11 is
enforced by the standalone matrix, which runs for every shared-crate change.

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

## G8. Recovery paths are tested against isolated clusters — **met**

Each supported failure class now has a procedure and a test against an isolated
cluster:

| Class | Procedure | Evidence |
| --- | --- | --- |
| Ordinary restart | restart the process | `an_ordinary_restart_resumes_the_same_cluster` |
| Node recovers from durable local state | restart; reconcile against committed placement | `a_single_member_group_commits_and_survives_restart`, `reconciliation_of_a_consistent_node_changes_nothing` |
| Permanently lost node replaced | drain/force-decommission, admit a clean node | R5; `a_decommissioned_node_cannot_rejoin_on_its_old_identity` |
| Interrupted snapshot | ignored, reported, member still starts | `an_interrupted_snapshot_is_reported_rather_than_hidden` |
| Lost metadata quorum, payloads survive | `rs cluster recover` against one survivor | `a_cluster_recovered_from_one_survivor_still_serves_its_verified_objects` |
| Second cluster formed by accident | refused at startup | `a_survivor_whose_metadata_state_is_gone_refuses_to_form_a_second_cluster` |
| Identity and state disagree | refused at startup | `a_node_whose_identity_and_state_disagree_refuses_to_pick_one` |
| Identity lost | refused at startup | `a_node_that_lost_its_identity_file_refuses_to_adopt_its_own_data` |
| Unsafe recovery attempted | refused with a specific reason | `unsafe_recovery_attempts_are_refused_with_a_specific_reason`, `a_member_that_applied_nothing_cannot_be_recovered_from` |

The end-to-end case verifies **object bytes**, not a status code, and confirms
the recovered cluster keeps its identity, advances its recovery lineage, and
refuses writes it cannot make durable rather than silently weakening the policy.

Recovery never selects "the newest-looking copy" on its own: `inspect-state`
reports each survivor's applied index and the operator chooses, and the procedure
refuses a cluster it does not belong to, a member the group never had, and an
operator who has not acknowledged the loss.

The residual gaps are in [`limitations.md`](limitations.md) L2 and are
**significant rather than blocking**: two survivors recovered separately, no
surviving member at all, and cluster-wide consistent backup.

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

**Met:** G1, G2, G3, G4, G5, G8, G11, G12.
**Partly met:** G6, G7.
**Not met:** G9, G10.

**G9 and G10 are release blockers.** Clustering stays internal until they are
met. G8 is met for the supported failure classes; the scenarios it does not
cover are recorded as limitations rather than claimed.
