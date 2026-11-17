# Upload Problems

## `NotImplemented` on every upload

The client is sending AWS's `aws-chunked` trailing checksums, which Record Store
does not accept. This affects AWS CLI 2.23 and later, and every AWS SDK that
defaults to `WHEN_SUPPORTED`. It is the most common first-upload failure.

Set request checksum calculation and response checksum validation to "when
required" in the client:

| Client | Setting |
| --- | --- |
| AWS CLI | `aws configure set request_checksum_calculation when_required --profile record-store` and `aws configure set response_checksum_validation when_required --profile record-store`. See [AWS CLI](../guides/aws-cli.md#configure-a-profile). |
| JavaScript v3 | `requestChecksumCalculation: "WHEN_REQUIRED"` and `responseChecksumValidation: "WHEN_REQUIRED"`. See [JavaScript](../sdk/javascript.md#client). |
| boto3 | `Config(request_checksum_calculation="when_required", response_checksum_validation="when_required")`. See [Python](../sdk/python.md#client). |
| Go v2 | `RequestChecksumCalculation` and `ResponseChecksumValidation` set to their `WhenRequired` values. See [Go](../sdk/go.md#client). |
| Rust | `.request_checksum_calculation(...)` and `.response_checksum_validation(...)` with `WhenRequired`. See [Rust](../sdk/rust.md#client). |
| Java v2 | `chunkedEncodingEnabled(false)`. See [Java](../sdk/java.md#client). |

If you can't change the client configuration, set the following environment
variables instead. The AWS CLI and every AWS SDK read them:

```bash
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
```

## `NotImplemented` when changing bucket or Object Lock settings

Uploads work, but a command such as `put-bucket-versioning`, `put-bucket-cors`,
or `put-object-legal-hold` fails with `NotImplemented`.

S3 requires a checksum on these requests, so the AWS CLI sends one even with
`request_checksum_calculation when_required`. Recent versions of the AWS CLI
use CRC64NVME, which Record Store doesn't support. Pass
`--checksum-algorithm CRC32` to the command. For the full list of commands, see
[Commands that require a checksum](../guides/aws-cli.md#commands-that-require-a-checksum).

## Uploads fail above a size threshold

Small uploads work, large ones fail — a proxy body-size limit.

```nginx
client_max_body_size 0;
proxy_request_buffering off;
```

Caddy has no default limit. Traefik and Coolify's proxy do — raise them for the storage
domain.

Confirm by bypassing the proxy. On the Record Store host, upload a large file
directly to the S3 listener. `--endpoint-url` overrides the profile's endpoint:

--8<-- "docs/.snippets/aws-cli-profile.md"

```bash
head -c 100M /dev/urandom > /tmp/large.bin
aws --profile record-store --endpoint-url http://127.0.0.1:7600 s3 cp /tmp/large.bin s3://uploads/
```

Works direct and fails through the proxy — it is the proxy.

## Uploads time out

Large uploads over a slow link exceed proxy read and send timeouts.

```nginx
proxy_read_timeout 600s;
proxy_send_timeout 600s;
```

Better: use [multipart uploads](../guides/multipart-uploads.md). Each part is a
separate bounded request, so no single request has to survive the whole transfer.

## `EntityTooSmall`

Every part of a multipart upload except the last must be at least **5 MiB**.

Most SDK upload helpers handle this. If you are building the parts yourself, size them
at 5 MiB or above — 8 MiB or 16 MiB is a reasonable working default.

## `InvalidPart`

A part in the completion list does not match what was uploaded. Either:

- The part number was never uploaded, or
- The ETag in the manifest does not match the stored part.

Use the ETag returned by each `UploadPart` response, verbatim. Do not compute it
yourself.

## `InvalidPartOrder`

Parts in the completion manifest must be in ascending part-number order. Sort before
sending.

## `QuotaExceeded`

The write would take the bucket past its [quota](../administration/quotas.md).

```bash
record-store storage inspect --endpoint <endpoint>
```

Note that quotas enforce on **logical** bytes. On a versioned bucket, non-current
versions do not count — so a bucket can be inside its quota and still large on disk.

Either raise the quota or delete data. Deleting on a versioned bucket writes a delete
marker; to reclaim space you also need
[`noncurrent_version_expiration`](../administration/lifecycle-rules.md).

## `AccessDenied` on upload

The account's policy does not allow `s3:PutObject` on that resource.

```json
{
  "effect": "allow",
  "actions": ["s3:PutObject"],
  "resources": ["bucket:uploads/*"]
}
```

`bucket:uploads` alone is not enough — that covers the bucket, not its objects.

A **copy** is checked twice: `s3:PutObject` on the destination and `s3:GetObject` on the
source. An account that can write but not read the source cannot copy.

## `BadDigest`

The bytes received did not match the checksum the client supplied. Usually a proxy
modifying the body, or a genuinely corrupted transfer. Retry once; if it recurs,
disable body-rewriting on the proxy.

## Disk full

```bash
curl https://management.example.com/api/v1/storage/status \
  -H "Authorization: Bearer <your-management-token>"
```

Immediate options, cheapest first:

```bash
record-store storage repair --endpoint <endpoint>          # dry run
record-store storage repair --apply --endpoint <endpoint>  # remove orphans
```

See [Capacity Planning](../operations/capacity-planning.md).

## `temporary_upload_bytes` keeps growing

Multipart uploads are being started and never completed or aborted. Each holds its parts
on disk until one or the other happens.

```bash
aws --profile record-store \
  s3api list-multipart-uploads --bucket uploads
```

Abort the stale ones:

```bash
aws --profile record-store \
  s3api abort-multipart-upload \
  --bucket uploads --key <key> --upload-id <id>
```

Then fix the client — an upload path that can fail without aborting will keep doing
this.

## Browser upload fails with a CORS error

The browser blocked it, and the request may never have reached Record Store.

Configure CORS on the bucket:

```bash
aws --profile record-store \
  s3api put-bucket-cors --bucket uploads \
  --checksum-algorithm CRC32 \
  --cors-configuration file://cors.json
```

The `--checksum-algorithm CRC32` option is required. For the reason, see
[Commands that require a checksum](../guides/aws-cli.md#commands-that-require-a-checksum).

```json
{
  "CORSRules": [
    {
      "AllowedOrigins": ["https://app.example.com"],
      "AllowedMethods": ["GET", "PUT", "POST", "HEAD"],
      "AllowedHeaders": ["*"],
      "ExposeHeaders": ["ETag"],
      "MaxAgeSeconds": 3000
    }
  ]
}
```

`ExposeHeaders: ["ETag"]` matters for browser-side multipart — without it the script
cannot read the part ETags it needs to complete the upload.

See [Networking and Proxies](networking.md).

## Presigned upload rejected

- **Expired.** Maximum 7 days; a short expiry plus clock skew is a common cause.
- **Headers do not match.** If `ContentType` was signed, the upload must send exactly
  that value.
- **The signing credential lost permission**, or was disabled or rotated.
- **Wrong method.** A URL presigned for `PUT` cannot be used with `POST`.

See [Presigned URLs](../guides/presigned-urls.md).

## Diagnosing any upload failure

1. Get the request ID from the response — `x-amz-request-id`.
2. Look it up:
   ```bash
   curl -G https://management.example.com/api/v1/audit/events \
     -H "Authorization: Bearer <your-management-token>" \
     --data-urlencode "request_id=<request id>"
   ```
3. `denied` means authorization; `failure` means something else — check the logs for
   the same ID.
4. Reproduce with the AWS CLI against the direct endpoint. If that works, the problem is
   between the client and Record Store.
