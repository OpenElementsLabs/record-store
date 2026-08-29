# Authentication

## S3 API

The S3 API accepts **AWS Signature Version 4** and nothing else. Every request is
signed with an access key and a secret key.

Two identities can sign:

| Identity | Source | Use |
| --- | --- | --- |
| Root credential | Configuration | Bootstrap only |
| Service account credential | Created through the management API | Applications |

Every SDK does this for you. See [Application Integration](../guides/application-integration.md).

### Root credential

```bash
RECORD_STORE_ROOT_ACCESS_KEY=<your-access-key>
RECORD_STORE_ROOT_SECRET_KEY=<your-secret-key>
```

Required — the server will not start without both. The access key is 3–128 characters
of ASCII letters, digits, `-`, `_`, or `.`; the secret is 16–256 visible ASCII
characters.

Root bypasses policy evaluation entirely. Use it to create the first service account,
then take it out of circulation:

```bash
RECORD_STORE_ROOT_S3_ENABLED=false
```

That leaves root usable for management and closed on the data plane.

### Service account credentials

Created through the management API. Access keys are issued as `SA` followed by 20
uppercase hexadecimal characters, so they are recognisable in a log.

Creating or rotating one requires `auth.credential_master_key` — credentials are stored
sealed under it, and Record Store will not fall back to storing them any other way.

Secrets are shown once. See [Service Accounts](../administration/service-accounts.md).

### Presigned URLs

A presigned URL carries the signature in the query string. Anyone holding the URL can
perform that one operation on that one object until it expires.

The maximum lifetime is 7 days (604800 seconds), matching SigV4's own limit.

See [Presigned URLs](../guides/presigned-urls.md).

### Temporary credentials

A service-account credential with an expiry, between 60 and 86400 seconds. There is no
session token — sign exactly as with any other credential, and do not set
`AWS_SESSION_TOKEN`.

See [Temporary Credentials](../administration/temporary-credentials.md).

## Management API

Bearer tokens, one per role:

```bash
RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=<your-system-token>
RECORD_STORE_MANAGEMENT_STORAGE_TOKEN=<your-storage-token>
RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN=<your-auditor-token>
```

```bash
curl https://management.example.com/api/v1/buckets \
  -H "Authorization: Bearer <your-management-token>"
```

Rules:

- 32–1024 visible ASCII characters each
- The three must be distinct
- `management_system_token` is required if either other role token is set
- Tokens are compared by SHA-256 digest in constant time

The CLI reads `RECORD_STORE_MANAGEMENT_TOKEN`:

```bash
RECORD_STORE_MANAGEMENT_TOKEN=<your-system-token> \
  record-store bucket list --endpoint https://management.example.com
```

### Root basic authentication

The management API also accepts HTTP Basic authentication with the root credential,
granting the system-administrator role. It exists so a deployment with no management
tokens configured is still administrable.

Configure real role tokens and use those. Basic auth sends the credential on every
request and offers no way to grant anything narrower than full access.

The CLI falls back to it when `RECORD_STORE_MANAGEMENT_TOKEN` is unset and
`RECORD_STORE_ROOT_ACCESS_KEY` / `RECORD_STORE_ROOT_SECRET_KEY` are present.

## Metrics endpoint

`/metrics` takes its own token and is **closed when none is configured**:

```bash
RECORD_STORE_METRICS_SCRAPE_TOKEN=<your-metrics-token>
```

It must differ from every management role token. A scrape credential lives in a
monitoring system's configuration and should carry no authority over management
routes.

## Web console

The console authenticates an operator with a management token and holds a session
cookie. Behind TLS:

```bash
RECORD_STORE_CONSOLE_SECURE_COOKIES=true
```

Without it the cookie is not marked `Secure`. The usual symptom is signing in and being
signed straight back out.

The console's server calls the management API; the browser never does.

## Public capability links

Share and embed links carry a token in the URL path. The token **is** the
authorization — there is no account behind it.

Every request re-checks the token against durable state, so a revocation takes effect
on the next request rather than at the next cache expiry.

See [Sharing Security](sharing-security.md).

## Failures

| Response | Meaning |
| --- | --- |
| `401` | No credential, or one that is not recognised |
| `403` | Authenticated, but not permitted |

Both are recorded in the [audit log](../administration/audit-log.md) with result
`denied`. A burst of them from one principal is either a broken client or someone
probing.

Troubleshooting: [Authentication Errors](../troubleshooting/authentication.md).
