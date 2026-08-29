# Authentication Errors

## `SignatureDoesNotMatch`

The signature did not verify. In order of likelihood:

### A proxy rewrote the `Host` header

SigV4 signs `Host`. A proxy that rewrites it invalidates every signature on requests
that are otherwise correct.

```nginx
proxy_set_header Host $host;
```

Test by bypassing the proxy:

```bash
AWS_ACCESS_KEY_ID=<your-access-key> \
AWS_SECRET_ACCESS_KEY=<your-secret-key> \
aws --endpoint-url http://127.0.0.1:7600 s3 ls
```

Works direct and fails through the proxy — it is the proxy.

### The secret is wrong

Whitespace and truncation are the usual culprits:

```bash
echo -n "$AWS_SECRET_ACCESS_KEY" | wc -c
```

Secrets are shown once. If it is lost, rotate:

```bash
record-store credential rotate <account-id> --endpoint <endpoint>
```

### A proxy modified the body

The signature covers a payload hash. Buffering is fine; transforming is not. Disable
any body-rewriting module on the storage route.

### The region does not match

The region is signed. Any value works as long as the client and every tool agree.
`us-east-1` is conventional.

## `InvalidAccessKeyId`

The key is not known.

- Check for a typo, and for the whole key having been copied.
- Confirm the account still exists: `record-store service-account list`.
- Confirm you are pointed at the right deployment — a key from staging fails against
  production with exactly this error.

## `RequestTimeTooSkewed`

The client clock is too far from the server's. Sync both:

```bash
timedatectl status
sudo systemctl restart systemd-timesyncd
```

## `AccessDenied` on S3

Authenticated, and no policy allows the request.

1. Which action does the request map to? See
   [Policies](../administration/policies.md#how-a-request-maps-to-an-action).
2. Which policies are attached?
   ```bash
   record-store service-account inspect <account-id> --endpoint <endpoint>
   ```
3. Do the resources cover it?

The single most common cause: **`bucket:uploads` does not cover
`bucket:uploads/photo.jpg`, and `bucket:uploads/*` does not cover the bucket itself.**
A policy that lists a bucket *and* reads its objects needs both entries.

The second most common: an explicit `deny` somewhere. One matching deny overrides every
allow.

Check what was refused:

```bash
record-store audit \
  --principal service_account:<account-id> \
  --limit 20 \
  --endpoint <endpoint>
```

## Root credential does not work on S3

```bash
RECORD_STORE_ROOT_S3_ENABLED=false
```

That is the intended production setting. Use a service account.

## `401 UNAUTHORIZED` on the management API

- Is `Authorization: Bearer <token>` present and spelled correctly?
- Is the token 32+ characters, and exactly what was configured?
- Does the deployment have management tokens at all? Without them, only root Basic
  authentication works.

Check what the token is:

```bash
curl https://management.example.com/api/v1/auth/session \
  -H "Authorization: Bearer <your-management-token>"
```

## `403 FORBIDDEN` on the management API

The token authenticated, and its role does not permit that route. Usually a storage or
auditor token on a system-only route.

| Route | Requires |
| --- | --- |
| `/api/v1/service-accounts` | system |
| `/api/v1/policies` | system |
| `/api/v1/webhooks` (write) | system |
| `/api/v1/audit` | system or auditor |
| Cluster mutations | system |
| `/api/v1/shares/{id}/url` | system or storage — **never** auditor |

See [Authorization](../security/authorization.md).

## Credential creation fails

Creating or rotating a service-account credential requires the master key:

```bash
RECORD_STORE_CREDENTIAL_MASTER_KEY=<your-master-key>
```

Without it the request fails. Credentials are stored sealed under that key, and
Record Store will not fall back to storing them any other way.

## Temporary credential rejected

- Has it expired? Lifetime is 60–86400 seconds.
- Was it disabled early?
- **Is `AWS_SESSION_TOKEN` set?** Record Store does not issue or expect one. Unset it.

## The console signs you straight out

Behind TLS, set:

```bash
RECORD_STORE_CONSOLE_SECURE_COOKIES=true
```

Without it, the session cookie is not marked `Secure` and the browser discards it.

## Share password not accepted

- Rate limited? `sharing.password_attempts_per_minute` defaults to 10. A `RATE_LIMITED`
  response means wait.
- The unlock lasts `sharing.unlock_lifetime_hours` (default 12) and is scoped to that
  share.
- Check the audit trail for `share.password_failed` and `share.password_throttled`.

## Reading the audit trail

Every authentication failure is recorded with result `denied`:

```bash
record-store audit --limit 50 --endpoint <endpoint>
```

Filter by principal, or by the exact request ID from the failing response. A burst of
denials from one principal is either a broken client or someone probing.
