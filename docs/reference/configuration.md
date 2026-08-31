# Configuration Reference

Every setting Record Store accepts, its default, and its accepted range.

## How configuration is resolved

Three layers, applied in order. Each one overrides the last.

1. Built-in defaults
2. The TOML file passed with `--config`, if any
3. `RECORD_STORE_*` environment variables

The result is validated as a whole. If validation fails the process reports every
problem at once and exits — it does not start in a half-configured state.

Unknown keys are rejected. A typo in a TOML key is an error, not a silently ignored
line.

Check a file without starting the server:

```bash
record-store server --config /etc/record-store/config.toml check-config
```

Not every setting has an environment variable. Where the "Environment" column is
empty, TOML is the only way to set it. See
[Environment Variables](environment-variables.md) for the complete variable list.

## `[server]`

| Key | Type | Default | Environment |
| --- | --- | --- | --- |
| `s3_bind` | socket address | `0.0.0.0:7600` | `RECORD_STORE_S3_BIND` |
| `api_bind` | socket address | `0.0.0.0:7601` | `RECORD_STORE_API_BIND` |
| `shutdown_grace_period_seconds` | integer 1–300 | `30` | `RECORD_STORE_SHUTDOWN_TIMEOUT_SECONDS` |

Constraints:

- The listeners must differ from each other.
- None may use port `7602`, which is reserved for the web console.

`api_bind` is unrestricted administrative access. Do not publish it. See
[Ports](ports.md).

## `[storage]`

| Key | Type | Default | Environment |
| --- | --- | --- | --- |
| `data_directory` | path | `./data` | `RECORD_STORE_STORAGE_DATA_DIRECTORY` |
| `temporary_directory` | path | `<data_directory>/tmp` | `RECORD_STORE_STORAGE_TEMPORARY_DIRECTORY` |
| `encryption_enabled` | boolean | `false` | `RECORD_STORE_STORAGE_ENCRYPTION_ENABLED` |

`encryption_enabled` requires `auth.credential_master_key`. It applies to newly
committed payloads; it does not re-encrypt existing objects. See
[Encryption](../security/encryption.md).

## `[auth]`

| Key | Type | Default | Environment |
| --- | --- | --- | --- |
| `root_access_key` | string | none — required | `RECORD_STORE_ROOT_ACCESS_KEY` |
| `root_secret_key` | secret | none — required | `RECORD_STORE_ROOT_SECRET_KEY` |
| `root_s3_enabled` | boolean | `true` | `RECORD_STORE_ROOT_S3_ENABLED` |
| `credential_master_key` | secret | none | `RECORD_STORE_CREDENTIAL_MASTER_KEY` |
| `management_system_token` | secret | none | `RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` |
| `management_storage_token` | secret | none | `RECORD_STORE_MANAGEMENT_STORAGE_TOKEN` |
| `management_auditor_token` | secret | none | `RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN` |
| `metrics_scrape_token` | secret | none | `RECORD_STORE_METRICS_SCRAPE_TOKEN` |

Constraints:

- Root credentials are required. The server will not start without both.
- `root_access_key`: 3–128 characters, ASCII letters, digits, `-`, `_`, `.`
- `root_secret_key`: 16–256 visible ASCII characters
- `credential_master_key`: 32–1024 visible ASCII characters
- Each management and metrics token: 32–1024 visible ASCII characters
- `management_system_token` is required if either other role token is set
- The three role tokens must be distinct from one another
- `metrics_scrape_token` must differ from every role token

!!! danger "Changing `credential_master_key` is not reversible"
    It wraps stored credentials, webhook secrets, and — when encryption is on —
    per-object data keys. Replacing it makes everything sealed under the previous
    key unreadable. Treat it as permanent for the life of the deployment.

## `[limits]`

| Key | Type | Default | Environment |
| --- | --- | --- | --- |
| `maximum_concurrent_operations` | integer > 0 | `256` | `RECORD_STORE_MAX_CONCURRENT_OPERATIONS` |
| `maximum_custom_metadata_entries` | integer ≤ 1024 | `64` | — |
| `maximum_custom_metadata_bytes` | integer 1–1048576 | `16384` | — |
| `maximum_header_bytes` | integer 1024–1048576 | `65536` | `RECORD_STORE_MAX_HEADER_BYTES` |

`maximum_custom_metadata_*` bound `x-amz-meta-*` on a single object.

## `[webhooks]`

| Key | Type | Default | Environment |
| --- | --- | --- | --- |
| `allow_http` | boolean | `false` | `RECORD_STORE_WEBHOOK_ALLOW_HTTP` |
| `allow_private_networks` | boolean | `false` | `RECORD_STORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS` |
| `request_timeout_seconds` | integer 1–300 | `10` | `RECORD_STORE_WEBHOOK_TIMEOUT_SECONDS` |
| `maximum_attempts` | integer 1–32 | `6` | `RECORD_STORE_WEBHOOK_MAXIMUM_ATTEMPTS` |
| `poll_interval_seconds` | integer 1–3600 | `2` | `RECORD_STORE_WEBHOOK_POLL_INTERVAL_SECONDS` |

`allow_http` and `allow_private_networks` default to off deliberately: a webhook
target is a URL an administrator supplies, and without these guards it can be aimed
at loopback or link-local addresses. Turn them on only for development or a
deliberately internal receiver. See
[Events and Webhooks](../administration/events-and-webhooks.md).

## `[lifecycle]`

| Key | Type | Default | Environment |
| --- | --- | --- | --- |
| `interval_seconds` | integer 1–86400 | `3600` | `RECORD_STORE_LIFECYCLE_INTERVAL_SECONDS` |
| `batch_size` | integer 1–1000 | `100` | `RECORD_STORE_LIFECYCLE_BATCH_SIZE` |

`batch_size` bounds one pass per rule, not total work. See
[Lifecycle Rules](../administration/lifecycle-rules.md).

## `[sharing]`

| Key | Type | Default | Environment |
| --- | --- | --- | --- |
| `shares_enabled` | boolean | `true` | `RECORD_STORE_SHARING_SHARES_ENABLED` |
| `embeds_enabled` | boolean | `true` | `RECORD_STORE_SHARING_EMBEDS_ENABLED` |
| `maximum_lifetime_days` | integer 0–3650 | `365` | `RECORD_STORE_SHARING_MAXIMUM_LIFETIME_DAYS` |
| `require_expiration` | boolean | `false` | `RECORD_STORE_SHARING_REQUIRE_EXPIRATION` |
| `require_share_password` | boolean | `false` | `RECORD_STORE_SHARING_REQUIRE_PASSWORD` |
| `maximum_access_count` | integer 1–1000000 | `10000` | `RECORD_STORE_SHARING_MAXIMUM_ACCESS_COUNT` |
| `password_attempts_per_minute` | integer 1–1000 | `10` | `RECORD_STORE_SHARING_PASSWORD_ATTEMPTS_PER_MINUTE` |
| `token_probes_per_minute` | integer 1–100000 | `60` | `RECORD_STORE_SHARING_TOKEN_PROBES_PER_MINUTE` |
| `unlock_lifetime_hours` | integer 1–168 | `12` | `RECORD_STORE_SHARING_UNLOCK_LIFETIME_HOURS` |
| `preview_text_limit_bytes` | integer 1024–67108864 | `1048576` | `RECORD_STORE_SHARING_PREVIEW_TEXT_LIMIT_BYTES` |
| `share_base_url` | absolute URL | none | `RECORD_STORE_SHARING_SHARE_BASE_URL` |
| `embed_base_url` | absolute URL | none | `RECORD_STORE_SHARING_EMBED_BASE_URL` |

`maximum_lifetime_days = 0` means no ceiling. That is an opt-in, not the default: a
capability that never expires is one somebody has to track forever.

Base URLs must start with `http://` or `https://`, contain no whitespace, and stay
under 512 bytes.

The two base URLs are different addresses on purpose:

- `share_base_url` is the **console**, because a share link is a page a person opens.
- `embed_base_url` is the **storage endpoint**, because an embed serves object bytes
  into somebody else's page.

When `embed_base_url` is unset, Record Store falls back to the S3 listener address,
rendered as loopback when the bind address is unspecified. That fallback is wrong
behind a proxy. Set it explicitly in production. See
[Sharing Security](../security/sharing-security.md).

## `[observability]`

| Key | Type | Default | Environment |
| --- | --- | --- | --- |
| `log_filter` | tracing filter | `record_store=info` | `RECORD_STORE_LOG` |
| `json` | boolean | `false` | `RECORD_STORE_LOG_JSON` |

`log_filter` must not be empty. See [Monitoring](../operations/monitoring.md).

## Complete example

A deployment behind a reverse proxy. Secrets come from the environment,
not from the file.

```toml
[server]
s3_bind = "0.0.0.0:7600"
api_bind = "127.0.0.1:7601"
shutdown_grace_period_seconds = 30

[storage]
data_directory = "/var/lib/record-store"
encryption_enabled = true

[limits]
maximum_concurrent_operations = 256
maximum_header_bytes = 65536

[webhooks]
allow_http = false
allow_private_networks = false
maximum_attempts = 6

[lifecycle]
interval_seconds = 3600
batch_size = 100

[sharing]
require_expiration = true
maximum_lifetime_days = 30
share_base_url = "https://console.example.com"
embed_base_url = "https://storage.example.com"

[observability]
log_filter = "record_store=info"
json = true
```

Secrets stay out of the file:

```bash
RECORD_STORE_ROOT_ACCESS_KEY=<your-access-key>
RECORD_STORE_ROOT_SECRET_KEY=<your-secret-key>
RECORD_STORE_CREDENTIAL_MASTER_KEY=<your-master-key>
RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=<your-system-token>
```
