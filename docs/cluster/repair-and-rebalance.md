# Repair and Rebalance

Two background activities that move replicas, for two different reasons.

| | Repair | Rebalance |
| --- | --- | --- |
| Restores | Lost redundancy | Even capacity use |
| Runs | Automatically | Only when started (by default) |
| Urgency | High — durability is reduced | Low — nothing is at risk |
| Triggered by | A node going offline, a corrupt replica | An operator, or after adding a node |

Repair is not optional. Rebalance is a choice.

## Repair

When a payload has fewer healthy replicas than its replication factor, the cluster
copies it to a new node.

```mermaid
flowchart LR
    A[Node goes offline] --> B[Its replicas stop counting for durability]
    B --> C[Payloads become under-replicated]
    C --> D[Repair tasks queued]
    D --> E[Replicas copied to healthy nodes]
    E --> F[Redundancy restored]
```

Causes:

- A node reaching `offline`
- A replica reported `missing` by its node
- A replica failing integrity verification and becoming `corrupt`
- A node being drained or decommissioned

### Watching it

```bash
record-store repair status --endpoint https://management.example.com
```

| Field | Meaning |
| --- | --- |
| `active_tasks` | Movement tasks still to run |
| `parked_tasks` | Tasks that exhausted their retries and need attention |

`parked_tasks` above zero is the one to act on: repair has given up on those payloads
after 8 attempts. Check the logs for why — usually the source replica is also gone, or
no eligible target has capacity.

Fuller picture:

```bash
record-store cluster status --endpoint https://management.example.com
```

`under_replicated_payloads` should trend to zero. If it plateaus, repair is blocked
rather than slow — look at `parked_tasks` and at whether any node can still accept
placement.

### Limits

Repair movement is bounded so restoring redundancy does not take the cluster down with
it:

| | Default |
| --- | --- |
| Concurrent tasks | 8 |
| Streams per node | 4 |
| Bytes per second | 64 MiB |
| Scan interval | 30 seconds |
| Attempts before parking | 8 |
| Task lease | 600 seconds |

The lease means a task claimed by a node that then dies is picked up by another after
it expires, rather than being stuck forever.

### Why repair does not start immediately

A node in `suspect` or `unreachable` still counts toward durability. Repair begins only
once it reaches `offline`.

This is deliberate: a network blip that briefly hides a node holding terabytes would
otherwise trigger a full recovery storm, which is far more damaging than the blip. The
delay costs a window of reduced redundancy and buys protection from self-inflicted
outages.

## Rebalance

Evens out utilization across nodes. It does not change durability — every payload keeps
the same number of replicas.

**Automatic rebalancing is off by default.** Moving data is expensive and should be a
decision.

### Running one

```bash
record-store rebalance start --endpoint https://management.example.com
record-store rebalance status --endpoint https://management.example.com
```

### How targets are chosen

The cluster computes mean utilization across members, then:

- **Donors** — nodes above `mean + tolerance`, or any node needing capacity relief
- **Recipients** — nodes below `mean - tolerance` whose capacity level is normal

Default tolerance is 10 percent. With no donors or no recipients, there is nothing to
do and the rebalance completes immediately — which is the expected outcome on a
balanced cluster.

### Limits

Rebalance is throttled harder than repair, because it is never urgent:

| | Default |
| --- | --- |
| Concurrent tasks | 4 |
| Streams per node | 2 |
| Bytes per second | 32 MiB |
| Scan interval | 300 seconds |
| Tolerance | 10 percent |

### When to run one

- After [adding a node](adding-nodes.md) — new nodes start empty
- After a large deletion left utilization uneven
- When one node is approaching a watermark while others are idle

Not worth running when utilization is already within tolerance, or during a repair — let
repair finish first. Durability comes before tidiness.

## Both at once

They share the movement infrastructure and their limits apply independently. Running a
rebalance while repair is working means competing for the same disks and network.

Check before starting one:

```bash
record-store repair status --endpoint https://management.example.com
```

If `active_tasks` is non-zero, wait.

## Node-local reconciliation

Separately from both, each node periodically reconciles what it actually holds on disk
against what the cluster believes it holds:

```bash
RECORD_STORE_CLUSTER_RECONCILE_INTERVAL_SECONDS=300
```

This is what turns a silently missing or corrupt local replica into a `missing` or
`corrupt` record — which is in turn what queues a repair. Without it, a lost replica
would stay invisible until something tried to read it.

See [Integrity Verification](../operations/integrity-verification.md).

## Why a task exists

The queue names the reason, not just the work:

| Kind | Means |
| --- | --- |
| `repair` | A replica is missing or stale |
| `repair-corrupt` | Stored bytes failed verification |
| `device-failed` | A device is gone, and took its copies with it |
| `drain` | A node or device is being evacuated on purpose |
| `rebalance` | Evening out capacity |
| `rebalance-topology` | Improving failure-domain spread |
| `delete` | Releasing a tombstoned replica |

`device-failed` and `drain` are deliberately separate. A drain is planned and its
replicas are still readable; a failed device has already lost its copies. A queue
that called both "drain" would make an incident indistinguishable from
maintenance, and it also matters to the executor: there is no source to release
on a device that is gone.

Failed-device work outranks a drain at the same durability, for the same reason.

## What "balanced" means

Balance is measured **per device**, not per node.

A node holding one full drive and three empty ones is not balanced, but its
average looks comfortable. A node-level view reports a healthy number while the
storage class backed by the full drive can take no more writes. Devices are what
placement selects, so devices are what rebalancing evens out — including moving
data between drives inside one machine.

A move preserves the replica count. The source replica is described to placement
as absent and its device excluded, so the engine picks one replacement under the
same failure-domain rules that governed the original decision. That is what lets
a drive-to-drive move within a node happen without changing how many machines
hold a copy.

Drives that cannot take new placement — draining, failed, in maintenance — are
never destinations. Filling a drive somebody is trying to empty would be worse
than leaving the imbalance.

## Holding a rebalance

```bash
record-store rebalance pause
record-store rebalance resume
record-store rebalance throttle 33554432   # 32 MB/s per transfer, 0 disables
```

Pausing stops the planning of new movement **and** the transfers already queued.
A pause that only stopped planning would keep moving data for a while, which is
not what pressing pause meant.

A paused rebalance stays outstanding rather than disappearing: it has not
finished, and it still needs an operator to resume or cancel it.

Throttling is cluster configuration, not a property of one operation, so it
applies to rebalancing generally and survives the current one completing.
