# Security Checklist

The short version. Deployment specifics are in the
[Production Checklist](../deployment/production-checklist.md).

## Credentials

- [ ] Root credentials are not a development default from any Compose file
- [ ] `RECORD_STORE_ROOT_S3_ENABLED=false` once every application has a service account
- [ ] One service account per application, never one shared
- [ ] Every service account has a policy scoped to what it actually needs
- [ ] No service account holds `bucket:*` unless it genuinely administers everything
- [ ] Credential rotation is scheduled, and the **old credential is disabled** at the
      end of it

## Keys and tokens

- [ ] `RECORD_STORE_CREDENTIAL_MASTER_KEY` is set and 32+ characters
- [ ] **The master key is backed up outside the data directory**
- [ ] The three management role tokens are set, distinct, and 32+ characters
- [ ] `RECORD_STORE_METRICS_SCRAPE_TOKEN` is set and differs from every role token
- [ ] Operators hold the narrowest role that lets them do their job
- [ ] No secret is in a repository, an image layer, or a shell history

The master key cannot be rotated. It is the one item here with no recovery path.

## Network

- [ ] 7601 is not reachable from the internet
- [ ] 7600 and 7602 are behind TLS
- [ ] The proxy preserves the `Host` header
- [ ] The proxy sets `X-Forwarded-For` and overwrites any client-supplied value
- [ ] `RECORD_STORE_CONSOLE_SECURE_COOKIES=true`

Verify rather than assume:

```bash
curl -sS --max-time 5 https://storage.example.com:7601/health || echo "closed, as intended"
```

## Data

- [ ] `storage.encryption_enabled` decided deliberately
- [ ] Object keys do not themselves contain sensitive identifiers — they are not
      encrypted
- [ ] Versioning enabled where accidental overwrite is a real risk
- [ ] Backups are encrypted or stored somewhere access-controlled
- [ ] A restore has actually been tested

## Sharing

- [ ] Shares and embeds are disabled if the deployment does not use them
- [ ] `sharing.maximum_lifetime_days` set to something defensible
- [ ] `sharing.require_expiration` on
- [ ] Embed origins restricted where the consuming site is known
- [ ] Both base URLs set to the correct public hosts
- [ ] Active capabilities reviewed periodically

## Webhooks

- [ ] `webhooks.allow_http` is `false`
- [ ] `webhooks.allow_private_networks` is `false`
- [ ] Receivers verify the `x-record-store-signature` HMAC before parsing
- [ ] Receivers are idempotent on `x-record-store-event-id`

Both flags default to off because a webhook URL is an administrator-supplied,
server-side fetch. Turning them on makes webhook creation a privileged operation.

## Monitoring

- [ ] `/metrics` is scraped with its dedicated token
- [ ] Alerts on error rate and disk space
- [ ] Audit denials are reviewed or alerted on
- [ ] Logs are collected and searchable

```bash
record-store audit --limit 100 --endpoint https://management.example.com
```

## Ongoing

- [ ] Dependencies and the base image are updated on a schedule
- [ ] Release notes are read before upgrading
- [ ] Someone reviews audit denials
- [ ] Access is removed when people leave
- [ ] The runbook records where the master key is kept
