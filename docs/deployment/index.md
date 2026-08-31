# Deployment

Running Record Store somewhere real.

<div class="grid cards" markdown>

-   **[Container Images](container-images.md)** — the published images, tags, and digest pinning
-   **[Docker](docker.md)** — the image, its defaults, and running a container
-   **[Docker Compose](docker-compose.md)** — the server on its own, and with the console
-   **[Coolify](coolify.md)** — end-to-end on a Coolify server
-   **[Reverse Proxy and TLS](reverse-proxy.md)** — what to expose and what to keep private
-   **[Persistent Storage](persistent-storage.md)** — the data directory and its requirements
-   **[Production Checklist](production-checklist.md)** — before you take traffic
-   **[Upgrading](upgrading.md)** — moving to a new version safely
-   **[Verifying a Release](verifying-releases.md)** — checksums, digests, SBOMs, and what is not signed

</div>

## The shape of a deployment

```mermaid
flowchart TB
    subgraph public [Public]
        S["S3 API :7600<br/>applications, embeds"]
        C["Console :7602<br/>administrators, share links"]
    end
    subgraph private [Private]
        M["Management API :7601<br/>CLI, console backend, metrics"]
    end
    C --> M
```

The single most important deployment decision: **7600 and 7602 may face the
internet; 7601 must not.** The management API is unrestricted administrative
access. See [Ports](../reference/ports.md).

## Before you start

Have these ready:

- A data directory on durable storage, backed up
- Root credentials, a credential master key, and a management system token
- A plan for TLS in front of the public ports
- Somewhere to keep the master key that is not the data directory

The master key cannot be rotated. Losing it means losing every stored credential and,
if encryption is enabled, every object. Back it up first, not later.
