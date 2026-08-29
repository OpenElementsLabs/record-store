# Architecture

Record Store is one Rust workspace. Protocol crates call shared application services;
they never reach into storage internals directly.

## Standalone request path

```mermaid
flowchart TB
    Client[S3 client] -->|SigV4 :7600| S3[S3 protocol]
    Console[Console :7602] -->|Bearer token :7601| API[Management API]
    S3 --> Svc[Object and bucket services]
    API --> Svc
    Svc --> Store[Filesystem store]
    Svc --> Meta[(Metadata catalog)]
    Store --> Disk[(objects/)]
```

Both protocol surfaces go through the same service layer, so an object written over S3
and an object written through the console are the same object under the same rules.

## Cluster request path

In cluster mode the service layer is backed by replicated storage and a strongly
consistent metadata repository. The protocol crates are unchanged.

```mermaid
flowchart TB
    Client[S3 client] --> Node[Any storage node]
    Node --> Svc[Object services]
    Svc --> Repl[Distributed object store]
    Repl -->|bounded gRPC streams :7603| Peers[Peer replica stores]
    Svc --> RMeta[Replicated metadata]
    RMeta --> Raft[(Raft metadata group)]
```

Object bytes never enter the Raft log. Consensus carries metadata and placement
decisions; payloads travel over authenticated gRPC streams directly between nodes.

## Console path

```mermaid
flowchart LR
    Browser --> Console[Console server :7602]
    Console -->|Bearer token| API[Management API :7601]
    API --> RS[Record Store]
```

The browser only ever talks to the console's own origin. The console server attaches
the management credential server-side, so it lives in an HTTP-only cookie the page
cannot read, no CORS configuration is required, and the browser never reaches the
management API, storage, metadata, or the internal RPC port.

## Crates

| Crate | Responsibility |
| --- | --- |
| `record-store-core` | Validated domain model: names, keys, checksums, ranges |
| `record-store-service` | Shared bucket and object application services |
| `record-store-s3` | S3 protocol, SigV4, XML, multipart, versioning |
| `record-store-api` | Management HTTP API and management roles |
| `record-store-storage` | Streaming filesystem backend and recovery journal |
| `record-store-metadata` | Durable indexed catalog and ordered migrations |
| `record-store-auth` | Encrypted credentials and authorization policies |
| `record-store-audit` | Durable bounded security audit trail |
| `record-store-sharing` | Share and embed capabilities |
| `record-store-events` | Durable events and signed webhook delivery |
| `record-store-lifecycle` | Incremental expiration worker |
| `record-store-cluster` | Membership, placement, health, movement model |
| `record-store-consensus` | Persistent Raft metadata state machine |
| `record-store-replication` | Distributed reads/writes, repair, rebalance |
| `record-store-rpc` | Authenticated gRPC consensus and replica transport |
| `record-store-config` | Configuration loading and validation |
| `record-store-observability` | Structured tracing initialization |
| `record-store-protocol` | Versioned Protobuf internal contracts |
| `record-store-erasure` | Reed-Solomon library, **not currently wired in** |

See [Repository Structure](../contributing/repository-structure.md) for the layout
including applications and the console.

## On-disk layout

```text
<data-directory>/
├── node-identity.json                     # cluster mode
├── node-credential.json                   # cluster mode, 0600 on Unix
├── metadata/catalog.redb                  # standalone mode
├── metadata/consensus/consensus-log.redb  # cluster mode
├── metadata/consensus/consensus-state.redb
├── metadata/consensus/snapshots/
├── metadata/credentials.redb
├── metadata/audit.redb
├── metadata/events.redb
├── metadata/lifecycle.redb
├── objects/<2 hex>/<2 hex>/<object UUID>
├── system/
└── tmp/
```

Payloads are addressed by generated UUID. No bucket name or object key ever becomes
part of a filesystem path.
