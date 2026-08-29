# Cluster Problems

## Start here

```bash
record-store cluster status --endpoint https://management.example.com
record-store node list --endpoint https://management.example.com
```

The status document's `notes` fields explain each health classification in plain
language. Read them before anything else.

## A node starts but never joins

The most common cluster problem, and almost always `RECORD_STORE_RPC_ADVERTISE`.

A bind address of `0.0.0.0` is reachable from nowhere. Behind Docker, Kubernetes, or
NAT, peers need a routable address:

```bash
RECORD_STORE_RPC_ADVERTISE=node-2.internal:7603
```

Check reachability from another node:

```bash
nc -zv node-2.internal 7603
```

Other causes:

| Cause | Check |
| --- | --- |
| Port 7603 blocked | Firewall between nodes |
| Seeds unreachable | `RECORD_STORE_CLUSTER_SEEDS` names a live member's **RPC** address |
| Token expired or used | Issue a fresh one |
| Token from another cluster | Compare `cluster_id` |

## The node formed its own cluster

The dangerous one, because nothing fails — the node succeeds at the wrong thing.

A node in `cluster` mode with **no seeds** initializes a new cluster. If
`RECORD_STORE_CLUSTER_SEEDS` was empty or unset at startup, that is what happened.

```bash
record-store cluster status --endpoint https://node-2.internal:7601
```

A different `cluster_id` from the others confirms it.

Recovery:

1. Stop the node.
2. Delete its data directory. It contains a whole separate cluster's state.
3. Set `RECORD_STORE_CLUSTER_SEEDS` and a fresh join token.
4. Start it again.

## Join refused

| Cause | Fix |
| --- | --- |
| Token expired | Issue another — 60–86400 seconds |
| Token already used | Single-use. Issue one per node |
| Token from a different cluster | Issue it from a member of the right cluster |
| Seeds not configured | A token without seeds is refused at startup |

```bash
record-store cluster issue-join-token \
  --description "node-2 retry" \
  --endpoint https://management.example.com
```

## Nodes joined but credentials are unreadable

`RECORD_STORE_CREDENTIAL_MASTER_KEY` differs between nodes. It must be byte-identical:
it seals credentials, webhook secrets, and capability secrets, and a node with a
different key cannot read what another wrote.

Compare — carefully, without printing the values into a shared log — and redeploy the
mismatched node with the correct key.

## Quorum lost

```text
metadata quorum lost: 1 of 3 members reachable, 2 required
```

Metadata writes stop cluster-wide. Reads of applied metadata and of object payloads
continue.

**The only recovery is to bring voters back.** There is no safe way to commit without a
majority — that is the property Raft provides, and bypassing it means accepting
divergent metadata.

1. `record-store cluster status` — which voters are down?
2. Restore enough of them to reach a majority.
3. Quorum returns on its own.

If a majority is permanently gone, this is whole-cluster loss. See
[Disaster Recovery](../operations/disaster-recovery.md).

## A node is `suspect` or `unreachable`

Heartbeats are late, or RPC is failing. Its replicas **still count** toward durability —
repairing immediately after a network blip causes a recovery storm worse than the blip.

1. Is the process running? `record-store status --endpoint <that node>`
2. Is 7603 reachable from its peers?
3. Are the clocks roughly in sync?
4. Check its logs.

It becomes `offline` only after being unavailable long enough, and repair starts then.

## `under_replicated_payloads` is not decreasing

Repair is blocked rather than slow.

```bash
record-store repair status --endpoint https://management.example.com
```

`parked_tasks` above zero means repair gave up after 8 attempts. Usual causes:

- No eligible target — every remaining node is at the critical watermark, or none
  matches the required storage class.
- The source replica is also gone.

```bash
record-store node list --endpoint https://management.example.com
```

Look at utilization. Add a node, or free space.

## `unavailable_payloads` above zero

A payload has **no** healthy replica. That data is unreadable.

1. `record-store cluster status` for the count.
2. Check whether an offline node holds the only remaining copy — bringing it back is the
   fastest fix.
3. Otherwise restore from backup.

Do not decommission a node while this number is non-zero unless you have confirmed it
does not hold the missing replicas.

## Decommission refused

```text
cannot decommission node <id>. Reason: 12 object version(s) would become
unreadable and 340 would fall below the required durability of 2 replica(s)
```

The safety check is working. Options:

1. **Drain first**, wait for it to complete, then decommission. The safe path.
   ```bash
   record-store node drain <node-id> --endpoint <endpoint>
   ```
2. **Force**, only when the node is physically gone and its data is already lost:
   ```bash
   record-store node decommission <node-id> --force --endpoint <endpoint>
   ```

`--force` bypasses the objection, not the data movement. A mistyped node ID plus
`--force` destroys durability — which is exactly why the check is mandatory without it.

## Drain never finishes

Draining copies real data. It is bounded by `cluster.movement_bytes_per_second`
(default 64 MiB/s per movement) and `cluster.movement_concurrency` (default 4).

```bash
record-store rebalance status --endpoint https://management.example.com
```

If it has genuinely stalled rather than being slow, the usual cause is no eligible
target — the remaining nodes are full or the storage classes do not match.

## Writes fail with a placement error

| Error | Cause |
| --- | --- |
| `NoEligibleNodes` | No node passed the filters. The message reports how many were healthy, class-matching, and had capacity — read it to see which filter bit |
| `InsufficientFailureDomains` | Strict domains are on and there are not enough distinct ones |
| `InsufficientDurability` | Fewer eligible targets than the acknowledgement requirement |

`NoEligibleNodes` is usually capacity or storage class, not health. See
[Replication](../cluster/replication.md).

## Leader elections keep happening

Persistent leadership churn means the election timeout is too tight for the network.

```toml
[cluster]
consensus_heartbeat_millis = 500
election_timeout_min_millis = 2000
election_timeout_max_millis = 4000
```

Raise them together, keeping `election_timeout_min_millis` above twice the heartbeat,
and apply the same values on every node.

## Replicas are all in one place

Every node has the same failure-domain labels, so placement has only one domain to work
with.

```bash
RECORD_STORE_CLUSTER_FAILURE_DOMAIN=region=eu-central,zone=b,rack=r2
```

Labels describe reality — they do not create it. Three nodes on one host give you one
host's worth of durability no matter what the labels say.

Changing labels affects **new** placement. Run a rebalance to move existing data.

## Nothing moved to a new node

Automatic rebalancing is off by default. New writes will use the node immediately;
existing data stays where it is.

```bash
record-store rebalance start --endpoint https://management.example.com
record-store rebalance status --endpoint https://management.example.com
```

If it completes instantly, utilization is already within the 10 percent tolerance and
there is nothing to move.
