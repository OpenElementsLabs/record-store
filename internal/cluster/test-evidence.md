# Test evidence

Internal. Not published. See [`../README.md`](../README.md).

Recorded 2026-09-22. Every number here is an **in-process**
measurement on one developer machine. They are useful as regression signals and
as an order-of-magnitude sanity check. They are **not** performance figures for a
deployed cluster, and nothing in [`release-gates.md`](release-gates.md) treats
them as such — see gate G9.

## Environment

| | |
| --- | --- |
| Host | Apple M5 |
| OS | macOS 26.6.2 |
| Toolchain | rustc 1.97.1 (8bab26f4f 2026-07-14) |
| Profile | `dev` (unoptimized, debug assertions on) |

## Suite

```
cargo test --workspace --no-fail-fast
```

| | Before this work | After |
| --- | --- | --- |
| Tests passed | 1048 | **1070** |
| Tests failed | 0 | **0** |
| Clippy warnings | 0 | 0 |
| `cargo fmt --check` | clean | clean |
| `mkdocs build --strict` | clean | clean |

The 22 added tests are the evidence rows marked in bold in
[`failure-matrix.md`](failure-matrix.md).

## Measurements

Wall clock for a single test, which includes process start, consensus group
bootstrap, and teardown. Treat these as upper bounds on the operation itself.

### Failover and consensus

| Scenario | Test | Time |
| --- | --- | --- |
| Leader killed, new leader elected, write accepted | `a_new_leader_is_elected_when_the_leader_is_killed` | 1.69 s |
| Log compacted, snapshot built and installed on a new member | `snapshots_compact_the_log_and_transfer_to_a_new_member` | 1.33 s |
| Minority partition refuses writes for the whole partition window | `a_minority_partition_cannot_accept_writes` | 5.77 s |
| Forwarded write refused rather than relayed | `a_forwarded_write_is_never_forwarded_a_second_time` | 0.60 s |

Election time is governed by the configured election timeout (default
1000–2000 ms). The measured 1.69 s for the whole test is consistent with one
election timeout plus bootstrap, and is the number to watch for regressions.

### Repair and movement

| Scenario | Test | Time |
| --- | --- | --- |
| Under-replicated write detected, repair queued, executed, verified | `an_under_replicated_write_queues_and_completes_an_executable_repair` | 0.51 s |
| Damaged replica detected and scheduled | `a_damaged_replica_becomes_scheduled_repair_work` | 0.34 s |
| Repair backlog age and failure reasons assembled | `repair_status_reports_backlog_age_and_why_work_is_failing` | 0.34 s |

Payloads in these tests are a few kilobytes, so the time is scheduling and commit
overhead, not transfer. Transfer is separately bounded by
`MovementLimits::bytes_per_second` (default 64 MiB/s per movement) and by the
per-device movement concurrency. **Neither bound has been measured against a real
device** — see [`limitations.md`](limitations.md) L5 and L6.

### Removal and recovery

| Scenario | Test | Time |
| --- | --- | --- |
| Decommissioned node refused re-entry, by restart and by fresh token | `a_decommissioned_node_cannot_rejoin_on_its_old_identity` | 0.30 s |
| Commit-outcome resolution and reconciliation safety (6 cases) | `store::tests::*` | 0.63 s |

## What these numbers do not show

- Any behaviour across processes or hosts (G9).
- Any behaviour under a real network partition, as opposed to a modelled one.
- Any behaviour on a slow, full, or failing disk.
- Recovery from loss of metadata quorum, which has no supported procedure.
- Throughput or latency under concurrent load.
