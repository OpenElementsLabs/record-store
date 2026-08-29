# Storage Classes

A storage class is a name a bucket asks for. A storage **policy** is what that
name means.

```text
class:  "hot"
policy: solid state only, 2 copies, separated across racks, 15% kept free
```

Keeping the two apart is the point. `nvme` is a fact about a device; `hot` is a
decision about how it is used. An operator may put any hardware behind any
class, and a deployment running entirely on directories never has to pretend to
know what its disks are.

## The default class

Every cluster has `standard`, whether or not anyone defined it. It is
synthesized from cluster configuration — the cluster's replication factor and
failure domain, and no hardware restriction at all.

That matters for upgrades: a cluster that predates storage classes resolves
every bucket to exactly the behaviour it already had. Nothing moves.

`standard` cannot be removed.

## Defining a class

```bash
record-store storage-class set hot \
  --replicas 2 \
  --failure-domain rack \
  --strict \
  --device-kind nvme \
  --device-kind sata_ssd \
  --minimum-free-percent 15 \
  --description "Solid state, rack separated"
```

```bash
record-store storage-class list
record-store storage-class show hot
```

## What a policy controls

| Field | Meaning |
| --- | --- |
| `durability` | How copies are made. Replication today |
| `failure_domain` | What replicas must be separated across |
| `strict_failure_domains` | Refuse rather than place without that separation |
| `device_filter` | Which device kinds may hold the data |
| `minimum_free_space_percent` | Capacity held back on every device in the class |

Each overrides the cluster-wide setting for buckets using that class. A policy
that sets nothing behaves exactly like cluster configuration.

### Device filters

An empty filter accepts anything, which is the right default — a filter nobody
configured should not quietly exclude a deployment's storage.

A filter that names kinds excludes `unknown`. A platform that could not identify
a device is not evidence that the device is an NVMe drive, and a class that
asked for solid state should not receive a disk nobody could classify.

### Free space

`minimum_free_percent` is held back **on top of** the cluster's own safety
margin. It lets one class keep headroom the rest of the cluster does not, which
is how a latency-sensitive class avoids ever running near full.

## Erasure coding is not available yet

A policy can express erasure coding, and Record Store will **refuse it**:

```text
erasure coding is not available as a bucket durability strategy in this release;
the coding engine exists but no write path uses it
```

The Reed-Solomon engine is implemented and tested, but nothing writes or reads
stripes, so a policy claiming `4+2` would store three copies instead. Refusing is
the honest answer: a durability promise you cannot detect is broken until you
need the parity is worse than no promise.

The variant exists in the durable format so enabling it later needs no
migration.

## Removing a class

```bash
record-store storage-class delete hot
```

Refused while any device still carries the class:

```text
storage class 'hot' is still assigned to 4 device(s);
reassign them before removing the policy
```

Those devices would otherwise resolve to no policy at all and silently stop
being placement candidates, which looks like capacity vanishing. Reassign them
first.

## Upgrading

Storage policies advance the cluster format version to 3. A node running an
older binary is refused rather than allowed to join, because it would resolve
every bucket to cluster defaults instead of its configured class — a placement
difference an operator could not see.

Upgrade every node before defining a class. See
[Upgrading](../deployment/upgrading.md).
