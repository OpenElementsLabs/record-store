# Cluster runbooks

Internal. Not published. See [`../README.md`](../README.md).

These procedures assume the failure model in [`failure-model.md`](failure-model.md).
Run them against an isolated cluster first. Several of them are irreversible and
say so.

Throughout: `rs` is the `record-store` CLI, `MGMT` is a management endpoint on a
**reachable member**, and "the leader" is whichever member `rs cluster status`
reports as holding metadata leadership.

---

## R1. Read the cluster's actual state

Always start here. Most incidents are misread before they are mishandled.

```
rs cluster status --endpoint $MGMT
```

Read, in this order:

1. **Metadata quorum.** `status.writable` false means the cluster has no metadata
   authority. Nothing below will work until that is fixed — go to [R6](#r6-lost-metadata-quorum).
2. **Voter count and reachability.** Only the leader observes every peer, so a
   follower reports unknown rather than unreachable. If you are reading from a
   follower, re-read from the leader before concluding a peer is down.
3. **Node states.** `Joining`, `Healthy`, `Suspect`, `Unreachable`, `Draining`,
   `Maintenance`, `Decommissioned`. `Suspect` and `Unreachable` are *suspicion*,
   not a decision — the cluster has not given up on the node's data.
4. **Durability counters and repair backlog.** Under-replicated payload count,
   repair queue depth and age, and parked tasks.

A node being unreachable and a payload being at risk are different facts. Do not
act on the first as if it were the second.

---

## R2. Leader loss and failover

**Expected behaviour.** Election completes within the configured election
timeout (default 1–2 s). During the gap, writes and linearizable reads fail with
an explicit unavailability error. They do not hang indefinitely and they do not
silently return stale data.

**Procedure.**

1. Confirm a leader exists: `rs cluster status` on any reachable member.
2. If no leader after several election timeouts, count reachable voters. Fewer
   than a majority means the group cannot elect — this is [R6](#r6-lost-metadata-quorum),
   not a failover.
3. If a leader exists, nothing needs doing. Background work resumes on the new
   leader's next coordination pass; in-flight passes on the old leader abort
   rather than forwarding their decisions.

**What is safe during a leader change.** In-flight uploads either commit or fail;
they cannot half-commit. A client that times out gets an explicit ambiguous
outcome — re-read the object rather than assuming either result.

**What to check afterwards.** Repair backlog age. A coordination pass that
aborted mid-scan is re-derived from replicated state, not lost, but the first
pass on a new leader restarts its incremental scan cursor.

---

## R3. Repair a degraded payload

**When.** `rs cluster status` reports under-replicated or damaged payloads.

Repair is automatic. This procedure is for when it is not progressing.

1. **Is there a valid source?** A payload with zero healthy replicas cannot be
   repaired and is reported, not retried in a loop. It stays unreadable until a
   holder returns. If no holder will ever return, the payload is lost — say so
   explicitly rather than waiting.
2. **Is there an eligible destination?** Repair refuses to place a replica that
   would violate the failure-domain constraint or land on a full device. Check
   free capacity and failure-domain spread. Adding capacity is the fix; weakening
   the policy is not.
3. **Are tasks parked?** A task that exhausted its attempt budget parks with its
   last error. Read the error before requeueing — a parked task usually means a
   real problem (corrupt source, unreachable destination, full disk), and
   requeueing without fixing it just spends another budget.
4. **Throughput.** Repair is bandwidth- and concurrency-bounded per device so it
   cannot starve foreground traffic. If repair is too slow for the risk, raise
   the movement budget deliberately and watch foreground latency.

**Never** delete a replica to "clean up" a repair. Corrupt replicas are already
excluded from being sources; they are rebuilt in place.

---

## R4. Remove a node safely

Data evacuation and consensus membership are separate jobs and are done in that
order.

```
rs cluster decommission-safety --node $NODE --endpoint $MGMT   # 1. check
rs cluster drain --node $NODE --endpoint $MGMT                 # 2. evacuate
rs cluster status --endpoint $MGMT                             # 3. watch
rs cluster decommission --node $NODE --endpoint $MGMT          # 4. retire
```

1. **Check first.** `decommission-safety` reports how many object versions would
   fall below required durability and how many would become unreadable. A safe
   removal reports zero of both.
2. **Drain.** The node stops receiving new placement and its replicas are copied
   elsewhere. Progress is observable and the drain is resumable — a leader change
   or a restart of the draining node does not restart it.
3. **Wait for zero.** Do not proceed while replicas remain. The decommission path
   will drain first anyway, but doing it explicitly keeps the two steps separable
   when something goes wrong.
4. **Decommission.** When no replicas remain, the node is marked
   `Decommissioned` **and removed from the metadata consensus group**. These
   happen together: a node retired from the data plane but left in the voter set
   keeps counting toward every quorum.

**If the membership change fails**, the operation deliberately stays outstanding
and the next coordination pass retries it. Do not mark it done by hand.

**The last voter is never removed.** The attempt is refused with a specific
reason. A cluster with no metadata authority cannot be recovered by consensus.

**Cancelling.** `rs cluster resume --node $NODE` returns a draining node to
service and cancels the drain. Replicas already moved stay where they are; that
is not a loss, only a rebalance opportunity. Once a node is `Decommissioned`,
the transition is **irreversible** — resume is refused.

**After removal**, the node cannot rejoin on its old identity. It must be given
a new node identity and a fresh join token. This is deliberate: its local state
no longer reflects any placement the cluster believes in.

### Forced removal

`--force` bypasses only the durability objection, never the data movement. It
still drains. Use it when the node is already gone and its data is already lost;
the reported deficit is the truth about what you are accepting. It is never
"safe" and is not labelled as such.

---

## R5. Replace a permanently lost node

The lost node's replicas are rebuilt elsewhere by ordinary repair; the lost
member must be retired from consensus.

1. Confirm the node is genuinely gone, not merely unreachable. `Unreachable` is
   suspicion; permanent loss is an operator's determination.
2. `rs cluster decommission --node $LOST --force` — accepts the durability
   deficit and retires the membership. Record the reported deficit.
3. Watch repair restore the configured durability level. Payloads with no
   surviving valid source are reported as unrecoverable; they will not be
   repaired and must be restored from an external backup.
4. Admit the replacement as a **new node** with a fresh join token and a fresh
   node identity — never by reusing the lost node's identity or credential.

---

## R6. Lost metadata quorum

**This is disaster recovery, not a restart.** Read this whole section first.

**Symptoms.** No leader elects. `rs cluster status` reports the group as
unwritable. Fewer than a majority of voters survive.

**What is and is not true.** Payload bytes on surviving nodes are unaffected —
they are not in the consensus log. What is lost is the authority to say what
those bytes *mean*. Metadata quorum and payload durability are separate; losing
one does not imply losing the other.

**Do not:**

- Start surviving nodes as a fresh cluster. That creates a *second* cluster with
  the same data and no shared history, and the two can never be reconciled.
- Pick "the newest-looking copy" and promote it. Log length is not authority.
- Delete cluster state to "get it starting again". Reconciliation on a node that
  cannot confirm current cluster state is already prevented from deleting
  payloads, but re-initialising discards the evidence needed to recover.

**Do:**

1. Stop trying. A cluster without quorum is *correctly* unavailable. Availability
   is not worth inventing authority for.
2. Try to restore a majority: bring back any voter whose disk survives. A voter
   that restarts with its consensus log intact rejoins and counts. This is the
   only recovery that needs no judgement.
3. If a majority cannot be restored, this is an unsupported recovery at present.
   See [`limitations.md`](limitations.md): there is **no tested procedure** for
   reconstructing metadata authority from a minority. Payloads are intact and
   readable from disk; metadata must come from an external backup of the cluster
   catalog.

**Preventing it.** Run an odd number of metadata voters, spread across failure
domains, and back up the cluster catalog. `rs cluster snapshot` triggers a
metadata snapshot on demand.

---

## R7. Ordinary restart of one node

Distinct from everything above, and the common case.

1. Stop the node. In-flight writes fail cleanly; nothing half-commits.
2. Start it. It replays its consensus log, catches up from the leader (by log or
   by snapshot), and reconciles its local payloads against committed placement.
3. Reconciliation on a node that cannot yet confirm it is reading current cluster
   state **reports but does not delete**. A node catching up will therefore not
   collect anything until it is current. This is intended and is why a restart is
   safe even after a long absence.
4. Watch readiness: `Ready` requires quorum, no failed background task, and a
   `Healthy` node state. `Degraded` is serving; `Unavailable` is not.

---

## R8. Corrupted local state on one node

1. Take the node out of service: `rs cluster maintenance --node $NODE`.
2. If the corruption is in payload bytes, verification marks the affected
   replicas `Corrupt` and repair rebuilds them from a validated source. Corrupt
   bytes are never promoted into a healthy replica.
3. If the corruption is in the node's consensus state or catalog, do **not**
   repair in place. Treat the node as permanently lost ([R5](#r5-replace-a-permanently-lost-node))
   and admit a clean replacement. A member with a damaged state machine that
   rejoins can disagree with the cluster about committed state.
4. A member whose durable state is genuinely broken stops rather than continuing
   to apply commands. That is deliberate: the alternative is silent divergence.
