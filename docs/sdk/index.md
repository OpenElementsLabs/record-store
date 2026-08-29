# SDKs

Record Store speaks the S3 API, so the AWS SDK for your language works. Three settings
are always the same:

| Setting | Value | Why |
| --- | --- | --- |
| Endpoint | Your Record Store S3 URL | Point the SDK away from AWS |
| Region | `us-east-1` | Record Store has no regions, but SigV4 requires one |
| Path-style addressing | Enabled | The bucket goes in the path, not the hostname |

Path-style matters: virtual-hosted style would put the bucket in the hostname
(`demo.storage.example.com`), requiring wildcard DNS and certificates.

## Credentials

Use a [service account](../administration/service-accounts.md), not root credentials,
and keep the secret server-side. See
[Application Integration](../guides/application-integration.md).

```bash
RECORD_STORE_ENDPOINT=https://storage.example.com
RECORD_STORE_ACCESS_KEY=<service account access key>
RECORD_STORE_SECRET_KEY=<service account secret key>
```

## Guides

<div class="grid cards" markdown>

-   **[JavaScript and TypeScript](javascript.md)** — AWS SDK v3
-   **[Next.js](nextjs.md)** — presigned browser uploads, end to end
-   **[Python](python.md)** — boto3
-   **[Go](go.md)** — AWS SDK for Go v2
-   **[Rust](rust.md)** — `aws-sdk-s3`
-   **[AWS CLI](../guides/aws-cli.md)** — the command-line client

</div>

## Verified against real clients

The repository runs compatibility tests against boto3, the AWS SDK for JavaScript v3,
and the AWS SDK for Go, driving a real Record Store server:

```bash
bash tests/compatibility/run.sh
```

They cover bucket and object I/O, listing, multipart completion, presigned requests,
browser CORS, ranges, versioning, historical reads, and copy behaviour.
