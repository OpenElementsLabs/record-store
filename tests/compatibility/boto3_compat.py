"""Real boto3 compatibility exercise against a running Record Store node."""

import datetime
import os
import urllib.request
import uuid

import boto3
import botocore.exceptions
from botocore.config import Config


ENDPOINT = os.environ.get("RECORD_STORE_COMPAT_ENDPOINT", "http://127.0.0.1:7600")
ACCESS_KEY = os.environ["RECORD_STORE_ROOT_ACCESS_KEY"]
SECRET_KEY = os.environ["RECORD_STORE_ROOT_SECRET_KEY"]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def require_denied(call, message: str) -> None:
    """Asserts a call is refused with 403 AccessDenied rather than succeeding.

    Object Lock is only worth anything if the refusals are real against a real
    client, so these assert the failure rather than the absence of a crash.
    """
    try:
        call()
    except botocore.exceptions.ClientError as error:
        code = error.response["Error"]["Code"]
        status = error.response["ResponseMetadata"]["HTTPStatusCode"]
        require(code == "AccessDenied", f"{message}: expected AccessDenied, got {code}")
        require(status == 403, f"{message}: expected 403, got {status}")
        return
    raise AssertionError(f"{message}: the request was allowed")


def object_lock_compatibility(client) -> None:
    """Exercises Object Lock end to end, including the refusals."""
    bucket = f"record-store-lock-{uuid.uuid4().hex[:16]}"
    client.create_bucket(Bucket=bucket, ObjectLockEnabledForBucket=True)

    # Object Lock implies versioning, and cannot be suspended afterwards.
    versioning = client.get_bucket_versioning(Bucket=bucket)
    require(versioning.get("Status") == "Enabled", "object lock must enable versioning")

    configuration = client.get_object_lock_configuration(Bucket=bucket)
    require(
        configuration["ObjectLockConfiguration"]["ObjectLockEnabled"] == "Enabled",
        "object lock configuration mismatch",
    )

    # A bucket default applies to writes that name no lock of their own.
    client.put_object_lock_configuration(
        Bucket=bucket,
        ObjectLockConfiguration={
            "ObjectLockEnabled": "Enabled",
            "Rule": {"DefaultRetention": {"Mode": "GOVERNANCE", "Days": 1}},
        },
    )
    defaulted = client.put_object(Bucket=bucket, Key="defaulted.txt", Body=b"default")
    head = client.head_object(Bucket=bucket, Key="defaulted.txt")
    require(
        head.get("ObjectLockMode") == "GOVERNANCE",
        "a bucket default must reach the object",
    )
    require(
        head.get("ObjectLockRetainUntilDate") is not None,
        "a defaulted object reports its retain-until date",
    )

    now = datetime.datetime.now(datetime.timezone.utc)
    compliance_until = now + datetime.timedelta(days=2)
    governance_until = now + datetime.timedelta(days=2)

    compliance = client.put_object(
        Bucket=bucket,
        Key="statement.pdf",
        Body=b"immutable record",
        ObjectLockMode="COMPLIANCE",
        ObjectLockRetainUntilDate=compliance_until,
    )
    governance = client.put_object(
        Bucket=bucket,
        Key="draft.txt",
        Body=b"working copy",
        ObjectLockMode="GOVERNANCE",
        ObjectLockRetainUntilDate=governance_until,
    )

    # The response headers report back what was asked for.
    stored = client.head_object(
        Bucket=bucket, Key="statement.pdf", VersionId=compliance["VersionId"]
    )
    require(stored.get("ObjectLockMode") == "COMPLIANCE", "retention mode mismatch")

    retention = client.get_object_retention(
        Bucket=bucket, Key="statement.pdf", VersionId=compliance["VersionId"]
    )
    require(
        retention["Retention"]["Mode"] == "COMPLIANCE",
        "retention subresource mismatch",
    )

    # A compliance retention cannot be shortened, by anyone, even with a bypass.
    require_denied(
        lambda: client.put_object_retention(
            Bucket=bucket,
            Key="statement.pdf",
            VersionId=compliance["VersionId"],
            BypassGovernanceRetention=True,
            Retention={
                "Mode": "COMPLIANCE",
                "RetainUntilDate": now + datetime.timedelta(minutes=1),
            },
        ),
        "shortening a compliance retention",
    )
    require_denied(
        lambda: client.delete_object(
            Bucket=bucket,
            Key="statement.pdf",
            VersionId=compliance["VersionId"],
            BypassGovernanceRetention=True,
        ),
        "deleting a version under compliance retention",
    )

    # Extending it is always allowed.
    client.put_object_retention(
        Bucket=bucket,
        Key="statement.pdf",
        VersionId=compliance["VersionId"],
        Retention={
            "Mode": "COMPLIANCE",
            "RetainUntilDate": compliance_until + datetime.timedelta(days=1),
        },
    )

    # A governance retention refuses a plain delete and yields to a bypass.
    require_denied(
        lambda: client.delete_object(
            Bucket=bucket, Key="draft.txt", VersionId=governance["VersionId"]
        ),
        "deleting a version under governance retention without a bypass",
    )
    client.delete_object(
        Bucket=bucket,
        Key="draft.txt",
        VersionId=governance["VersionId"],
        BypassGovernanceRetention=True,
    )

    # A legal hold blocks deletion on its own, and no bypass applies to it.
    held = client.put_object(Bucket=bucket, Key="held.txt", Body=b"under hold")
    client.put_object_legal_hold(
        Bucket=bucket,
        Key="held.txt",
        VersionId=held["VersionId"],
        LegalHold={"Status": "ON"},
    )
    status = client.get_object_legal_hold(
        Bucket=bucket, Key="held.txt", VersionId=held["VersionId"]
    )
    require(status["LegalHold"]["Status"] == "ON", "legal hold status mismatch")
    require_denied(
        lambda: client.delete_object(
            Bucket=bucket,
            Key="held.txt",
            VersionId=held["VersionId"],
            BypassGovernanceRetention=True,
        ),
        "deleting a version under a legal hold",
    )
    client.put_object_legal_hold(
        Bucket=bucket,
        Key="held.txt",
        VersionId=held["VersionId"],
        LegalHold={"Status": "OFF"},
    )
    # Removing the hold does not remove the bucket's default governance
    # retention, which this object also picked up at write time. The two are
    # independent, so releasing one still leaves the other in force.
    require_denied(
        lambda: client.delete_object(
            Bucket=bucket, Key="held.txt", VersionId=held["VersionId"]
        ),
        "deleting a version whose governance retention outlives its legal hold",
    )
    client.delete_object(
        Bucket=bucket,
        Key="held.txt",
        VersionId=held["VersionId"],
        BypassGovernanceRetention=True,
    )

    # A delete marker over a retained version stays allowed, and the version
    # underneath it remains readable.
    marker = client.delete_object(Bucket=bucket, Key="statement.pdf")
    require(marker.get("DeleteMarker") is True, "a delete marker must still be allowed")
    body = client.get_object(
        Bucket=bucket, Key="statement.pdf", VersionId=compliance["VersionId"]
    )["Body"].read()
    require(body == b"immutable record", "the retained version must remain readable")

    # Versioning cannot be suspended while Object Lock is enabled.
    try:
        client.put_bucket_versioning(
            Bucket=bucket, VersioningConfiguration={"Status": "Suspended"}
        )
        raise AssertionError("suspending versioning on a locked bucket was allowed")
    except botocore.exceptions.ClientError as error:
        require(
            error.response["Error"]["Code"] == "InvalidBucketState",
            "suspending versioning must report InvalidBucketState",
        )

    # A bucket that never had Object Lock reports that, rather than inventing one.
    plain = f"record-store-plain-{uuid.uuid4().hex[:16]}"
    client.create_bucket(Bucket=plain)
    try:
        client.get_object_lock_configuration(Bucket=plain)
        raise AssertionError("an unlocked bucket reported an object lock configuration")
    except botocore.exceptions.ClientError as error:
        require(
            error.response["Error"]["Code"] == "ObjectLockConfigurationNotFoundError",
            "an unlocked bucket must report ObjectLockConfigurationNotFoundError",
        )

    require(defaulted["VersionId"] is not None, "a locked bucket returns version ids")
    print("boto3 object lock compatibility: ok")


def checksum_compatibility(bucket: str) -> None:
    """Body digests the SDK sends are verified, with the SDK's own encoding.

    The other clients here are configured with checksum calculation
    WHEN_REQUIRED; this one keeps boto3's default (WHEN_SUPPORTED), which sends
    a CRC32 with every upload -- the case that used to be stored unverified.
    """
    import base64
    import hashlib

    default = boto3.client(
        "s3",
        endpoint_url=ENDPOINT,
        region_name="us-east-1",
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
    )
    default.put_object(Bucket=bucket, Key="checksum-default", Body=b"default checksum")
    require(
        default.get_object(Bucket=bucket, Key="checksum-default")["Body"].read() == b"default checksum",
        "an upload with the SDK's default checksum round-trips",
    )
    # CRC32C needs botocore[crt], an optional client dependency; the server side
    # of CRC32C is covered against the standard check vector in the S3 crate.
    for algorithm in ("CRC32", "SHA1", "SHA256"):
        key = f"checksum-{algorithm.lower()}"
        default.put_object(Bucket=bucket, Key=key, Body=b"algorithm " + algorithm.encode(), ChecksumAlgorithm=algorithm)
        require(
            default.get_object(Bucket=bucket, Key=key)["Body"].read() == b"algorithm " + algorithm.encode(),
            f"{algorithm} upload round-trips",
        )
    good_md5 = base64.b64encode(hashlib.md5(b"md5 body").digest()).decode()
    default.put_object(Bucket=bucket, Key="checksum-md5", Body=b"md5 body", ContentMD5=good_md5)
    wrong_md5 = base64.b64encode(hashlib.md5(b"something else").digest()).decode()
    try:
        default.put_object(Bucket=bucket, Key="checksum-md5-wrong", Body=b"md5 body", ContentMD5=wrong_md5)
    except botocore.exceptions.ClientError as error:
        require(error.response["Error"]["Code"] == "BadDigest", f"wrong Content-MD5: got {error.response['Error']['Code']}")
    else:
        raise AssertionError("a wrong Content-MD5 was accepted")
    try:
        default.head_object(Bucket=bucket, Key="checksum-md5-wrong")
        raise AssertionError("an object refused for its Content-MD5 was stored")
    except botocore.exceptions.ClientError:
        pass


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
    object_lock_compatibility(client)
    checksum_compatibility(bucket)
    print("boto3 compatibility: ok")


if __name__ == "__main__":
    main()
