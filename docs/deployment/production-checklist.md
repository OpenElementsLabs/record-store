# Production Checklist

Work through this before the deployment takes real traffic.

## Credentials and keys

- [ ] Root credentials are not the development defaults from any Compose file
- [ ] `RECORD_STORE_CREDENTIAL_MASTER_KEY` is set, 32–1024 visible ASCII characters
- [ ] **The master key is backed up somewhere that is not the data directory**
- [ ] Every application has its own service account; none uses the root credential
- [ ] `RECORD_STORE_ROOT_S3_ENABLED=false` once service accounts are in place
- [ ] Management role tokens are set, distinct, and 32+ characters
- [ ] `RECORD_STORE_METRICS_SCRAPE_TOKEN` is set and differs from every role token
- [ ] No secret appears in a checked-in file, an image layer, or a shell history

The master key cannot be rotated. If it is lost, every stored credential and — with
encryption on — every object becomes permanently unreadable. This is the one item on
this list with no recovery path.

## Network

- [ ] 7601 is not reachable from the internet
- [ ] 7603 is not published at all, or is restricted to cluster peers only
- [ ] 7600 and 7602 are behind TLS
- [ ] The proxy preserves the `Host` header
- [ ] The proxy's request body limit is raised or removed
- [ ] The proxy sets `X-Forwarded-For` and overwrites any client-supplied value

Verify rather than assume:

```bash
curl -sS --max-time 5 https://storage.example.com:7601/health || echo "closed, as intended"
```

## Configuration

- [ ] `record-store server check-config` passes
- [ ] `storage.data_directory` is on durable block storage, not a network filesystem
- [ ] `RECORD_STORE_CONSOLE_SECURE_COOKIES=true`
- [ ] `RECORD_STORE_SHARING_SHARE_BASE_URL` is the public console URL
- [ ] `RECORD_STORE_SHARING_EMBED_BASE_URL` is the public storage URL
- [ ] `RECORD_STORE_LOG_JSON=true` if logs are collected by a parser
- [ ] `storage.encryption_enabled` decided deliberately, on or off

Test the links rather than trusting the configuration — create a share link and open
it from a machine that is not the server.

## Data

- [ ] Versioning enabled on buckets whose history matters
- [ ] Lifecycle rules exist for buckets that would otherwise grow forever
- [ ] Quotas set on buckets that could starve the rest of the deployment
- [ ] Backups run on a schedule, covering both `metadata/` and `objects/`
- [ ] **A restore has been performed into a scratch environment**

An untested backup is a belief, not a backup.

## Monitoring

- [ ] Prometheus scrapes `/metrics` with the scrape token
- [ ] Alerts on disk space, error rate, and — in a cluster — quorum health and
      under-replication
- [ ] Logs are collected and searchable
- [ ] Container healthchecks are wired to your orchestrator's restart policy

Suggested alert rules are in [Metrics](../administration/metrics.md).

## Sharing policy

If share and embed links are enabled, decide the deployment-wide ceilings:

- [ ] `sharing.maximum_lifetime_days` set to something you would defend
- [ ] `sharing.require_expiration` on, unless permanent links are a requirement
- [ ] `sharing.require_share_password` decided
- [ ] Shares or embeds disabled entirely if the deployment does not need them

These bound what your most careless administrator can create. See
[Sharing Security](../security/sharing-security.md).

## Cluster

- [ ] At least three storage nodes
- [ ] Distinct `RECORD_STORE_CLUSTER_FAILURE_DOMAIN` per node, reflecting real
      physical separation
- [ ] `RECORD_STORE_RPC_ADVERTISE` set to an address peers can actually reach
- [ ] Internal TLS configured, if cluster traffic crosses a network you do not control
- [ ] `record-store cluster status` reports every node healthy
- [ ] A node failure has been rehearsed

See [Creating a Cluster](../cluster/creating-a-cluster.md).

## Operational readiness

- [ ] Someone other than the person who built it can restart the deployment
- [ ] The runbook says where the master key is kept
- [ ] Upgrades have been rehearsed on a non-production deployment
- [ ] Log and audit retention are decided — neither is pruned automatically

## Final verification

```bash
# Ready and correctly configured
record-store status --endpoint http://127.0.0.1:7601

# Round-trip through the public endpoint
aws --endpoint-url https://storage.example.com s3 mb s3://smoke-test
echo "hello" > /tmp/smoke.txt
aws --endpoint-url https://storage.example.com s3 cp /tmp/smoke.txt s3://smoke-test/
aws --endpoint-url https://storage.example.com s3 cp s3://smoke-test/smoke.txt -
aws --endpoint-url https://storage.example.com s3 rm s3://smoke-test/smoke.txt
aws --endpoint-url https://storage.example.com s3 rb s3://smoke-test

# The audit trail recorded all of it
record-store audit --limit 20 --endpoint http://127.0.0.1:7601
```
