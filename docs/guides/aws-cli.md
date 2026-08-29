# AWS CLI

The AWS CLI works against Record Store once it is told to use a custom endpoint and
path-style addressing.

## Configure a profile

```bash
aws configure --profile record-store
```

Enter the access key and secret from a [service account](../administration/service-accounts.md).
For **Default region name** use `us-east-1` — Record Store does not use regions, but
SigV4 requires one and the signature must match.

Path-style addressing must be set per profile:

```bash
aws configure set s3.addressing_style path --profile record-store
```

!!! info "Why path-style"
    Virtual-hosted style puts the bucket in the hostname (`demo.storage.example.com`),
    which needs wildcard DNS and a wildcard certificate. Record Store expects the
    bucket in the path (`storage.example.com/demo`).

## Two environment settings you will want

```bash
export AWS_EC2_METADATA_DISABLED=true
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
```

The first stops the CLI pausing to look for EC2 instance metadata that is not there.
The other two keep the CLI off AWS's `aws-chunked` trailing-checksum encoding, which
Record Store reports as unsupported. Without them, newer AWS CLI versions can fail
uploads with a `NotImplemented` error.

## Common operations

```bash
alias rs='aws --profile record-store --endpoint-url https://storage.example.com'
```

=== "Buckets"

    ```bash
    rs s3api list-buckets
    rs s3api create-bucket --bucket demo
    rs s3api head-bucket --bucket demo
    rs s3api delete-bucket --bucket demo
    ```

=== "Objects"

    ```bash
    rs s3 cp ./report.pdf s3://demo/reports/report.pdf
    rs s3 cp s3://demo/reports/report.pdf ./report.pdf
    rs s3 ls s3://demo/reports/
    rs s3 rm s3://demo/reports/report.pdf
    ```

=== "Versioning"

    ```bash
    rs s3api put-bucket-versioning \
      --bucket demo --versioning-configuration Status=Enabled
    rs s3api list-object-versions --bucket demo
    rs s3api get-object --bucket demo --key report.pdf \
      --version-id <version-id> ./old.pdf
    ```

=== "CORS"

    ```bash
    rs s3api put-bucket-cors --bucket demo --cors-configuration '{
      "CORSRules": [{
        "AllowedOrigins": ["https://app.example.com"],
        "AllowedMethods": ["PUT", "GET", "HEAD"],
        "AllowedHeaders": ["content-type", "x-amz-*"],
        "ExposeHeaders": ["ETag", "x-amz-version-id"],
        "MaxAgeSeconds": 3600
      }]
    }'
    ```

## Copy

Server-side copy works within and across buckets:

```bash
rs s3api copy-object \
  --bucket demo --key copy.pdf \
  --copy-source demo/reports/report.pdf
```

Both `COPY` (inherit the source's metadata) and `REPLACE` (supply new metadata)
directives are supported.

## Multipart

`aws s3 cp` switches to multipart automatically for large files, and `aws s3api`
exposes the individual operations. Both work. See
[Multipart Uploads](multipart-uploads.md).

## Presigned URLs

```bash
rs s3 presign s3://demo/reports/report.pdf --expires-in 900
```

See [Presigned URLs](presigned-urls.md).

## What does not work

| Command | Why |
| --- | --- |
| `aws s3api put-object-acl`, `get-object-acl` | ACLs are not implemented |
| `aws s3api put-object-lock-configuration` | Object Lock is not implemented |
| `aws s3api upload-part-copy` | `UploadPartCopy` is not implemented |
| `--sse`, `--sse-kms-key-id` | Server-side encryption headers are not implemented |

Unsupported operations return an S3 XML `NotImplemented` error. They are never
silently accepted. Record Store's own
[encryption at rest](../security/encryption.md) is configured server-side and needs no
request headers.

`aws s3 sync` works, since it is built from `ListObjectsV2`, `GetObject`, `PutObject`,
and `DeleteObject`.

## Troubleshooting

A `SignatureDoesNotMatch` error is nearly always one of four things: the wrong secret,
a proxy rewriting the request, clock skew, or missing path-style addressing. See
[Authentication Errors](../troubleshooting/authentication.md).
