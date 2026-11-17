# AWS CLI

The AWS CLI works with any S3-compatible service, including Record Store. You
configure it with the Record Store endpoint and a few settings that Record Store
requires.

## Configure a profile

Store the Record Store settings in a separate profile named `record-store`. A
profile is a named set of AWS CLI credentials and settings, so your existing AWS
configuration stays unchanged.

To create the profile, run the following commands:

```bash
aws configure set endpoint_url ENDPOINT --profile record-store
aws configure set aws_access_key_id ACCESS_KEY --profile record-store
aws configure set aws_secret_access_key SECRET_KEY --profile record-store
aws configure set region us-east-1 --profile record-store
aws configure set s3.addressing_style path --profile record-store
aws configure set request_checksum_calculation when_required --profile record-store
aws configure set response_checksum_validation when_required --profile record-store
```

Replace the following:

- `ENDPOINT`: the URL of the Record Store S3 API, which listens on port 7600 by
  default. For example, `https://storage.example.com` or
  `http://127.0.0.1:7600`.
- `ACCESS_KEY` and `SECRET_KEY`: the access key pair of a
  [service account](../administration/service-accounts.md). For local testing,
  you can use the root key pair.

The following table explains each setting:

| Setting | Reason |
| --- | --- |
| `endpoint_url` | Sends requests from this profile to Record Store instead of AWS. Use the S3 API, not the management API (port 7601) or the web console (port 7602). Requires AWS CLI 2.13 or later. |
| `aws_access_key_id`, `aws_secret_access_key` | **S3 credentials** that sign each request. The management token doesn't work here: it's for the web console and the `record-store` CLI. |
| `region` | Record Store has no regions, but SigV4 signs a region into every request. Any value works if every client uses the same one. |
| `s3.addressing_style` | Puts the bucket name in the URL path (`storage.example.com/demo`). Record Store doesn't support virtual-hosted style (`demo.storage.example.com`), which needs wildcard DNS and a wildcard certificate. |
| `request_checksum_calculation`, `response_checksum_validation` | Stop the CLI from sending upload checksums as `aws-chunked` trailers, which Record Store rejects. Without these settings, uploads from AWS CLI 2.23 and later fail with a `NotImplemented` error. Earlier versions ignore them. |

To confirm that the profile works, list your buckets:

```bash
aws --profile record-store s3api list-buckets
```

!!! warning "Always pass the profile"
    A command without `--profile record-store` uses your default profile. If
    your default profile has AWS credentials, the command goes to your AWS
    account instead of to Record Store. To use the profile for every command in
    the current shell, run `export AWS_PROFILE=record-store`.

## Common operations

To shorten the commands, define an alias for the profile:

```bash
alias rs='aws --profile record-store'
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
    rs s3api put-bucket-versioning --checksum-algorithm CRC32 \
      --bucket demo --versioning-configuration Status=Enabled
    rs s3api list-object-versions --bucket demo
    rs s3api get-object --bucket demo --key report.pdf \
      --version-id VERSION_ID ./old.pdf
    ```

    Replace `VERSION_ID` with a version ID from `list-object-versions`.
    For why the first command passes `--checksum-algorithm CRC32`, see
    [Commands that require a checksum](#commands-that-require-a-checksum).

=== "CORS"

    ```bash
    rs s3api put-bucket-cors --checksum-algorithm CRC32 \
      --bucket demo --cors-configuration '{
      "CORSRules": [{
        "AllowedOrigins": ["https://app.example.com"],
        "AllowedMethods": ["PUT", "GET", "HEAD"],
        "AllowedHeaders": ["content-type", "x-amz-*"],
        "ExposeHeaders": ["ETag", "x-amz-version-id"],
        "MaxAgeSeconds": 3600
      }]
    }'
    ```

## Commands that require a checksum

S3 requires a checksum on some requests, such as those that change bucket
versioning, CORS, or Object Lock settings. For these commands, the AWS CLI
always sends a checksum, even with `request_checksum_calculation when_required`.
Recent versions of the AWS CLI use CRC64NVME by default, and Record Store
rejects CRC64NVME with a `NotImplemented` error.

To avoid the error, pass `--checksum-algorithm CRC32` to these commands:

- `s3api put-bucket-versioning`
- `s3api put-bucket-cors`
- `s3api put-object-lock-configuration`
- `s3api put-object-retention`
- `s3api put-object-legal-hold`

Record Store verifies CRC32 checksums against the request body. The examples in
this documentation already include the option.

## Use different credentials for one command

To run a command as a different identity, such as a second service account or
temporary credentials, set the keys in the environment and select the profile
with `AWS_PROFILE`:

```bash
AWS_PROFILE=record-store \
AWS_ACCESS_KEY_ID=OTHER_ACCESS_KEY \
AWS_SECRET_ACCESS_KEY=OTHER_SECRET_KEY \
aws s3 ls s3://demo/
```

The command uses the keys from the environment and every other setting from the
profile. Don't use `--profile` here: with `--profile`, the AWS CLI ignores
credentials in the environment.

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
