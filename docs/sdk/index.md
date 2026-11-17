# SDKs

Record Store implements the S3 API, so you can use the AWS SDK for your
language. You change a few client settings so that the SDK sends requests to
Record Store instead of AWS.

## Client settings

Every S3 client needs the following settings before its first request. Each SDK
guide shows them in its client example.

| Setting | Value | Reason |
| --- | --- | --- |
| Endpoint | The URL of the Record Store S3 API, for example `https://storage.example.com`. The S3 API listens on port 7600 by default. | Sends requests to Record Store instead of AWS. Don't use the management API (port 7601) or the web console (port 7602). |
| Credentials | An S3 access key pair. | Signs each request with SigV4. See [Credentials](#credentials). |
| Region | `us-east-1` | Record Store has no regions, but SigV4 signs a region into every request. Any value works if every client uses the same one. |
| Path-style addressing | Enabled | Puts the bucket name in the URL path (`storage.example.com/demo`). Record Store doesn't support virtual-hosted style (`demo.storage.example.com`). |
| Request checksum calculation | `WHEN_REQUIRED` | Recent SDKs send upload checksums as `aws-chunked` trailers by default, which Record Store rejects with `NotImplemented`. |
| Response checksum validation | `WHEN_REQUIRED` | Matches the request setting. |

## Credentials

Record Store has two kinds of credentials. S3 clients use only the first kind:

| Credential | Format | Used by | Port |
| --- | --- | --- | --- |
| **S3 credentials** | Access key ID and secret access key | S3 clients: AWS SDKs, the AWS CLI, rclone | 7600 |
| **Management token** | Bearer token | The web console, the `record-store` CLI, the management API | 7601 |

The management token doesn't work with the S3 API, and an S3 key pair doesn't
work with the management API.

For applications, create a [service account](../administration/service-accounts.md)
and use its access key pair. Use the root key pair only for local testing. Keep
the secret key on the server. For more information, see
[Application integration](../guides/application-integration.md).

## Environment variables

The SDK examples read the client settings from the following environment
variables:

```bash
RECORD_STORE_ENDPOINT=https://storage.example.com
RECORD_STORE_ACCESS_KEY=SERVICE_ACCOUNT_ACCESS_KEY
RECORD_STORE_SECRET_KEY=SERVICE_ACCOUNT_SECRET_KEY
```

Replace `SERVICE_ACCOUNT_ACCESS_KEY` and `SERVICE_ACCOUNT_SECRET_KEY` with the
service account's access key pair.

These variables belong to your application. Don't confuse them with the server's
`RECORD_STORE_ROOT_ACCESS_KEY` and `RECORD_STORE_ROOT_SECRET_KEY`, or with the
`RECORD_STORE_MANAGEMENT_TOKEN` that the `record-store` CLI reads.

## Guides

<div class="grid cards" markdown>

-   **[Java](java.md)**: AWS SDK for Java v2
-   **[JavaScript and TypeScript](javascript.md)**: AWS SDK for JavaScript v3
-   **[Next.js](nextjs.md)**: presigned browser uploads, end to end
-   **[Python](python.md)**: boto3
-   **[Go](go.md)**: AWS SDK for Go v2
-   **[Rust](rust.md)**: `aws-sdk-s3`
-   **[AWS CLI](../guides/aws-cli.md)**: the command-line client

</div>

## Verified against real clients

The repository runs compatibility tests against boto3, the AWS SDK for
JavaScript v3, the AWS SDK for Java v2, and the AWS SDK for Go. The tests run
against a real Record Store server:

```bash
bash tests/compatibility/run.sh
```

The tests cover bucket and object I/O, listing, multipart completion, presigned
requests, browser CORS, ranges, versioning, historical reads, and copy behavior.
