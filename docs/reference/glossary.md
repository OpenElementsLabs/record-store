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

**Credential master key** — `RECORD_STORE_CREDENTIAL_MASTER_KEY`. Seals stored
credentials, webhook secrets, capability secrets, and per-object data keys. **Cannot be
rotated.**

**Delete marker** — On a versioned bucket, the version a delete writes. The object
appears gone; its history is retained.

**Embed link** — A capability delivering raw object bytes at `/e/<token>` on the
**storage** endpoint, intended for a website. May carry an origin allowlist. See
[Embed Links](../guides/embed-links.md).

**ETag** — The MD5-based entity tag S3 clients use for conditional requests.

**Lifecycle rule** — A per-bucket rule expiring objects or non-current versions by age.
See [Lifecycle Rules](../administration/lifecycle-rules.md).

**Logical bytes** — What users think they have: current object versions. Quotas enforce
on these.

**Management token** — A bearer token for the management API, carrying one of three
roles.

**Multipart upload** — Uploading a large object as bounded parts, completed as one
object. See [Multipart Uploads](../guides/multipart-uploads.md).

**Non-current version** — Any version of an object other than the current one.

**Object** — Bytes plus metadata, addressed by a key within a bucket.

**Object key** — The full path-like name of an object within its bucket. Not encrypted.

**Physical bytes** — What the disk actually holds: current versions, history, and
multipart parts.

**Policy** — A named list of statements granting or refusing actions on `bucket:`
resources. See [Policies](../administration/policies.md).

**Presigned URL** — A URL carrying a SigV4 signature in its query string, authorizing
one operation on one object until it expires. Maximum 7 days.

**Root credential** — The bootstrap S3 and management identity from configuration. It
bypasses policy evaluation.

**Service account** — The identity an application uses. Holds credentials, gets
permissions from attached policies. See
[Service Accounts](../administration/service-accounts.md).

**Share link** — A capability delivering a page at `/s/<token>` on the **console**,
intended for a person. May carry a password and an access budget. See
[Share Links](../guides/share-links.md).

**SigV4** — AWS Signature Version 4, the only signing method Record Store accepts.

**Temporary credential** — A service-account credential with an expiry, 60–86400
seconds. No session token is involved.

**Version ID** — The identifier of one immutable version of an object.

**Versioning state** — `disabled`, `enabled`, or `suspended`. `enabled` cannot go back
to `disabled`. See [Versioning](../concepts/versioning.md).

**Version mode** — Whether a capability resolves to the current version
(`follow_current`) or a fixed one (`pinned`).

**Webhook** — An HTTP endpoint receiving signed storage events. See
[Events and Webhooks](../administration/events-and-webhooks.md).

