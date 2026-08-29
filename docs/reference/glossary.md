# Glossary

**Access key** — The public half of an S3 credential. Record Store issues
service-account keys as `SA` followed by 20 uppercase hexadecimal characters.

**Audit log** — The durable, append-only record of security and administrative actions.
Separate from process logs and never pruned. See
[Audit Log](../administration/audit-log.md).

**Bucket** — A named container for objects. Names are 3–63 bytes with
[restricted characters](s3-compatibility.md#bucket-names).

**Capability** — A share link or embed link. The token in its URL is the entire
authorization; there is no account behind it.

**Control node** — A cluster member that serves the management API and holds no object
replicas.

**Credential master key** — `RECORD_STORE_CREDENTIAL_MASTER_KEY`. Seals stored
credentials, webhook secrets, capability secrets, and per-object data keys. **Cannot be
rotated.**

**Delete marker** — On a versioned bucket, the version a delete writes. The object
appears gone; its history is retained.

**Deployment mode** — `standalone`, `cluster`, or `control`.

**Embed link** — A capability delivering raw object bytes at `/e/<token>` on the
**storage** endpoint, intended for a website. May carry an origin allowlist. See
[Embed Links](../guides/embed-links.md).

**ETag** — The MD5-based entity tag S3 clients use for conditional requests.

**Failure domain** — `key=value` labels describing what fails together. Placement
spreads replicas across them. See [Replication](../cluster/replication.md).

**Failure-domain scope** — Which label placement groups by: `node`, `host`, `rack`
(default), `zone`, or `region`.

**Join token** — A single-use, short-lived token authorizing one node to join a cluster.
60–86400 seconds.

**Lifecycle rule** — A per-bucket rule expiring objects or non-current versions by age.
See [Lifecycle Rules](../administration/lifecycle-rules.md).

**Logical bytes** — What users think they have: current object versions. Quotas enforce
on these.

**Management token** — A bearer token for the management API, carrying one of three
roles.

**Metadata plane** — Buckets, objects, versions, membership, and placement, replicated
through Raft consensus.

**Multipart upload** — Uploading a large object as bounded parts, completed as one
object. See [Multipart Uploads](../guides/multipart-uploads.md).

**Non-current version** — Any version of an object other than the current one.

**Object** — Bytes plus metadata, addressed by a key within a bucket.

**Object key** — The full path-like name of an object within its bucket. Not encrypted.

**Physical bytes** — What the disk actually holds: current versions, history, and
multipart parts.

**Placement** — Deciding which nodes hold an object's replicas.

**Policy** — A named list of statements granting or refusing actions on `bucket:`
resources. See [Policies](../administration/policies.md).

**Presigned URL** — A URL carrying a SigV4 signature in its query string, authorizing
one operation on one object until it expires. Maximum 7 days.

**Quorum** — The majority of consensus voters required to commit. `members / 2 + 1`.

**Rebalance** — Moving replicas to even out utilization. Off by default. See
[Repair and Rebalance](../cluster/repair-and-rebalance.md).

**Repair** — Restoring lost redundancy by copying replicas to healthy nodes. Automatic.

**Replica** — One copy of an object payload on one node.

**Replication factor** — Copies of each payload. 1–3, default 3. Fixed at cluster
initialization.

**Root credential** — The bootstrap S3 and management identity from configuration. It
bypasses policy evaluation.

**Service account** — The identity an application uses. Holds credentials, gets
permissions from attached policies. See
[Service Accounts](../administration/service-accounts.md).

**Share link** — A capability delivering a page at `/s/<token>` on the **console**,
intended for a person. May carry a password and an access budget. See
[Share Links](../guides/share-links.md).

**SigV4** — AWS Signature Version 4, the only signing method Record Store accepts.

**Storage class** — A label a node advertises. Placement matches a request's requested
class against it. Default `standard`.

**Temporary credential** — A service-account credential with an expiry, 60–86400
seconds. No session token is involved.

**Under-replicated** — A payload with fewer healthy replicas than its replication
factor.

**Unavailable payload** — A payload with **no** healthy replica. Unreadable.

**Version ID** — The identifier of one immutable version of an object.

**Versioning state** — `disabled`, `enabled`, or `suspended`. `enabled` cannot go back
to `disabled`. See [Versioning](../concepts/versioning.md).

**Version mode** — Whether a capability resolves to the current version
(`follow_current`) or a fixed one (`pinned`).

**Voter** — A cluster member that votes in metadata consensus. The target count is odd,
default 3.

**Watermark** — A capacity threshold — low, high, critical — governing whether a node
accepts new placement.

**Webhook** — An HTTP endpoint receiving signed storage events. See
[Events and Webhooks](../administration/events-and-webhooks.md).

**Write acknowledgement** — How many replicas must be durable before a write is
acknowledged: `Quorum` (default), `All`, or a count.
