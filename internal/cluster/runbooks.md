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

## R6. Lost metadata quorum — disaster recovery

**This is not a restart.** Read the whole section before running anything.

**Symptoms.** No leader elects. `rs cluster status` reports the group as
unwritable. Fewer than a majority of metadata voters survive.

**What is and is not lost.** Payload bytes on surviving nodes are unaffected —
they were never in the consensus log. What is lost is the authority to say what
those bytes *mean*. Metadata quorum and payload durability are separate; losing
one does not imply losing the other.

### R6.1 First, try the recovery that needs no judgement

Bring back any voter whose disk survives. A voter that restarts with its
consensus state intact rejoins and counts toward the majority. If that restores a
quorum, you are done — this was an outage, not a disaster.

A node that starts and refuses with *"would form a second cluster"* is telling
you its consensus state is gone while its data is not. That node cannot be part
of restoring a quorum by itself; go to R6.2.

### R6.2 Choose a survivor

Run this on **every** survivor, with the server stopped:

```
rs cluster inspect-state --config /etc/record-store/config.toml
```

It changes nothing. Compare the **applied index** across survivors: the highest
one has the most committed metadata and is the only correct choice. Choosing a
lower one silently discards every commit between the two.

Also read the snapshot line. `DAMAGED` means a snapshot publication or transfer
was interrupted; it does not stop the member starting and it does not affect the
choice, but it is worth knowing before you conclude a node is healthy.

### R6.3 Understand what you are about to accept

Recovery **discards any metadata the lost quorum committed but never replicated
to this member**. That is data loss, it is not reversible, and the command will
not run without an explicit acknowledgement of it.

### R6.4 Never recover two survivors

Running the recovery on two survivors separately produces **two clusters holding
one identifier**. They can never be reconciled and neither is more correct than
the other.

Record Store cannot prevent this from inside a single node, so each recovery
stamps a distinct lineage into the cluster identity, which makes the split
detectable afterwards (`recovery_id` in `inspect-state`). Detectable is not
repairable. Pick one survivor, write down which, and recover only that one.

### R6.5 Recover

With the server **stopped** on the chosen survivor:

```
rs cluster recover \
  --config /etc/record-store/config.toml \
  --cluster-id <the id inspect-state reported> \
  --reason "two of three voters lost in the rack-b failure" \
  --accept-data-loss
```

It rebuilds the consensus membership around this member alone, keeping the state
machine untouched: object history, versions, retention and Object Lock,
credentials, placement, and the cluster's own identity all survive. It discards
the old log tail and any snapshot built under the old membership — a snapshot
carrying the old voter set would re-add every member this just removed.

Read the report. It states:

- **voters removed** — the members that are no longer part of the group;
- **log entries discarded** — the metadata that was lost;
- **payloads needing another holder** — object versions with no replica on this
  member. They are **unreadable** until one of their other holders returns, or
  must be restored from an external backup. This is the availability deficit
  recovery leaves behind;
- **the write policy** — see R6.7.

### R6.6 Start it and verify

```
rs cluster status --endpoint $MGMT
```

Confirm a leader, confirm the cluster id is unchanged, and confirm
`recovery_generation` has advanced. Read back a known object and check its
content, not just its status code.

### R6.7 Retire the nodes that are not coming back

Recovery rebuilt metadata **authority**. It did not change the cluster's view of
**who exists**: the lost nodes are still recorded as members, still look healthy
until failure detection catches up, and placement will keep choosing them. Until
they are retired, a write can be planned onto a node that no longer exists and
will fail for a reason that looks unrelated.

For each node named in the recovery report's `other_nodes` that is genuinely
gone:

```
rs node decommission <node-id> --force --endpoint $MGMT
```

`--force` is correct here and only here: the safety check will object because
those nodes hold replicas, and that objection is true — the replicas are lost.
Forcing accepts a deficit that has already happened rather than causing one.
Record the reported deficit.

A node you intend to reuse is not retired this way; give it an empty data
directory and re-admit it as a replacement (R6.8).

### R6.8 Expect it to be readable before it is writable

A single surviving member usually **cannot accept writes**: a policy requiring
two acknowledgements cannot be met by one node, and the cluster refuses rather
than acknowledging below its policy. That refusal is the durability guarantee
working.

Two ways forward, and they are different decisions:

- **Restore capacity.** Admit replacement nodes (R6.9). Writes resume on their
  own once the policy can be met, and repair rebuilds the missing replicas.
- **Lower the policy deliberately.** `rs cluster status` shows the current
  factor; setting it lower is an explicit acceptance of weaker durability, and it
  applies to every subsequent write. Do this only to restore service, and raise
  it again once capacity returns.

Never treat the refusal as a bug to work around.

### R6.9 Re-admit the other nodes

Every other node is now outside the consensus group. Bring each back as a
**replacement**, not as its old self:

1. Confirm the node is genuinely to be reused. A node whose data is stale is
   fine — it will be overwritten by the cluster's committed state — but a node
   whose disk is suspect should be replaced instead.
2. Give it an empty data directory and a fresh node identity.
3. Issue a join token on the recovered leader and start it with `cluster.seeds`
   pointing there.
4. Watch repair restore the configured durability level.

A node that was decommissioned cannot rejoin on its old identity; this is
deliberate and is refused with that reason.

### What cannot be recovered

- Metadata committed by the lost quorum that never reached the survivor. Gone.
- Object versions whose every replica was on lost nodes. The metadata may survive
  and the bytes do not; these are reported by the recovery report and read as
  unavailable rather than being silently dropped.
- Anything on a node with no surviving copy anywhere. Restore from an external
  backup ([R9](#r9-external-backups)).

### Preventing it

Run an odd number of metadata voters across failure domains, keep the
replication factor above one, and take regular off-box backups (R9).
`rs cluster snapshot` triggers a metadata snapshot on demand.

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

---

## R9. External backups

Recovery rebuilds authority from a survivor. It cannot conjure data that no
survivor holds, so a backup taken off the cluster is the only answer to the total
loss of a failure domain.

`rs server backup` takes a coordinated offline backup of one node's whole data
directory — catalog, payloads, and the system records that say which storage
format and master key the payloads were written under — while holding the data
directory's exclusive lock. It is a point-in-time copy of a **stopped**
deployment, and the manifest records it as such.

`rs server verify-backup` checks it before you need it. Do this on a schedule;
an unverified backup is a hope.

`rs server restore` restores into an empty data directory, and refuses a
destination that is not empty.

**What a per-node backup does and does not give you in a cluster.** It captures
that node's view: its payloads, and the replicated metadata as of the moment it
was stopped. It is a sound basis for rebuilding a single node. It is **not** a
cluster-wide consistent snapshot — different nodes stopped at different moments
hold different applied positions — so restoring several nodes from independently
taken backups and starting them together is not a supported procedure and is not
tested. Restore one, recover it (R6), and re-admit the rest as replacements.
