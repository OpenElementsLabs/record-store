"""Real boto3 compatibility exercise against a running Record Store node."""

import os
import urllib.request
import uuid

import boto3
from botocore.config import Config


ENDPOINT = os.environ.get("RECORD_STORE_COMPAT_ENDPOINT", "http://127.0.0.1:7600")
ACCESS_KEY = os.environ["RECORD_STORE_ROOT_ACCESS_KEY"]
SECRET_KEY = os.environ["RECORD_STORE_ROOT_SECRET_KEY"]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def main() -> None:
    client = boto3.client(
        "s3",
        endpoint_url=ENDPOINT,
        region_name="us-east-1",
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        config=Config(
            signature_version="s3v4",
            s3={"addressing_style": "path"},
            request_checksum_calculation="when_required",
            response_checksum_validation="when_required",
        ),
    )
    bucket = f"record-store-boto3-{uuid.uuid4().hex[:16]}"
    client.create_bucket(Bucket=bucket)

    client.put_object(Bucket=bucket, Key="single.txt", Body=b"boto3-single")
    downloaded = client.get_object(Bucket=bucket, Key="single.txt")["Body"].read()
    require(downloaded == b"boto3-single", "boto3 download mismatch")
    listed = client.list_objects_v2(Bucket=bucket)
    require(any(item["Key"] == "single.txt" for item in listed["Contents"]), "missing list key")

    upload = client.create_multipart_upload(Bucket=bucket, Key="multipart.bin")
    first = client.upload_part(
        Bucket=bucket,
        Key="multipart.bin",
        UploadId=upload["UploadId"],
        PartNumber=1,
        Body=b"a" * (5 * 1024 * 1024),
    )
    second = client.upload_part(
        Bucket=bucket,
        Key="multipart.bin",
        UploadId=upload["UploadId"],
        PartNumber=2,
        Body=b"tail",
    )
    parts = client.list_parts(Bucket=bucket, Key="multipart.bin", UploadId=upload["UploadId"])
    require(len(parts["Parts"]) == 2, "multipart parts were not listed")
    client.complete_multipart_upload(
        Bucket=bucket,
        Key="multipart.bin",
        UploadId=upload["UploadId"],
        MultipartUpload={
            "Parts": [
                {"PartNumber": 1, "ETag": first["ETag"]},
                {"PartNumber": 2, "ETag": second["ETag"]},
            ]
        },
    )
    ranged = client.get_object(Bucket=bucket, Key="multipart.bin", Range="bytes=5242878-5242883")
    require(ranged["Body"].read() == b"aatail", "multipart range mismatch")

    put_url = client.generate_presigned_url(
        "put_object", Params={"Bucket": bucket, "Key": "presigned.txt"}, ExpiresIn=60
    )
    request = urllib.request.Request(put_url, data=b"presigned", method="PUT")
    with urllib.request.urlopen(request, timeout=10) as response:
        require(response.status == 200, "presigned PUT failed")
    get_url = client.generate_presigned_url(
        "get_object", Params={"Bucket": bucket, "Key": "presigned.txt"}, ExpiresIn=60
    )
    with urllib.request.urlopen(get_url, timeout=10) as response:
        require(response.read() == b"presigned", "presigned GET mismatch")

    client.put_bucket_versioning(Bucket=bucket, VersioningConfiguration={"Status": "Enabled"})
    first_version = client.put_object(Bucket=bucket, Key="versioned.txt", Body=b"one")["VersionId"]
    second_version = client.put_object(Bucket=bucket, Key="versioned.txt", Body=b"two")["VersionId"]
    require(first_version != second_version, "version IDs were reused")
    historical = client.get_object(Bucket=bucket, Key="versioned.txt", VersionId=first_version)
    require(historical["Body"].read() == b"one", "historical version mismatch")
    deletion = client.delete_object(Bucket=bucket, Key="versioned.txt")
    require(deletion.get("DeleteMarker") is True, "delete marker was not created")
    versions = client.list_object_versions(Bucket=bucket, Prefix="versioned.txt")
    require(len(versions.get("Versions", [])) == 2, "version listing mismatch")
    require(len(versions.get("DeleteMarkers", [])) == 1, "delete marker listing mismatch")

    client.copy_object(Bucket=bucket, Key="copied.txt", CopySource={"Bucket": bucket, "Key": "single.txt"})
    require(
        client.get_object(Bucket=bucket, Key="copied.txt")["Body"].read() == b"boto3-single",
        "copy mismatch",
    )
    print("boto3 compatibility: ok")


if __name__ == "__main__":
    main()
