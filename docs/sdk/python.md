# Python

Use boto3. The compatibility suite pins `boto3==1.43.77` and runs it against a real
Record Store server.

```bash
pip install boto3
```

## Client

```python
import boto3

s3 = boto3.client(
    "s3",
    endpoint_url="https://storage.example.com",
    aws_access_key_id="<service account access key>",
    aws_secret_access_key="<service account secret key>",
    region_name="us-east-1",
    config=boto3.session.Config(s3={"addressing_style": "path"}),
)
```

`addressing_style="path"` is required. Region is arbitrary but must be consistent —
SigV4 signs it.

!!! tip "Prefer environment variables"
    Read credentials from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` or your own
    variables rather than committing them.

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

If uploads fail with `NotImplemented`, boto3 is using AWS's `aws-chunked` trailing
checksums. Turn that off:

```bash
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
```
