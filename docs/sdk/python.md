# Python

Use boto3. The compatibility suite pins `boto3==1.43.77` and runs it against a real
Record Store server.

```bash
pip install boto3
```

## Client

The client reads its settings from the environment variables described in
[SDKs](index.md#environment-variables):

```python
import os

import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url=os.environ["RECORD_STORE_ENDPOINT"],
    aws_access_key_id=os.environ["RECORD_STORE_ACCESS_KEY"],
    aws_secret_access_key=os.environ["RECORD_STORE_SECRET_KEY"],
    region_name="us-east-1",
    config=Config(
        s3={"addressing_style": "path"},
        request_checksum_calculation="when_required",
        response_checksum_validation="when_required",
    ),
)
```

The following settings are specific to Record Store:

- `endpoint_url`: the URL of the Record Store S3 API, for example
  `https://storage.example.com`. The S3 API listens on port 7600 by default.
- `aws_access_key_id` and `aws_secret_access_key`: a service account's S3 access
  key pair. The management token doesn't work with the S3 API.
- `region_name`: Record Store has no regions, but SigV4 signs a region into
  every request. Any value works if every client uses the same one.
- `addressing_style="path"`: required. Puts the bucket name in the URL path
  instead of the hostname.
- `request_checksum_calculation` and `response_checksum_validation`: required.
  Stop boto3 from sending upload checksums as `aws-chunked` trailers, which
  Record Store rejects with `NotImplemented`.

## Buckets

```python
s3.create_bucket(Bucket="uploads")

for bucket in s3.list_buckets()["Buckets"]:
    print(bucket["Name"])
```

## Upload

```python
s3.put_object(
    Bucket="uploads",
    Key="invoices/2026/03/inv-1.pdf",
    Body=open("inv-1.pdf", "rb"),
    ContentType="application/pdf",
)
```

`upload_file` handles large objects with multipart automatically:

```python
s3.upload_file("big.bin", "uploads", "big.bin")
```

## Download

```python
s3.download_file("uploads", "invoices/2026/03/inv-1.pdf", "inv-1.pdf")
```

Or stream it:

```python
response = s3.get_object(Bucket="uploads", Key="invoices/2026/03/inv-1.pdf")
for chunk in response["Body"].iter_chunks():
    ...
```

## List

```python
paginator = s3.get_paginator("list_objects_v2")
for page in paginator.paginate(Bucket="uploads", Prefix="invoices/2026/"):
    for obj in page.get("Contents", []):
        print(obj["Key"], obj["Size"])
```

Use the paginator. Listing is always bounded.

## Delete

```python
s3.delete_object(Bucket="uploads", Key="invoices/2026/03/inv-1.pdf")
```

## Presigned URLs

```python
url = s3.generate_presigned_url(
    "put_object",
    Params={"Bucket": "uploads", "Key": key, "ContentType": "application/pdf"},
    ExpiresIn=900,
)
```

Sign server-side only. See [Presigned URLs](../guides/presigned-urls.md).

## Versioning

```python
s3.put_bucket_versioning(
    Bucket="uploads",
    VersioningConfiguration={"Status": "Enabled"},
)

versions = s3.list_object_versions(Bucket="uploads")
```

## Ranges

```python
response = s3.get_object(Bucket="uploads", Key="big.bin", Range="bytes=0-1023")
```

## Checksums

The [client](#client) sets `request_checksum_calculation` and
`response_checksum_validation` to `when_required`. If you can't change the client
code, set the equivalent environment variables instead:

```bash
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
```
