# Linux Packages

Record Store publishes a `.deb` and an `.rpm` for `amd64` and `arm64` with every
release. They install the server and the CLI, a systemd service, and a
configuration file, and they generate credentials unique to the machine.

Use a package when Record Store is a service on a host you manage. Use
[container images](container-images.md) when it is one workload among many.

## Installing

Download the package for your architecture from the
[release page](https://github.com/OpenElementsLabs/record-store/releases), then:

=== "Debian, Ubuntu"

    ```bash
    sudo apt-get install ./record-store_0.2.1_amd64.deb
    ```

=== "RHEL, Rocky, Fedora, openSUSE"

    ```bash
    sudo dnf install ./record-store-0.2.1-1.x86_64.rpm
    ```

Verify the download against `SHA256SUMS` from the same release first — see
[Verifying a Release](verifying-releases.md).

### What gets installed

| Path | Purpose |
| --- | --- |
| `/usr/bin/record-store` | The CLI, and the server entry point |
| `/usr/bin/record-store-server` | The server binary |
| `/etc/record-store/record-store.toml` | Configuration; never overwritten by an upgrade |
| `/etc/record-store/record-store.env` | Generated credentials, mode `0600`, root-only |
| `/usr/lib/systemd/system/record-store.service` | The service unit |
| `/var/lib/record-store` | Objects and metadata, owned by the `record-store` account |

The binaries are statically linked, so the packages depend on no system
libraries at all and install on any distribution with systemd — Debian 11
onwards, RHEL 8 onwards, and their derivatives.

## Starting it

Installing does not start anything. Record Store holds data, and the first start
is a decision about where that data goes, so it is left to you.

Read the generated credentials, check the configuration, then start. The check
reads the credentials from `record-store.env` the way the systemd unit does;
without them it fails on the missing root credentials, not on your file:

```bash
sudo cat /etc/record-store/record-store.env
sudo sh -c 'set -a; . /etc/record-store/record-store.env; exec record-store server --config /etc/record-store/record-store.toml check-config'
sudo systemctl enable --now record-store
systemctl status record-store
```

The S3 API listens on `0.0.0.0:7600`. The management API listens on
`127.0.0.1:7601` and stays on loopback until you decide otherwise — it is
unrestricted administrative access. See [Ports](../reference/ports.md).

## The generated credentials

The package generates four values on first install and writes them to
`/etc/record-store/record-store.env`:

| Variable | What it is |
| --- | --- |
| `RECORD_STORE_ROOT_ACCESS_KEY` | The root S3 account |
| `RECORD_STORE_ROOT_SECRET_KEY` | Its secret |
| `RECORD_STORE_CREDENTIAL_MASTER_KEY` | Encrypts stored service-account credentials |
| `RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` | Bearer token for the management API and console |

They are unique to the machine, not defaults: two installs never share a
credential. Upgrades never rewrite the file.

!!! warning "Back up the master key off the host"

    `RECORD_STORE_CREDENTIAL_MASTER_KEY` cannot be rotated. If you lose it,
    every stored service-account credential becomes unreadable, and with
    encryption enabled so does every object. Copy it somewhere that is not this
    machine before you store anything.

Treat the root credential as break-glass: create
[service accounts](../administration/service-accounts.md) for applications, then
set `root_s3_enabled = false`.

## Configuring

Edit `/etc/record-store/record-store.toml`, validate, restart:

```bash
sudo sh -c 'set -a; . /etc/record-store/record-store.env; exec record-store server --config /etc/record-store/record-store.toml check-config'
sudo systemctl restart record-store
```

Every setting is described in [Configuration](../administration/configuration.md).
Secrets belong in `record-store.env`, not in the TOML: the TOML is readable by
the `record-store` group.

Upgrades never overwrite your edits. When a release changes the shipped
defaults, the new file is left beside yours as `.dpkg-dist` or `.rpmnew`.

### Hardening

The unit already runs with `ProtectSystem=strict`, an empty capability set, a
system-call filter and no privilege escalation. Add anything further with a
drop-in rather than editing the shipped unit, so upgrades keep it:

```bash
sudo systemctl edit record-store
```

## Logs

```bash
journalctl -u record-store -f
```

Set `json = true` under `[observability]` when shipping the journal to a log
aggregator.

## Upgrading

Install the new package over the old one. Configuration, credentials and data
are kept; the service restarts on the new binary.

```bash
sudo apt-get install ./record-store_<new-version>_amd64.deb   # or dnf install
sudo systemctl restart record-store
```

Read [Upgrading](upgrading.md) first — it covers what a version change can mean
for stored data.

## Removing

Removing the package stops and disables the service and leaves your data alone:

```bash
sudo apt-get remove record-store    # or: sudo dnf remove record-store
```

`/var/lib/record-store`, `/etc/record-store` and the service account are kept on
purpose. Uninstalling a package should not be the thing that destroys the
objects it was storing. To finish the job, once you are certain:

```bash
sudo rm -rf /var/lib/record-store /etc/record-store
sudo userdel record-store
```

## Building the packages yourself

The release workflow runs the same script you can:

```bash
cargo build --release --target x86_64-unknown-linux-musl \
  --bin record-store --bin record-store-server
deploy/linux/build-packages.sh amd64 0.2.1 \
  target/x86_64-unknown-linux-musl/release
```

It needs Docker, which it uses to run `nfpm`, and it refuses to package a
binary that is not statically linked.
