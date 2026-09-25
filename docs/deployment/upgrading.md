# Upgrading

## Before you start

1. **Read the release notes** for every version between the one you run and the one
   you want. They are generated from
   [CHANGELOG.md](https://github.com/OpenElementsLabs/record-store/blob/main/CHANGELOG.md)
   and list anything that requires action on your part.
2. **Plan a verified backup of the stopped deployment.** An upgrade is the moment a
   backup earns its keep. A backup refuses to run while the server holds the data
   directory, so it is taken after the stop, in the upgrade below.

3. **Rehearse on a non-production deployment** with a copy of real metadata.

An upgrade is a restart, and a restart is downtime for as long as the process is
stopped. Plan a window rather than expecting a seamless swap.

## The upgrade

The image's entrypoint is `record-store server`, so arguments passed to
`docker run` are server subcommands: `... record-store:<version> backup ...`
runs `record-store server backup ...`.

```bash
NEW=ghcr.io/openelementslabs/record-store:<version>

# 1. Stop, allowing the full drain window
docker stop --time 40 record-store

# 2. Pull the new image
docker pull "$NEW"

# 3. Back up the stopped deployment and verify the backup, with the new image.
#    /backups must be writable by uid 10001, the user the image runs as.
docker run --rm --volumes-from record-store --volume /backups:/backups \
  --env-file /etc/record-store/env "$NEW" backup /backups/pre-upgrade
docker run --rm --volume /backups:/backups \
  --env-file /etc/record-store/env "$NEW" verify-backup /backups/pre-upgrade --level full

# 4. Validate configuration against the new version before starting it
docker run --rm --env-file /etc/record-store/env "$NEW" check-config

# 5. Check the machine the way the new version's start-up will
docker run --rm --volumes-from record-store --env-file /etc/record-store/env "$NEW" doctor

# 6. Start
docker run -d --name record-store-new ... "$NEW"

# 7. Verify
record-store status --endpoint http://127.0.0.1:7601
```

!!! note "Upgrading from 0.1.3 to 0.2"
    0.1.3 has no `server backup`, `verify-backup` or `restore`: it ships only
    `backup-metadata`, which copies no payloads. Take the backup in step 3 with the
    **new** image, as shown. Taking it reads the stopped 0.1.3 data directory without
    changing it, and the backup restores into a directory 0.1.3 serves again, which
    is the rollback below. The first start migrates the metadata schema from 4 to 6,
    after which 0.1.3 cannot open the directory.

!!! warning "Upgrading from 0.1.2 or earlier"
    Upgrade to 0.1.3 first and start it once: it converts every database to the file
    format this release reads. A database 0.1.3 never opened is refused at start-up
    with a message naming 0.1.3.

Step 4 is the cheap one that catches the expensive problem: a setting that was valid
in the old version and is not in the new one. Step 5 checks what start-up will refuse
— a data directory writable by every local user, a temporary directory on another
filesystem, an unfinished restore — without starting anything, and exits 7 if any of
them fails.

## Metadata schema

Metadata carries a schema version. On startup the server checks it against what the
binary supports.

- **Newer binary, older data** — the server migrates or opens it as needed.
- **Older binary, newer data** — refused. Downgrading past a schema change is not
  supported.

The same rule applies to restores: `restore` rejects a backup whose schema version is
newer than the binary's, and says so as an upgrade instruction rather than as a
database error.

This is why the backup comes first. Rolling back the binary does not roll back the
metadata.

## Choosing what to upgrade to

Upgrade to an exact version, never to `latest`: you need to know what you are
moving to, and to be able to move back to what you had. See
[Container Images](container-images.md).

```bash
# Confirm what you are about to run before you run it
docker run --rm --entrypoint record-store \
  ghcr.io/openelementslabs/record-store:0.2.1 --version
```

Check the digest and the checksums before you deploy — see
[Verifying a Release](verifying-releases.md).

## Rolling back

If the new version fails to start or misbehaves:

```bash
# 1. Stop the new version
docker stop --time 40 record-store

# 2. Start the previous image against the same data directory
docker run -d --name record-store ... ghcr.io/openelementslabs/record-store:<previous>
```

If the metadata schema changed, the old binary will refuse to open it. 0.1.3 does
this by exiting with a panic inside its database library (`internal error: entered
unreachable code`) rather than a schema message; the data directory is left
unchanged. Then the path is a full restore of the pre-upgrade backup, with the new
image's CLI, into an empty directory:

```bash
# Move the whole data directory aside rather than deleting it
mv /var/lib/record-store /var/lib/record-store.failed
mkdir -p /var/lib/record-store && chown 10001:10001 /var/lib/record-store

docker run --rm --volume /var/lib/record-store:/var/lib/record-store \
  --volume /backups:/backups --env-file /etc/record-store/env \
  "$NEW" restore /backups/pre-upgrade --level full

# Then start the previous image against it
docker run -d --name record-store ... ghcr.io/openelementslabs/record-store:<previous>
```

Restore requires a data directory with no `metadata/`, `objects/`, or `system/` in it,
and brings all three back from the one backup.

## Console

The console is a separate image, `ghcr.io/openelementslabs/record-store-console`,
and can be upgraded independently. It is a client of
the management API, so a version skew between them is survivable — but keep them close
and upgrade the console after the server.

## After the upgrade

```bash
record-store status --endpoint http://127.0.0.1:7601

# Round-trip a real object
aws --endpoint-url https://storage.example.com s3 cp /tmp/smoke.txt s3://smoke-test/
aws --endpoint-url https://storage.example.com s3 cp s3://smoke-test/smoke.txt -
```

Watch error rates and log volume for a while afterwards. Keep the pre-upgrade backup
until you are confident.
