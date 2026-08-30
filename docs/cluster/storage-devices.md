# Storage Devices

A node is not a unit of storage. Each drive a node serves is registered
separately, and placement chooses a **device**, not just a node.

That distinction is what lets a machine with four drives contribute four places
to put data, while still counting as one thing that can fail.

## Why devices are separate

```text
Node A
├── NVMe   4 TB
├── SSD    8 TB
├── HDD   20 TB
└── HDD   20 TB
```

Placement sees four independent targets here. It also knows they share a
chassis: with the failure domain set to `node`, at most one replica of an object
lands on this machine no matter how many drives it has. Spreading three replicas
across three drives in one server is not durability — the server is still a
single thing that can stop.

## Identity

A device's identity is stable and is **not** its path. Linux device names move
across reboots and hardware changes, so `/dev/sda` is recorded as descriptive
information rather than as the thing Record Store refers to.

```bash
record-store drive list
```

Each device reports its stable identifier, the node serving it, its kind, its
storage class, capacity, health, and lifecycle state.

## Declaring a node's drives

A node always serves its `data_directory` as one device. Additional drives are
declared in its configuration:

```toml
[storage]
data_directory = "/var/lib/record-store"

[[storage.devices]]
name = "nvme0"
path = "/mnt/nvme0"
storage_class = "hot"
weight = 2000

[[storage.devices]]
name = "hdd0"
path = "/mnt/hdd0"
```

The node advertises all of them when it joins, and each becomes an independent
placement target. A node declaring nothing behaves exactly as it did before
devices existed, which is what leaves standalone and existing clusters unchanged.

!!! warning "The name is identity"
    A device's stable identity is derived from the node and this `name`, not from
    the path. That is what lets a node restart, or a disk come back as a
    different `/dev` node, without orphaning the replicas already on it.

    Renaming a device in configuration therefore declares a *different* device.
    The replicas placed under the old name become unreachable, and the cluster
    repairs them elsewhere. Change a path freely; change a name only
    deliberately.

Paths must be distinct from each other and from `data_directory`. Two devices on
one filesystem are not two devices, and Record Store refuses the configuration
rather than reporting failure independence it does not have.

## Discovery is not ownership

Finding a disk never enrolls it. Record Store does not format, mount, erase, or
claim a device it happens to see; a drive participates only because an
administrator declared it. The path must already exist and be writable —
Record Store will not create a filesystem for you.

To see what a node *could* use:

```bash
record-store drive discover
```

This reads the node's mount table and lists filesystems that could hold
payloads. It registers nothing. Copy a path into a `[[storage.devices]]` entry
and restart the node to actually use it.

Discovery deliberately never proposes:

| Excluded | Why |
| --- | --- |
| `/`, `/boot`, `/usr`, `/var`, `/etc` | The operating system. A full disk would take the machine down |
| `tmpfs`, `proc`, `sysfs`, `overlay`, and other pseudo filesystems | Memory or kernel state, not durable storage |
| Read-only mounts | A device that would fail on its first write |
| Paths already in use | The data directory and every declared device |

It reports capacity and, on Linux, whether the backing device is rotational.
It reports health as `unknown`, because nothing has inspected the hardware and
`healthy` would be a reading nobody took.

Discovery is implemented for Linux. Elsewhere the command reports that plainly
rather than returning an empty list, which would read as "no storage here".

## Kind and class are different questions

| | |
| --- | --- |
| **Kind** | What the hardware is: `nvme`, `sata_ssd`, `sata_hdd`, `filesystem_directory`, `unknown` |
| **Storage class** | What policy wants it used for |

Kind is a fact about the device. Class is a decision about it. Keeping them
apart is what lets an administrator put an NVMe drive in a capacity tier, or run
a whole cluster on directories without pretending to know the hardware
underneath.

When the platform does not expose something, it is recorded as `unknown` rather
than guessed. `unknown` health is not a problem — it is an honest absence of
information, and a device with unknown health is still eligible for placement.

## Lifecycle

| State | Takes new data | Counts for durability | Set by |
| --- | --- | --- | --- |
| `discovered` | no | no | system |
| `available` | no | no | administrator |
| `active` | **yes** | yes | administrator |
| `degraded` | no | yes | system |
| `draining` | no | yes | administrator |
| `maintenance` | no | yes | administrator |
| `failed` | no | **no** | system |
| `safe_to_remove` | no | no | system |
| `retired` | no | no | administrator |

Only `active` receives new data.

`safe_to_remove` is the one state an administrator cannot simply ask for. It is
set only after evacuation actually completed — see below.

Transitions are validated. A device cannot jump from `active` straight to
`safe_to_remove`, because that would assert an evacuation that never happened.

```bash
record-store drive activate <node> <device>
record-store drive maintenance <node> <device>
record-store drive resume <node> <device>
```

## Health and lifecycle are separate

A device can be administratively `active` while its health is `unknown`, or
`active` while health is `degraded`. Neither value is inferred from the other:

- **Lifecycle** is what an administrator decided.
- **Health** is what the platform reported.

Collapsing them would mean either a warning silently removing a drive from
service, or an administrator's decision being overwritten by a transient
reading. Both are worse than reporting two fields.

## Capacity

Raw capacity is not usable capacity.

| | |
| --- | --- |
| `raw` | What the platform reports |
| `usable` | What Record Store may allocate from |
| `available` | What is free right now |
| `reserved` | Held back for safety margins and in-flight work |

Placement stops using a device before it is completely full. A device at its
watermark stops receiving new data while continuing to serve what it already
holds.

## Weight

Capacity drives placement automatically: a 20 TB drive receives roughly five
times the data of a 4 TB drive. An administrator can bias this with a configured
weight, where `1000` is neutral.

Weight is a stable input. It is deliberately **not** adjusted from live latency
or queue depth — permanent placement should not move because a drive was briefly
busy. Transient measurements belong to read routing and scheduling instead.

## Movement concurrency

Repair and rebalance transfers are capped per device, derived from the hardware:

| Media | Transfers at once |
| --- | --- |
| Rotational | 1 |
| Solid state | 4 |
| Unidentified | 2 |

A rotational drive serves one transfer far better than several, because parallel
movement turns sequential reads into seeks. Solid-state media has no such
penalty. Media nobody identified gets the cautious middle: guessing generously
about unknown hardware risks flooding the drive least able to cope.

Override it per device with `movement_concurrency`. A configured value always
wins — derived defaults are a starting point, not a policy that overrides
something you measured.

## Next

- [Replacing a Drive](replacing-a-drive.md) — draining and removing safely
- [Node Lifecycle](node-lifecycle.md) — the same ideas one level up
- [Repair and Rebalance](repair-and-rebalance.md) — what moves data and when
