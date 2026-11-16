# Installation

Record Store is available as container images, Linux packages, a Helm chart,
standalone binaries, and source code. To choose an installation method, find
your goal in the following table:

| Goal | Installation method |
| --- | --- |
| Try Record Store with one command | [Published container images](#run-the-published-container-images) |
| Run Record Store as a service on a Linux host | [Linux packages](#install-a-linux-package) |
| Run Record Store on Kubernetes | [Helm chart](#install-on-kubernetes) |
| Run Record Store without a package manager | [Binary archives](#install-from-a-binary-archive) |
| Change Record Store | [Source code](#build-from-source) |

## Before you begin

Make sure that you have the requirements for your installation method:

| Installation method | Requirements |
| --- | --- |
| Published container images | Docker |
| Docker Compose | Docker with the Compose plugin |
| Linux packages | Debian 11 or later, or RHEL 8 or later, with systemd |
| Helm chart | Kubernetes 1.25 or later, Helm 3 |
| Binary archives | Linux on `amd64` or `arm64` |
| Source code | Rust 1.97.1 (pinned by `rust-toolchain.toml`) and a C toolchain |
| Web console from source | Node.js 24, in addition to one of the preceding methods |

> You don't need to install `protoc`. The build includes its own copy.

## Run the published container images

Use the published images for production and for trials. You don't need to
compile anything.

1.  Pull the server image and the console image:

    ```bash
    docker pull ghcr.io/openelementslabs/record-store:latest
    docker pull ghcr.io/openelementslabs/record-store-console:latest
    ```

    The `latest` tag points to the newest stable release, which is suitable for
    a trial. For a deployment that you keep running, pull a specific release
    instead, so that you control when you upgrade:

    ```bash
     docker pull ghcr.io/openelementslabs/record-store:0.3.0
     docker pull ghcr.io/openelementslabs/record-store-console:0.3.0
    ```

  Keep both images on the same version. Both images support `linux/amd64` and
    `linux/arm64`, and Docker pulls the correct architecture automatically.

1.  Record Store has no built-in credentials and doesn't start until you provide
    them. Export the following variables in your shell:

    ```bash
    export RECORD_STORE_ROOT_ACCESS_KEY='ROOT_ACCESS_KEY'
    export RECORD_STORE_ROOT_SECRET_KEY='ROOT_SECRET_KEY'
    export RECORD_STORE_CREDENTIAL_MASTER_KEY='CREDENTIAL_MASTER_KEY'
    export RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN='MANAGEMENT_SYSTEM_TOKEN'
    ```

    Replace the following:

    - `ROOT_ACCESS_KEY`: the access key ID for the root S3 credentials.
    - `ROOT_SECRET_KEY`: a long, random secret key for the root S3 credentials.
    - `CREDENTIAL_MASTER_KEY`: a stable key of 32 or more characters. Record
      Store uses it to encrypt stored credentials.
    - `MANAGEMENT_SYSTEM_TOKEN`: a separate token of 32 or more characters for
      the management API.

1.  Start the server. Each `-e` flag without a value passes the variable from
    your shell into the container:

    ```bash
    docker run --read-only \
      -e RECORD_STORE_ROOT_ACCESS_KEY \
      -e RECORD_STORE_ROOT_SECRET_KEY \
      -e RECORD_STORE_CREDENTIAL_MASTER_KEY \
      -e RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN \
      -p 7600:7600 -p 7601:7601 \
      -v record-store-data:/var/lib/record-store \
      ghcr.io/openelementslabs/record-store:latest
    ```

### Run the server and the console with Docker Compose

To run the server and the console together from the published images, download
the Compose file and start it:

```bash
curl -fsSL -o compose.yaml \
  https://raw.githubusercontent.com/OpenElementsLabs/record-store/v0.3.0/deploy/docker/compose.ghcr.yml
docker compose --env-file .env up -d
```

The `.env` file holds your credentials. The [Quick start](quick-start.md) shows
how to create it.

Both images are public, so you don't need to run `docker login`.

The `RECORD_STORE_VERSION` variable sets the image tag that the Compose file
uses. Its default value is the release that this page documents. To use a
different tag, set the variable when you start Compose:

```bash
RECORD_STORE_VERSION=latest docker compose --env-file .env up -d
```

For more information, see the following pages:

- [Container images](../deployment/container-images.md): the available tags, how
  to choose one, and how to pin an image by digest.
- [Verifying a release](../deployment/verifying-releases.md): how to check where
  an image came from.

## Run Docker Compose from source

The repository includes three Compose files in the `deploy/docker/` directory:

| File | What it runs |
| --- | --- |
| `compose.ghcr.yml` | The server and the console, from the published images. |
| `compose.yml` | The server only, with S3 on port 7600 and management on port 7601. Builds from source. |
| `compose.console.yml` | The server and the web console on port 7602. Builds from source. |

To build and run the server and the console from source, run the following
commands:

```bash
git clone https://github.com/OpenElementsLabs/record-store.git
cd record-store
docker compose -f deploy/docker/compose.console.yml up --build -d
```

!!! warning "Use the source Compose files for development only"
    `compose.yml` and `compose.console.yml` contain development default values,
    marked `change-me`, for every secret. Don't use them in any real
    deployment. `compose.ghcr.yml` contains no default secrets and doesn't start
    until you set every secret.

For more information, see [Docker Compose](../deployment/docker-compose.md).

## Install a Linux package

Record Store provides a `.deb` package and an `.rpm` package for `amd64` and
`arm64`. Each package installs the server, the CLI, a systemd service, and a
configuration file. The package also generates credentials that are unique to
the machine.

To install the package, run the command for your distribution:

```bash
sudo apt-get install ./record-store_0.3.0_amd64.deb    # Debian, Ubuntu
sudo dnf install ./record-store-0.3.0-1.x86_64.rpm     # RHEL, Rocky, Fedora
```

The binaries are statically linked, so the packages have no dependencies. The
service doesn't start until you enable it. For more information, see
[Linux packages](../deployment/linux-packages.md).

## Install on Kubernetes

To install Record Store with Helm, run the following command:

```bash
helm install record-store \
  oci://ghcr.io/openelementslabs/charts/record-store --version 0.3.0 \
  --namespace record-store --create-namespace \
  --set auth.rootAccessKey=admin \
  --set auth.rootSecretKey="$(openssl rand -hex 24)" \
  --set auth.credentialMasterKey="$(openssl rand -hex 32)" \
  --set auth.managementSystemToken="$(openssl rand -hex 32)"
```

This command passes credentials on the command line, which is suitable for a
trial. To configure storage, ingress, availability, and credential storage for
production, see [Kubernetes](../deployment/kubernetes.md).

## Install from a binary archive

Use a binary archive to run Record Store without a package manager or a
container. Each release includes two archives for each architecture:

| Archive | When to use it |
| --- | --- |
| `record-store-0.3.0-linux-amd64-musl.tar.gz` | On any Linux host. Statically linked, with no libc dependency. |
| `record-store-0.3.0-linux-amd64.tar.gz` | When you want the same binaries as the container image. |

```bash
tar xzf record-store-0.3.0-linux-amd64-musl.tar.gz
sudo install -m 0755 record-store record-store-server /usr/local/bin/
record-store --version
```

Both archives contain the `record-store` and `record-store-server` binaries.

1.  Check the archive against the `SHA256SUMS` file from the same release. For
    instructions, see [Verifying a release](../deployment/verifying-releases.md).

1.  Extract the archive and install the binaries:

    ```bash
    tar xzf record-store-0.2.1-linux-amd64-musl.tar.gz
    sudo install -m 0755 record-store record-store-server /usr/local/bin/
    ```

1.  Confirm that the CLI runs:

    ```bash
    record-store --version
    ```

An archive installs only the binaries. It doesn't create a service, a
configuration file, or a user account. To run Record Store as a managed service,
[install a Linux package](#install-a-linux-package) instead.

## Build the container image

Build the image yourself only for development or to test unreleased changes.
For deployments, use the
[published container images](#run-the-published-container-images).

1.  From the repository root, build the image:

    ```bash
    docker build -f deploy/docker/Dockerfile -t record-store .
    ```

1.  Export the credential variables as described in
    [Run the published container images](#run-the-published-container-images),
    and then run the image:

    ```bash
    docker run --read-only \
      -e RECORD_STORE_ROOT_ACCESS_KEY \
      -e RECORD_STORE_ROOT_SECRET_KEY \
      -e RECORD_STORE_CREDENTIAL_MASTER_KEY \
      -e RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN \
      -e RECORD_STORE_STORAGE_ENCRYPTION_ENABLED=true \
      -p 7600:7600 -p 7601:7601 \
      -v record-store-data:/var/lib/record-store \
      record-store
    ```

The image runs as a non-root user, supports a read-only root file system, and
stores data in the `/var/lib/record-store` volume. For more information, see
[Persistent storage](../deployment/persistent-storage.md).

## Build from source

1.  Clone the repository and build a release:

    ```bash
    git clone https://github.com/OpenElementsLabs/record-store.git
    cd record-store
    cargo build --release
    ```

    The build creates two binaries in the `target/release/` directory:

    | Binary | Purpose |
    | --- | --- |
    | `record-store` | The operational CLI. The `record-store server` command also starts the server. |
    | `record-store-server` | The server daemon. Accepts only the `--config` flag. |

1.  Record Store has no built-in credentials and doesn't start until you provide
    them. Set the credentials and start the server:

    ```bash
    export RECORD_STORE_ROOT_ACCESS_KEY='ROOT_ACCESS_KEY'
    export RECORD_STORE_ROOT_SECRET_KEY='ROOT_SECRET_KEY'
    export RECORD_STORE_CREDENTIAL_MASTER_KEY='CREDENTIAL_MASTER_KEY'
    export RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN='MANAGEMENT_SYSTEM_TOKEN'
    ./target/release/record-store server
    ```

    For a description of each placeholder, see
    [Run the published container images](#run-the-published-container-images).

## Run the web console from source

The web console is a separate Next.js application. The console is optional: you
can manage Record Store with the CLI and the management API.

To run the console from source, run the following commands from the repository
root:

```bash
cd console
npm install
RECORD_STORE_API_URL=http://127.0.0.1:7601 npm run dev
```

The console listens on <http://localhost:7602>. For more information, see
[Web console](../guides/web-console.md).

## Check the host before you start

The `doctor` command reports problems that prevent the deployment from working,
such as an unwritable data directory, a port that is in use, a temporary
directory on the wrong file system, or a missing encryption key. It doesn't
start the server and doesn't print any secrets.

To check the host, run the following command:

```bash
record-store server --config /etc/record-store/config.toml doctor
```

The command exits with `0` if every check passes, and with `7` if any check
fails. Each failure includes the action that fixes it, as in the following
example:

```text
FAIL  atomic_publication            /mnt/fast/tmp and /var/lib/record-store/objects are on different filesystems
                                    -> put storage.temporary_directory on the same filesystem as the data
                                       directory; a payload cannot be published atomically across a mount boundary
```

## Verify the installation

To confirm that the server is running, send a request to the health endpoint:

```bash
curl http://127.0.0.1:7601/health
```

If the server is running, the output is the following:

```json
{"status":"ok"}
```

To learn how `/health`, `/ready`, and `doctor` differ, see
[Health and readiness](../operations/health-and-readiness.md).

## What's next

- To create a bucket and store an object, follow the
  [Quick start](quick-start.md).
