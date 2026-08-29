# Temporary Credentials

A temporary credential is a normal service-account credential with an expiry. After
the expiry it stops authenticating, with nothing to clean up.

## When to use one

| Need | Use |
| --- | --- |
| One browser upload or download | [Presigned URL](../guides/presigned-urls.md) |
| A person needs to open or download a file | [Share link](../guides/share-links.md) |
| A short-lived job needs the S3 API for many operations | Temporary credential |
| A long-running service | Ordinary service-account credential |

A presigned URL covers one object and one operation. A temporary credential is a real
key: it can do anything the account's [policies](policies.md) allow, for as long as it
lives. Prefer the narrower tool.

## Issue one

```bash
record-store credential temporary <account-id> \
  --expires-in-seconds 3600 \
  --endpoint https://management.example.com
```

`--expires-in-seconds` defaults to 3600 and must be between **60 and 86400** (one
minute to one day). Values outside that range are refused.

The response contains an access key, a secret key, and the expiry. The secret is shown
once.

## Inherited permissions

A temporary credential belongs to its service account and inherits that account's
policies exactly. It cannot be given a narrower scope at issue time.

That has a consequence worth planning around: to hand out short-lived access to *one
prefix*, create a service account whose policy covers only that prefix, then issue
temporary credentials from it. Issuing them from a broadly-privileged account gives
the holder everything that account can do.

## Using one

There is no session token. A temporary credential is an access key and a secret key,
signed exactly like any other:

```bash
AWS_ACCESS_KEY_ID=<temporary access key> \
AWS_SECRET_ACCESS_KEY=<temporary secret key> \
aws --endpoint-url https://storage.example.com s3 ls s3://uploads/
```

Do not set `AWS_SESSION_TOKEN`. Record Store does not issue or expect one.

## Expiry

Expiry is checked at authentication time. Once it passes, requests fail with
`403 AccessDenied`; in-flight requests are not interrupted.

Expired credentials remain visible under `service-account inspect` so the history of
what was issued stays readable.

## Revoking early

Expiry is not the only exit. Disable the credential directly:

```bash
record-store credential disable <account-id> <credential-id> \
  --endpoint https://management.example.com
```

Get the credential ID from `service-account inspect`.

## Choosing a lifetime

Match the work, not the convenience. A nightly batch job that finishes in twenty
minutes does not need 24 hours. If a job routinely outlives its credential, that is a
signal it should hold an ordinary credential and rotate on a schedule instead.

Every issuance is recorded in the [audit log](audit-log.md).
