<!--
Draft announcement post. Not part of the documentation site; publish it to the
blog, mailing list, or forum it is meant for and delete or update this file when
the facts move on.

House rule for this text: no comparative claims about other projects. Say what
Record Store does and does not do, and let a reader draw their own comparison.
-->

# Record Store: a self-hosted records store

We are publishing [Record Store](https://github.com/OpenElementsLabs/record-store),
a self-hosted, S3-compatible **records store**, under Apache-2.0.

It is S3-compatible, so existing clients and SDKs work against it. But the job it
is built for is narrower than general object storage: keeping one authoritative
copy of something and being able to show, later and to somebody else, that it is
unchanged and who touched it.

## The problem it is for

Plenty of systems can store a file. Fewer can answer the question that comes
afterwards, sometimes years afterwards:

> Is this the document we filed? Has it been altered? Who touched it, and when?

That question turns up in regulated retention, in contract and invoice archives, in
research data, in anything that might one day be produced as evidence. Answering it
needs more than durable bytes. It needs the store to refuse deletions it has been
told to refuse, and to produce something a third party can check without trusting
the person running the server.

## What it does today

**One authoritative copy.** Payloads are immutable and addressed by generated
identifiers. Versioning keeps history instead of overwriting it. An object written
over the S3 API and one written through the console are the same object under the
same rules — there is no second copy to drift.

**Object Lock, with AWS semantics.** `GOVERNANCE` and `COMPLIANCE` retention, legal
holds, and per-bucket defaults. A `COMPLIANCE` retention can only be extended: not
shortened, not deleted, not by the root credential, not by anyone. A `GOVERNANCE`
retention yields only to a caller holding a separate `s3:BypassGovernanceRetention`
permission, and every bypass is written to the audit trail whether it succeeds or
is refused.

Enforcement lives inside the metadata transaction that would remove the version, so
a retention placed a moment earlier cannot be raced by a delete arriving a moment
later.

**Portable proof bundles.** For any object version, the server emits a signed JSON
document describing it: identity, size, the SHA-256 recorded at write time, the
deployment's public verification key. Then:

```console
$ record-store verify proof ./statement.proof.json --object ./statement.pdf
  [ok] bundle signature: the bundle has not been altered since it was signed
  [ok] payload digest: the file matches the SHA-256 recorded at write time
  [not proved] deployment identity: … no expected key was supplied …
  [not proved] audit history: this bundle carries no audit history …

VERIFIED: every check that could be performed passed.
This does NOT establish:
  - deployment identity
  - audit history
```

That runs offline. No server, no network, no credential — the file and the bundle
are enough. The bundle carries no credentials, no capability tokens, and not the
payload, so it is safe to hand to whoever needs to check.

The output is deliberately shaped that way. Every check reports its own status, and
anything that could not be performed is listed as **not proved** rather than
omitted. A verifier that prints a green tick after checking a digest, while quietly
skipping the parts it had no data for, teaches people to trust a word that did not
mean what they thought.

**Share and embed links.** A share link gives a person read access to one object
through a page the server renders. An embed link gives a site or an application
read-only bytes. Both are capabilities rather than credentials: the token in the URL
names one object and can express nothing else, and every request re-resolves it, so
revocation takes effect on the next one.

**The ordinary storage surface.** Streaming uploads and downloads, multipart,
versioning with delete markers, server-side copy, byte ranges, conditional requests,
per-bucket CORS, quotas, lifecycle expiration, signed webhooks for storage events,
and optional AES-256-GCM encryption at rest. The compatibility list is machine-checked
against the routing and the protocol tests rather than maintained as prose.

Real-client compatibility runs on every change against boto3, the AWS SDK for
JavaScript v3, the AWS SDK for Go, and the AWS SDK for Java v2.

## What it does not do

This is the part we would rather you read before installing it than after.

**A deployment is one process on one machine, with one copy of your data.** There is
no clustering, no replication, no erasure coding. Durability is whatever the storage
underneath gives you: use redundant disks and take backups. If the machine is gone,
the service is down until you restore it.

That is a real limit, and it is the one most likely to disqualify Record Store for a
given deployment. Replication is substantial work, and we would rather fund it with
adoption than ship a cluster story we cannot stand behind.

**Object Lock is enforced by Record Store, not by the filesystem.** It stops
deletions through the API, including by the root credential. It does not stop
somebody with access to the data directory, who never goes through the code that
would refuse them. If your threat model includes the person administering the
server, Object Lock alone does not cover it, and we say so in the security
documentation rather than leaving it to be inferred.

**A proof bundle is not a certificate of authenticity.** Its signing key derives
from the deployment's own master key, so anyone holding that key can sign any
bundle. It is evidence against alteration in transit and against a forgery by a
third party — not against the deployment's own operator.

**The tamper-evident audit chain is not finished.** The audit trail is durable,
queryable, and separate from the storage-event feed. Making it *tamper-evident* — a
hash chain over records, periodic checkpoints, and external anchoring so a past
state is provable against someone with disk access — is in progress. The proof
bundle format already carries the section and reports it as unavailable, so bundles
issued now stay readable by verifiers built later, and nobody mistakes an unanchored
document for an anchored one.

Also not implemented: ACLs, `UploadPartCopy`, `ListObjects` V1, batch `DeleteObjects`,
server-side encryption request headers, `aws-chunked` trailing checksums, object
tagging, S3 Select, and static website hosting. Unsupported operations return S3 XML
`NotImplemented` — they are never silently accepted, so a client finds out
immediately rather than discovering later that a header it sent was ignored.

## Supply chain

Container images are published for `linux/amd64` and `linux/arm64` with signed
build provenance, and every architecture carries an SPDX SBOM attested against the
platform manifest it actually describes. Binary archives carry provenance too.

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store:0.1.3 \
  --repo OpenElementsLabs/record-store
```

The release workflow verifies every one of those attestations before the release is
created, and refuses to publish if any is missing. Dependencies are audited with
`cargo audit --deny warnings` and no exceptions, which has already shaped design
decisions — a signature crate carrying an unpatched advisory was ruled out on those
grounds. The parsers that run before a request is authenticated are fuzzed.

## Getting it

```bash
docker pull ghcr.io/openelementslabs/record-store:latest
```

- Documentation: <https://openelementslabs.github.io/record-store/>
- Source: <https://github.com/OpenElementsLabs/record-store>
- Moving an existing deployment: [Migrating from MinIO](https://openelementslabs.github.io/record-store/getting-started/migrating-from-minio/)
- Licence: Apache-2.0

## Where it is going

Finishing the tamper-evident audit log is next: the hash chain, checkpoints, and
RFC 3161 timestamping, so a proof bundle can carry history that holds up against
the operator of the deployment that issued it. After that, replication — when there
is enough adoption to justify doing it properly.

Issues and pull requests are welcome. If you find a claim in the documentation that
is stronger than what the code guarantees, that is a bug and we would like to hear
about it.

Record Store is built and maintained by [Open Elements](https://open-elements.com).
