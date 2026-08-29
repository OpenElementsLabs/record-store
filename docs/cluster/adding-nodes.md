# Adding Nodes

Joining a node needs two things together: **seeds** so it knows whom to contact, and a
**join token** proving it is allowed to.

Configuring one without the other is refused at startup. That is deliberate — a token
with no seeds is meaningless, and seeds with no token would let anything that can
reach the RPC port join.

## 1. Issue a join token

From any node already in the cluster:

```bash
record-store cluster issue-join-token \
  --description "node-4, rack r4" \
  --lifetime-seconds 3600 \
  --endpoint https://management.example.com
```

| | |
| --- | --- |
| Lifetime | 60–86400 seconds, default 3600 |
| Uses | Single-use |
| Description | Free text; use it to record what the token was for |

The token is returned once. Treat it as a secret: it is authority to join the cluster.

Issue one token per node. Reusing a token for a second node fails.

## 2. Configure and start the node

```bash
RECORD_STORE_MODE=cluster
RECORD_STORE_RPC_BIND=0.0.0.0:7603
RECORD_STORE_RPC_ADVERTISE=node-4.internal:7603
RECORD_STORE_CLUSTER_SEEDS=node-1.internal:7603,node-2.internal:7603
RECORD_STORE_CLUSTER_JOIN_TOKEN=<token issued for node-4>
RECORD_STORE_CLUSTER_FAILURE_DOMAIN=region=eu-central,zone=d,rack=r4
RECORD_STORE_CLUSTER_S3_ENDPOINT=https://storage.example.com
RECORD_STORE_CLUSTER_STORAGE_CLASS=standard

# Identical to every other node
RECORD_STORE_CREDENTIAL_MASTER_KEY=<your-master-key>
RECORD_STORE_ROOT_ACCESS_KEY=<your-access-key>
RECORD_STORE_ROOT_SECRET_KEY=<your-secret-key>
RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=<your-system-token>
```

List several seeds — up to 32. The node tries them in turn, so one unreachable seed
does not block the join.

## The three settings that matter

### `RECORD_STORE_RPC_ADVERTISE`

The address **peers** use to reach this node. A bind address of `0.0.0.0` is reachable
from nowhere, so behind Docker, Kubernetes, or NAT this must be set explicitly.

This is the most common cause of a node that starts cleanly and never becomes healthy.

### `RECORD_STORE_CLUSTER_FAILURE_DOMAIN`

`key=value` pairs, comma-separated:

```bash
RECORD_STORE_CLUSTER_FAILURE_DOMAIN=region=eu-central,zone=d,rack=r4
```

Placement spreads replicas across these. Give every node labels that reflect **real**
physical separation — three nodes labelled `rack=r1` will happily receive all three
replicas of the same object.

The default scope is `rack`. See [Replication](replication.md).

### `RECORD_STORE_CREDENTIAL_MASTER_KEY`

Must be byte-identical across every node. Nodes with different master keys cannot read
each other's sealed credentials and webhook secrets.

## 3. Watch it join

```bash
record-store node list --endpoint https://management.example.com
```

The node appears as `joining`, then `healthy` once it has reconciled. It receives no
new replicas until then.

```bash
record-store cluster status --endpoint https://management.example.com
```

## 4. Distribute data to it

A new node starts empty. Existing objects are not moved to it automatically —
rebalancing is off by default, because moving data is expensive and should be a
decision, not a surprise.

New writes will use it immediately, since placement prefers the least-utilized
eligible node. To even out what is already there:

```bash
record-store rebalance start --endpoint https://management.example.com
record-store rebalance status --endpoint https://management.example.com
```

See [Repair and Rebalance](repair-and-rebalance.md).

## Adding a control node

```bash
RECORD_STORE_MODE=control
RECORD_STORE_CLUSTER_SEEDS=node-1.internal:7603
RECORD_STORE_CLUSTER_JOIN_TOKEN=<token issued for the control node>
RECORD_STORE_RPC_ADVERTISE=control.internal:7603
```

Control mode holds no replicas and serves no S3 traffic. Seeds are required — a
management-only process must never bootstrap a cluster of its own.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| Refuses to start: "join_token requires seeds" | Set `RECORD_STORE_CLUSTER_SEEDS` |
| Refuses to start: "control-plane process needs seeds" | Same, in control mode |
| Starts, never appears in `node list` | `RPC_ADVERTISE` is wrong, or 7603 is blocked between nodes |
| Join refused | Token expired, already used, or from a different cluster |
| Formed its **own** cluster | Seeds were empty at startup |
| Joined, but credentials are unreadable | The master key differs from the other nodes |

The "formed its own cluster" case is the dangerous one, because nothing fails — the
node succeeds at the wrong thing. Check that `cluster_id` matches:

```bash
record-store cluster status --endpoint https://node-4.internal:7601
```

If it does not, stop the node, delete its data directory, and rejoin with a fresh
token.

More in [Cluster Problems](../troubleshooting/cluster.md).
