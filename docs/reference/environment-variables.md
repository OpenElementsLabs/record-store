# Environment Variables

Every `RECORD_STORE_*` variable the server reads. Environment values are applied
after the TOML file and override it.

## Rules

- A variable that is **set at all** overrides the file, including when it is set to an
  empty string. To fall back to the file value, unset the variable.
- Booleans accept exactly `true` or `false`, lowercase. Anything else fails validation.
- Durations are whole seconds unless the name says otherwise.
- Values must be valid Unicode.
- A parse failure names the variable but never prints its value, so a bad secret does
  not end up in a log.

Some settings are file-only. Those are listed at the end.

## Server

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_MODE` | `standalone`, `cluster`, or `control` | `standalone` |
| `RECORD_STORE_S3_BIND` | S3 API listener | `0.0.0.0:7600` |
| `RECORD_STORE_API_BIND` | Management API listener | `0.0.0.0:7601` |
| `RECORD_STORE_RPC_BIND` | Internal RPC listener | `0.0.0.0:7603` |
| `RECORD_STORE_RPC_ADVERTISE` | Address peers use to reach this node | `rpc_bind` |
| `RECORD_STORE_SHUTDOWN_TIMEOUT_SECONDS` | Graceful drain ceiling, 1–300 | `30` |

## Storage

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_STORAGE_DATA_DIRECTORY` | Root of all durable state | `./data` |
| `RECORD_STORE_STORAGE_TEMPORARY_DIRECTORY` | Incomplete payload staging | `<data>/tmp` |
| `RECORD_STORE_STORAGE_ENCRYPTION_ENABLED` | Encrypt new payloads at rest | `false` |

## Credentials and tokens

All of these are secrets. Inject them from a secret manager or the platform's secret
store, never from a checked-in file.

| Variable | Effect |
| --- | --- |
| `RECORD_STORE_ROOT_ACCESS_KEY` | Bootstrap S3 access key — **required** |
| `RECORD_STORE_ROOT_SECRET_KEY` | Bootstrap S3 secret key — **required** |
| `RECORD_STORE_ROOT_S3_ENABLED` | Whether the root credential may use the S3 API (default `true`) |
| `RECORD_STORE_CREDENTIAL_MASTER_KEY` | Wraps stored credentials, webhook secrets, and object data keys |
| `RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` | Full management access |
| `RECORD_STORE_MANAGEMENT_STORAGE_TOKEN` | Storage administration |
| `RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN` | Read-only, including the audit trail |
| `RECORD_STORE_METRICS_SCRAPE_TOKEN` | Accepted only by `/metrics` |

Length requirements are in the [Configuration Reference](configuration.md#auth).

## Limits

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_MAX_CONCURRENT_OPERATIONS` | Simultaneous storage operations | `256` |
| `RECORD_STORE_MAX_HEADER_BYTES` | Aggregate header bytes accepted by the S3 adapter | `65536` |

## Webhooks

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_WEBHOOK_ALLOW_HTTP` | Permit plain-HTTP targets | `false` |
| `RECORD_STORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS` | Permit loopback and private targets | `false` |
| `RECORD_STORE_WEBHOOK_TIMEOUT_SECONDS` | Per-attempt timeout, 1–300 | `10` |
| `RECORD_STORE_WEBHOOK_MAXIMUM_ATTEMPTS` | Attempts before permanent failure, 1–32 | `6` |
| `RECORD_STORE_WEBHOOK_POLL_INTERVAL_SECONDS` | Queue poll interval, 1–3600 | `2` |

## Lifecycle

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_LIFECYCLE_INTERVAL_SECONDS` | Seconds between passes, 1–86400 | `3600` |
| `RECORD_STORE_LIFECYCLE_BATCH_SIZE` | Entries scanned per rule per pass, 1–1000 | `100` |

## Sharing

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_SHARING_SHARES_ENABLED` | Allow share links | `true` |
| `RECORD_STORE_SHARING_EMBEDS_ENABLED` | Allow embed links | `true` |
| `RECORD_STORE_SHARING_MAXIMUM_LIFETIME_DAYS` | Lifetime ceiling; `0` means none | `365` |
| `RECORD_STORE_SHARING_REQUIRE_EXPIRATION` | Every capability must expire | `false` |
| `RECORD_STORE_SHARING_REQUIRE_PASSWORD` | Every share must have a password | `false` |
| `RECORD_STORE_SHARING_MAXIMUM_ACCESS_COUNT` | Access-budget ceiling | `10000` |
| `RECORD_STORE_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE` | Failed unlock attempts allowed | `10` |
| `RECORD_STORE_SHARING_TOKEN_PROBES_PER_MINUTE` | Unknown-token lookups allowed | `60` |
| `RECORD_STORE_SHARING_UNLOCK_LIFETIME_HOURS` | How long an unlock lasts, 1–168 | `12` |
| `RECORD_STORE_SHARING_PREVIEW_TEXT_LIMIT_BYTES` | Console text preview slice | `1048576` |
| `RECORD_STORE_SHARING_SHARE_BASE_URL` | Public console URL used to build share links | unset |
| `RECORD_STORE_SHARING_EMBED_BASE_URL` | Public storage URL used to build embed links | unset |

## Cluster

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_CLUSTER_SEEDS` | Comma-separated `host:port` list, max 32 | empty |
| `RECORD_STORE_CLUSTER_JOIN_TOKEN` | Single-use join token — secret | unset |
| `RECORD_STORE_CLUSTER_STORAGE_CLASS` | Class this node advertises | `standard` |
| `RECORD_STORE_CLUSTER_FAILURE_DOMAIN` | `key=value,key=value` labels | empty |
| `RECORD_STORE_CLUSTER_S3_ENDPOINT` | Client-facing endpoint this node advertises | unset |
| `RECORD_STORE_CLUSTER_REPLICATION_FACTOR` | Factor used when initializing a cluster, 1–3 | `3` |
| `RECORD_STORE_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT` | Low watermark | `80` |
| `RECORD_STORE_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT` | High watermark | `90` |
| `RECORD_STORE_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT` | Critical watermark | `95` |
| `RECORD_STORE_CLUSTER_MOVEMENT_CONCURRENCY` | Concurrent replica movements, 1–256 | `4` |
| `RECORD_STORE_CLUSTER_MOVEMENT_BYTES_PER_SECOND` | Per-movement throughput ceiling | `67108864` |
| `RECORD_STORE_CLUSTER_RECONCILE_INTERVAL_SECONDS` | Local reconciliation interval, 1–86400 | `300` |

Whitespace around each seed is trimmed and empty entries are dropped, so
`a:7603, b:7603` and `a:7603,b:7603` are equivalent.

### Cluster TLS

| Variable | Effect |
| --- | --- |
| `RECORD_STORE_CLUSTER_TLS_CERTIFICATE` | PEM chain this node presents |
| `RECORD_STORE_CLUSTER_TLS_PRIVATE_KEY` | PEM private key for that chain |
| `RECORD_STORE_CLUSTER_TLS_PEER_CA` | PEM authority used to verify peers |
| `RECORD_STORE_CLUSTER_TLS_CLIENT_CA` | PEM authority for mutual TLS |
| `RECORD_STORE_CLUSTER_TLS_SERVER_NAME` | Handshake server name, when it differs |

## Logging

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_LOG` | `tracing-subscriber` filter expression | `record_store=info` |
| `RECORD_STORE_LOG_JSON` | Emit newline-delimited JSON | `false` |

## Web console

The console is a separate process and reads its own variables.

| Variable | Effect | Default |
| --- | --- | --- |
| `RECORD_STORE_API_URL` | Management API the console talks to | `http://127.0.0.1:7601` |
| `RECORD_STORE_CONSOLE_SECURE_COOKIES` | Mark session cookies `Secure` | unset |
| `PORT` | Console listener port | `7602` |

Set `RECORD_STORE_CONSOLE_SECURE_COOKIES` whenever the console is served over HTTPS.

## File-only settings

These have no environment variable. Use a TOML file.

| Setting | Section |
| --- | --- |
| `maximum_custom_metadata_entries` | `[limits]` |
| `maximum_custom_metadata_bytes` | `[limits]` |
| `consensus_heartbeat_millis` | `[cluster]` |
| `election_timeout_min_millis` | `[cluster]` |
| `election_timeout_max_millis` | `[cluster]` |
| `snapshot_logs_threshold` | `[cluster]` |
| `retained_logs` | `[cluster]` |
