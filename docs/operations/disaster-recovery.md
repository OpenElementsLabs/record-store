# Disaster Recovery

What to do when something has gone badly wrong. Work top to bottom: establish scope,
stop making it worse, then recover.

## First: do not make it worse

1. **Stop writes** if data integrity is in doubt. Disabling service accounts is
   instant and reversible.
2. **Do not run `storage repair --apply`.** It cannot recover missing data and it
   removes evidence.
3. **Do not delete anything**, including a data directory that looks broken. Move it
   aside instead.
4. **Take a copy** of the current state before attempting recovery.

## Establish scope

```bash
record-store status --endpoint https://management.example.com
record-store storage inspect --endpoint https://management.example.com
record-store cluster status --endpoint https://management.example.com   # cluster only
```

The number that decides everything: `metadata_without_data`. Above zero means object
bytes are gone.

## Server will not start

Read the logs first. Startup validation reports every problem at once.

| Message | Cause | Fix |
| --- | --- | --- |
| Configuration validation failed | Invalid or missing settings | `record-store server check-config` |
| Root credentials are required | Not set | Set both root variables |
| `credential_master_key` is required | Encryption on without a key | Set it — the **original** key |
| Data directory in use | Another process holds the lock | Stop it; check for a stale container |
| Schema newer than supported | Binary downgraded past a schema change | Use the newer binary, or restore a matching backup |

## Data directory is corrupt

```bash
# 1. Preserve it
mv /var/lib/record-store /var/lib/record-store.broken

# 2. Restore from backup into a fresh directory
mkdir -p /var/lib/record-store
record-store server restore-metadata /backups/latest/metadata
rsync -a /backups/latest/objects/ /var/lib/record-store/objects/

# 3. Start and verify
record-store storage inspect --endpoint http://127.0.0.1:7601
record-store verify bucket uploads --endpoint http://127.0.0.1:7601
```

Restore metadata and payloads from the **same** backup. See
[Backup and Restore](backup-and-restore.md).

## The master key is lost

This is unrecoverable, and it is worth being direct about what survives.

| | Recoverable |
| --- | --- |
| Object payloads, **encryption off** | Yes — plaintext on disk |
| Object payloads, **encryption on** | **No** |
| Service-account credentials | **No** |
| Webhook signing secrets | **No** |
| Share and embed capabilities | **No** |
| Bucket and object metadata | Yes |

With encryption off, you can stand up a new deployment with a new master key, recreate
every service account and webhook, and restore the payloads.

With encryption on, the object bytes cannot be decrypted by anything.

Back the master key up separately, today, if you have not.

## A node has failed (cluster)

Not a disaster — this is the case the cluster exists for.

1. Confirm it reached `offline`. Its replicas have stopped counting and repair is
   already restoring redundancy.
2. Wait for repair: `record-store repair status`.
3. `record-store node decommission <node-id> --force` — the data is genuinely gone, so
   the safety objection is moot.
4. Join a replacement with a fresh token and an **empty** data directory.

Do not reuse the failed node's data directory or identity. See
[Node Lifecycle](../cluster/node-lifecycle.md).

## Quorum is lost (cluster)

Fewer than a majority of voters are reachable. Metadata writes stop cluster-wide; reads
of applied metadata and of object payloads continue.

**Recovery is to bring voters back.** There is no safe way to commit without a majority
— that is the property Raft provides, and bypassing it means accepting divergent
metadata.

1. Identify which voters are down: `record-store cluster status`.
2. Restore enough of them to reach a majority.
3. Quorum returns on its own.

If a majority is permanently gone, this is whole-cluster loss.

## Whole cluster lost

1. Restore **one** node from its metadata backup.
2. Start it in `cluster` mode with **no seeds**, so it initializes a cluster.
3. Restore its object payloads from the same backup.
4. Issue join tokens and add fresh nodes with **empty** data directories.
5. Let repair rebuild replication.
6. Verify: `storage inspect`, then `verify bucket` on critical buckets.

!!! warning "Restore exactly one node"
    Restoring several nodes from their own backups gives each one consensus state
    describing a group that no longer exists. They will not form a cluster. Restore one,
    and join the rest as new nodes.

## Objects are missing but metadata is present

`metadata_without_data` above zero.

1. Identify them — `missing_payload_samples` in the inspection output gives examples.
2. In a **cluster**, check whether other replicas are healthy. Repair restores from
   them.
3. **Standalone**, restore the affected objects from backup.
4. Investigate the hardware. Silently vanished payloads usually mean a failing disk,
   a filesystem problem, or something outside Record Store writing to the data
   directory.
5. Only once you have finished investigating, run `storage repair --apply` to clean up.

## Checksum mismatches

```bash
record-store verify bucket uploads --endpoint https://management.example.com
```

A mismatch means the bytes on disk changed after they were written. That is a hardware
signal before it is a Record Store problem.

1. Check SMART data and the kernel log for I/O errors.
2. Check memory — bad RAM corrupts data on the way to disk.
3. In a cluster, healthy replicas of the same payload will repair it.
4. Standalone, restore the affected objects from backup.
5. Fix the hardware before restoring, or you will do this again.

## After any recovery

- [ ] `storage inspect` reports no missing payloads
- [ ] `verify bucket` passes on critical buckets
- [ ] Applications can read and write
- [ ] Cluster reports healthy, if applicable
- [ ] A **fresh backup** is taken and tested
- [ ] The root cause is understood, not just the symptom
- [ ] The runbook is updated with what actually happened

## Preparing in advance

The work that makes recovery possible is all done beforehand:

- The master key is backed up outside the data directory
- Backups run on a schedule and **restores have been tested**
- The runbook says where the master key and backups live
- Monitoring alerts before a disk fills, not after
- Cluster deployments have real failure-domain separation
- Someone other than the person who built it can do all of this
