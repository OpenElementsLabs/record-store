# S3 Compatibility

Record Store implements a subset of the S3 API. This page is derived from the
capability registry in the source, which is kept in step with the routing and protocol
tests.

## Supported

| Capability | Notes |
| --- | --- |
| `SigV4HeaderAuthentication` | The only accepted signing method |
| `PresignedGetObject` | Up to 7 days |
| `PresignedPutObject` | Up to 7 days |
| `BucketOperations` | Create, delete, head, list |
| `BucketCors` | Get, put, delete |
| `ObjectOperations` | Put, get, head, delete |
| `ListObjectsV2` | Prefix, delimiter, pagination |
| `MultipartUpload` | Create, upload part, complete, abort, list |
| `ObjectVersioning` | Enable, suspend, list versions, delete markers |
| `CopyObject` | Server-side copy |
| `RangeAndConditionalReads` | `Range`, `If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since` |
| `ClientSha256Checksums` | `x-amz-content-sha256` |
| `ClientBodyDigests` | `Content-MD5` and `x-amz-checksum-crc32`, `-crc32c`, `-sha1`, `-sha256` are verified against the body; a mismatch is `400 BadDigest` and stores nothing. A verified `x-amz-checksum-*` is echoed in the response |
| `ObjectLock` | Retention (`GOVERNANCE`/`COMPLIANCE`), legal holds, bucket defaults, governance bypass |

## Unsupported

| Capability | Instead |
| --- | --- |
| `UploadPartCopy` | Download and re-upload the part |
| `ServerSideEncryptionHeaders` | [Encryption](../security/encryption.md) is a deployment setting, not per request |
| `AccessControlLists` | [Policies](../administration/policies.md) |
| `AwsChunkedEncoding` | Configure the SDK to send unsigned or fully-signed payloads |
| `Crc64NvmeChecksums` | Use CRC32, CRC32C, SHA-1 or SHA-256; an unverifiable checksum is refused rather than ignored |

Requests for an unsupported operation return `501 NotImplemented`.

## Client requirements

### Path-style addressing

```text
https://storage.example.com/bucket-name/object-key
```

Virtual-hosted style (`https://bucket.storage.example.com/key`) is not supported.
Every SDK has a setting for this:

| SDK | Setting |
| --- | --- |
| AWS CLI | `--endpoint-url` (path-style is used automatically) |
| boto3 | `config=Config(s3={"addressing_style": "path"})` |
| Go v2 | `o.UsePathStyle = true` |
| Java v2 | `forcePathStyle(true)`, `chunkedEncodingEnabled(false)`; see [Java](../sdk/java.md) |
| JavaScript v3 | `forcePathStyle: true` |
| Rust | `.force_path_style(true)` |

### Region

Any region works, but it must be consistent — SigV4 signs it. `us-east-1` is the
conventional choice.

### Checksums

A checksum sent as a header — `Content-MD5`, or `x-amz-checksum-crc32`, `-crc32c`,
`-sha1` or `-sha256` — is verified against the body before the object is committed. A
body that does not match is refused with `400 BadDigest` and nothing is stored.
CRC64NVME is refused with `NotImplemented` rather than accepted unverified.

Newer AWS SDKs default to calculating a checksum on every upload. Sent as a header, as
they do over plain HTTP, it is verified. Sent as an `aws-chunked` trailer, as they
typically do over HTTPS, it is refused with `NotImplemented`, because Record Store does
not accept `aws-chunked` payloads. If uploads fail that way:

```bash
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
```

## Query parameters

### Bucket-level

| Parameter | Effect |
| --- | --- |
| `?cors` | CORS configuration |
| `?versioning` | Versioning state |
| `?versions` | List object versions |
| `?uploads` | List in-progress multipart uploads |

### `ListObjectsV2`

| Parameter | Notes |
| --- | --- |
| `list-type` | `2` |
| `prefix` | |
| `delimiter` | |
| `max-keys` | |
| `continuation-token` | |
| `start-after` | |

A duplicated parameter is a `400 InvalidRequest` rather than a silent last-wins.

### Object-level

| Parameter | Operation |
| --- | --- |
| `?versionId=<id>` | Read or delete a specific version |
| `?uploads` | Initiate a multipart upload |
| `?uploadId=<id>&partNumber=<n>` | Upload a part |
| `?uploadId=<id>` | Complete, abort, or list parts |

## Bucket names

3–63 bytes, lowercase letters, digits, `-`, and `.`. Must begin and end with a letter
or digit. No `..`. Not an IPv4 address.

Reserved:

| | |
| --- | --- |
| Prefixes | `xn--`, `sthree-` |
| Suffixes | `-s3alias`, `--ol-s3` |
| Exact names | `record-store-system`, `record-store-internal` |

## Errors

Errors are XML in the S3 shape:

```xml
<Error>
  <Code>NoSuchKey</Code>
  <Message>The specified key does not exist</Message>
  <Resource>/uploads/missing.txt</Resource>
  <RequestId>...</RequestId>
</Error>
```

The request ID is also in the `x-amz-request-id` response header. See
[Error Reference](errors.md).

Header-authenticated `UNSIGNED-PAYLOAD` is supported, including PUT requests.
Unsupported AWS streaming payloads and trailing checksums return `501 NotImplemented`
with a message identifying the encoding. Malformed `x-amz-content-sha256` values
return `400 InvalidRequest` with a message identifying the header.

## Verified against real SDKs

The repository's compatibility suite runs against a live server with these pinned
versions:

| SDK | Version |
| --- | --- |
| `github.com/aws/aws-sdk-go-v2/service/s3` | 1.107.3 |
| `software.amazon.awssdk:s3` | 2.54.12 |
| `boto3` | 1.43.77 |
| `@aws-sdk/client-s3` | 3.1115.0 |
| `@aws-sdk/s3-request-presigner` | 3.1115.0 |

```bash
bash tests/compatibility/run.sh
```

## Tools

| Tool | Works | Notes |
| --- | --- | --- |
| AWS CLI | Yes | `--endpoint-url` |
| `s3cmd` | Yes | `host_bucket` must be path-style |
| `rclone` | Yes | Provider `Other`, `force_path_style` |
| `mc` | Yes | Standard S3 endpoint configuration |

## Extensions

Two things Record Store adds beyond S3:

- **[Share links](../guides/share-links.md)** — `/s/<token>` on the console, a page for
  a person.
- **[Embed links](../guides/embed-links.md)** — `/e/<token>` on the storage endpoint,
  raw bytes for a website.

Neither is part of the S3 API, and neither requires a signature. Both are managed
through the [management API](management-api.md).
