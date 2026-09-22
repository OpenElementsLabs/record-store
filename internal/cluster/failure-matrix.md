# Cluster failure matrix

Internal. Not published. See [`../README.md`](../README.md).

Every invariant in [`failure-model.md`](failure-model.md) is listed here with the
test that reproduces it. An invariant with no test is a claim, not a guarantee,
and is marked as such.

Run the cluster-relevant suites with:

```
cargo test -p record-store-cluster -p record-store-consensus \
           -p record-store-replication -p record-store-metadata
```

## 1. Replication correctness

| Invariant | Test | Location |
| --- | --- | --- |
| A write is acknowledged only when the required number of replicas independently verified what they stored | `exactly_the_required_acknowledgement_count_succeeds`, `one_acknowledgement_short_of_the_requirement_fails` | `record-store-replication/tests/replication.rs` |
| An ingress node holding the bytes is not durability | `a_write_that_only_reached_the_local_node_fails` | same |
| A replica reporting a checksum for different bytes is not counted | `a_replica_reporting_a_mismatched_checksum_is_not_durable` | same |
| A replica lost mid-stream is not counted | `a_replica_lost_partway_through_the_stream_is_not_durable` | same |
| A stalled replica is dropped on a deadline, not waited on | `a_replica_whose_call_times_out_is_not_durable`, `a_timed_out_replica_does_not_hold_up_a_satisfied_write` | same |
| A cluster too small for the factor refuses the write rather than weakening it | `a_cluster_too_small_for_the_replication_factor_refuses_the_write` | same |
| A retried transfer reuses one operation identity and does not duplicate a replica | `a_retried_write_reuses_one_operation_identity_per_object` | same |
| A commit that provably failed releases its payload | `a_commit_that_definitely_failed_releases_the_payload` | `record-store-replication/src/store.rs` |
| **An ambiguous commit that actually landed keeps its payload** | `an_ambiguous_commit_that_actually_landed_keeps_its_payload` | same |
| A barrier that disproves the commit releases the payload | `an_ambiguous_commit_that_the_barrier_disproves_releases_the_payload` | same |
| An unresolvable outcome keeps the payload and reports itself as ambiguous | `an_unresolvable_commit_keeps_its_payload_and_says_so` | same |
| Only failures that precede the proposal are treated as definite | `only_failures_that_precede_the_proposal_are_treated_as_definite` | `record-store-consensus/src/consensus.rs` |
| Object version and replica placement commit atomically | `a_single_command_batch_is_transparent_to_its_caller`, `a_rejected_batch_surfaces_the_rejection_rather_than_an_empty_result` | `record-store-consensus/src/command.rs` |
| Identical command sequences produce identical durable state | `applying_the_same_commands_produces_identical_state`, `journalling_commands_are_deterministic_across_members` | `record-store-metadata/src/commands.rs` |

## 2. Failover and fencing

| Invariant | Test | Location |
| --- | --- | --- |
| A new leader is elected when the leader is lost | `a_new_leader_is_elected_when_the_leader_is_killed` | `record-store-consensus/tests/consensus.rs` |
| Followers forward ordinary writes to the leader | `followers_forward_writes_to_the_leader` | same |
| **A leader-fenced write is refused on a follower rather than forwarded** | `a_follower_cannot_commit_a_leader_fenced_write_by_forwarding_it` | same |
| **A write fenced to a superseded term is refused** | `a_write_fenced_to_a_stale_term_is_refused_by_the_current_leader` | same |
| A read barrier makes a leader commit visible to a follower on the first read | `a_read_barrier_makes_a_leader_commit_visible_to_a_follower_immediately` | same |
| **A forwarded write is never forwarded again, so redirects cannot cycle** | `a_forwarded_write_is_never_forwarded_a_second_time` | same |
| Snapshots compact the log and transfer to a new member | `snapshots_compact_the_log_and_transfer_to_a_new_member` | same |
| **A member restored from a snapshot still enforces retention** | `a_member_restored_from_a_snapshot_still_enforces_retention` | `record-store-metadata/tests/object_lock.rs` |
| **A member restored from a snapshot inherits the clock high-water mark** | `a_member_restored_from_a_snapshot_inherits_the_clock_high_water_mark` | same |
| Readiness is separate from liveness | `a_healthy_node_reports_itself_ready`, `a_drained_node_is_reported_as_degraded_rather_than_ready`, `a_failed_background_task_degrades_readiness` | `record-store-replication/tests/replication.rs` |

## 3. Network partitions

| Invariant | Test | Location |
| --- | --- | --- |
| A minority partition cannot accept writes | `a_minority_partition_cannot_accept_writes` | `record-store-consensus/tests/consensus.rs` |
| A node that goes silent is marked suspect, then unreachable | `a_silent_node_is_marked_suspect_and_then_unreachable` | `record-store-replication/tests/replication.rs` |
| A node that reports in again recovers | `a_node_that_reports_in_again_recovers` | same |
| **A member that cannot confirm current cluster state deletes nothing** | `reconciliation_deletes_nothing_while_it_cannot_confirm_cluster_state` | `record-store-replication/src/store.rs` |
| A confirmed orphan past its grace period is still collected | `reconciliation_collects_a_confirmed_orphan` | same |
| A returning node does not resurrect a deleted payload | `a_tombstone_survives_until_every_holder_acknowledges_it` | `record-store-cluster/src/catalog/commands.rs` |
| **Token expiry is decided on the committed timestamp, not a local clock** | `an_expired_token_is_refused_on_the_committed_timestamp` | same |
| Clock rollback beyond tolerance is refused | `a_clock_behind_the_high_water_mark_refuses_to_release_a_retained_version`, `ordinary_clock_drift_inside_the_tolerance_is_absorbed`, `the_clock_mark_survives_a_restart` | `record-store-metadata/tests/object_lock.rs` |

## 4. Repair and reconciliation

| Invariant | Test | Location |
| --- | --- | --- |
| A damaged replica becomes scheduled repair work | `a_damaged_replica_becomes_scheduled_repair_work` | `record-store-replication/tests/replication.rs` |
| An under-replicated payload is scheduled and repaired | `an_under_replicated_write_queues_and_completes_an_executable_repair`, `an_under_replicated_payload_is_scheduled_for_repair` | same |
| Repeated passes do not duplicate repair work | `repeated_coordination_passes_do_not_duplicate_repair_work`, `identical_task_requests_do_not_duplicate_work` | same / cluster catalog |
| Only healthy replicas are offered as repair sources | `only_healthy_replicas_are_offered_as_repair_sources` | `record-store-replication/src/tasks.rs` |
| Corrupt bytes are never served or promoted | `a_read_never_serves_bytes_that_fail_their_checksum`, `a_replica_that_is_corrupt_at_open_time_is_skipped_for_a_healthy_one` | `record-store-replication/tests/replication.rs` |
| A read with no usable replica fails rather than returning partial content | `a_read_with_no_usable_replica_fails_rather_than_returning_partial_content` | same |
| An expired movement lease returns its task to the queue | `an_expired_movement_lease_returns_its_task_to_the_queue` | same |
| **A worker whose lease was reclaimed cannot report an outcome** | `a_worker_whose_lease_was_reclaimed_cannot_report_an_outcome` | `record-store-cluster/src/catalog/commands.rs` |
| **A task predating fencing is not trusted on its absent token** | `a_task_from_before_fencing_is_not_trusted_on_its_absent_token` | same |
| Raising the desired replica count creates repair work | `raising_the_desired_replica_count_creates_repair_work` | `record-store-replication/tests/replication.rs` |
| **Repair backlog age, failure reason, and unrepairable count are exposed** | `repair_status_reports_backlog_age_and_why_work_is_failing` | same |

## 5. Safe node removal

| Invariant | Test | Location |
| --- | --- | --- |
| Removal reports the durability it would cost | `decommissioning_reports_the_durability_it_would_cost`, `decommission_safety_notices_when_durability_would_drop` | `record-store-replication/tests/replication.rs` |
| Unsafe removal is refused without an explicit force | `decommissioning_reports_the_durability_it_would_cost` | same |
| A drain reports what remains and can be resumed | `progressing_a_drain_reports_what_remains`, `a_node_can_be_drained_put_in_maintenance_and_resumed` | same |
| Lifecycle operations on an unknown node are refused | `lifecycle_operations_on_an_unknown_node_are_refused` | same |
| **A decommissioned node cannot rejoin on its old identity** | `a_decommissioned_node_cannot_rejoin_on_its_old_identity` | same |
| The last metadata voter is never removed | `MetadataConsensus::demote_member` refuses the change; `Coordinator::retire_membership` surfaces the refusal and keeps the operation outstanding. **No dedicated test** — see [`limitations.md`](limitations.md) | `record-store-consensus/src/consensus.rs`, `record-store-replication/src/coordinator.rs` |
| **A single-use join token cannot admit two nodes** | `a_single_use_token_cannot_be_consumed_twice_even_when_both_requests_are_valid` | `record-store-cluster/src/catalog/commands.rs` |
| A revoked token is refused by the state machine | `a_revoked_token_is_refused_by_the_state_machine` | same |
| A used token cannot be replayed over the wire | `a_join_token_cannot_be_replayed` | `record-store-replication/tests/replication.rs` |

## 6. Recovery

| Invariant | Test | Location |
| --- | --- | --- |
| A node that already belongs to another cluster is refused | `binding_is_idempotent_and_refuses_a_foreign_cluster`, `initialization_is_idempotent_and_refuses_a_foreign_cluster` | `record-store-cluster/src/identity.rs`, catalog |
| Member reassignment is refused | `member_reassignment_is_refused` | `record-store-cluster/src/identity.rs` |
| A snapshot round-trips the whole catalog | `snapshot_export_and_import_restore_the_catalog`, `snapshot_export_and_import_round_trip` | `record-store-metadata/src/snapshot.rs`, cluster catalog |
| A snapshot carries retention, events, and the clock mark | see §2 snapshot rows | `record-store-metadata/tests/object_lock.rs` |
| Reconciliation of a consistent node changes nothing | `reconciliation_of_a_consistent_node_changes_nothing` | `record-store-replication/tests/replication.rs` |
| An incompatible storage format is refused by name, and a v4 directory migrates and serves every object unchanged | `a_schema_four_directory_starts_migrates_and_serves_every_object_unchanged` | `record-store-metadata/tests/migration_v4.rs` |

## Not yet covered by a test

These are stated limitations, not guarantees. They are tracked in
[`limitations.md`](limitations.md) and gate the release.

| Gap | Why it is not covered |
| --- | --- |
| Asymmetric partitions (A sees B, B does not see A) | The consensus test harness models symmetric reachability only. |
| Disk-full and slow-storage behaviour under load | No fault-injecting filesystem in the test harness. |
| Multi-process, multi-host cluster runs | Every cluster test is in-process; the RPC surface is exercised over a real listener but on one host. |
| Concurrent-history linearizability checking | No invocation/completion history recorder exists yet. |
| Prolonged outage and repair workloads with bounded resource use | No long-running soak harness. |
| Interrupted drain with permanent loss of the draining node | Requires multi-process orchestration. |
