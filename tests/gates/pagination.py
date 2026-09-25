#!/usr/bin/env python3
"""COR-PAGINATION: every paginated listing returns every item exactly once.

A listing that drops one item per page boundary looks correct on any dataset
smaller than a page, which is every dataset most tests use. This gate builds
known datasets larger than the pages it asks for and walks every paginated
surface at several page sizes, with and without filters, comparing the union of
the pages with ground truth: nothing missing, nothing twice, the order the API
promises.

Surfaces: ListObjectsV2 (plain, prefix, delimiter, StartAfter), ListObjectVersions,
ListMultipartUploads, ListParts, the storage-event feed (plain and filtered),
and the audit trail (bounded by `until`, since reading the trail appends to it).
Keys include spaces, '+', '%', and non-ASCII characters, because continuation
tokens that encode keys are where escaping bugs live.
"""

from __future__ import annotations

import concurrent.futures
import datetime
import time
import urllib.parse

from gatelib import Gate, Secrets, Server, artifact_from_arguments, identify_artifact

SPECIAL = ["space key", "plus+key", "percent%25key", "unicode-ü-ß", "tilde~key", "a/deep/nested/key"]


def walk_objects(client, page: int, **arguments) -> tuple[list[str], list[str]]:
    keys, prefixes, token = [], [], None
    while True:
        request = {"Bucket": "pages", "MaxKeys": page, **arguments}
        if token:
            request["ContinuationToken"] = token
        response = client.list_objects_v2(**request)
        keys += [item["Key"] for item in response.get("Contents", [])]
        prefixes += [item["Prefix"] for item in response.get("CommonPrefixes", [])]
        if not response.get("IsTruncated"):
            return keys, prefixes
        token = response["NextContinuationToken"]


def walk_versions(client, page: int) -> list[tuple[str, str]]:
    seen, key_marker, version_marker = [], None, None
    while True:
        request = {"Bucket": "pages-versions", "MaxKeys": page}
        if key_marker is not None:
            request.update(KeyMarker=key_marker, VersionIdMarker=version_marker)
        response = client.list_object_versions(**request)
        seen += [(v["Key"], v["VersionId"]) for v in response.get("Versions", [])]
        seen += [(m["Key"], m["VersionId"]) for m in response.get("DeleteMarkers", [])]
        if not response.get("IsTruncated"):
            return seen
        key_marker, version_marker = response["NextKeyMarker"], response.get("NextVersionIdMarker", "")


def walk_uploads(client, page: int) -> list[str]:
    """Pages uploads the way the AWS SDK does: its own paginator."""
    seen = []
    for response in client.get_paginator("list_multipart_uploads").paginate(
            Bucket="pages", PaginationConfig={"PageSize": page}):
        seen += [u["UploadId"] for u in response.get("Uploads", [])]
        if len(seen) > 10_000:
            raise AssertionError("upload pagination did not terminate")
    return seen


def walk_uploads_by_markers(client, page: int) -> list[str]:
    """Pages uploads the way the S3 API specifies, by hand: each request carries
    the previous page's NextKeyMarker and NextUploadIdMarker, and a truncated
    page must name both (release/findings/RSG-009 was the missing key marker)."""
    seen, key_marker, upload_marker = [], None, None
    while True:
        request = {"Bucket": "pages", "MaxUploads": page}
        if key_marker is not None:
            request.update(KeyMarker=key_marker, UploadIdMarker=upload_marker)
        response = client.list_multipart_uploads(**request)
        seen += [u["UploadId"] for u in response.get("Uploads", [])]
        if not response["IsTruncated"] or len(seen) > 10_000:
            return seen
        if "NextKeyMarker" not in response or "NextUploadIdMarker" not in response:
            raise AssertionError(f"a truncated page lacks a marker: {sorted(response)}")
        key_marker, upload_marker = response["NextKeyMarker"], response["NextUploadIdMarker"]


def walk_parts(client, upload_id: str, page: int) -> list[int]:
    seen, marker = [], None
    while True:
        request = {"Bucket": "pages", "Key": "many-parts", "UploadId": upload_id, "MaxParts": page}
        if marker is not None:
            request["PartNumberMarker"] = marker
        response = client.list_parts(**request)
        seen += [p["PartNumber"] for p in response.get("Parts", [])]
        if not response.get("IsTruncated"):
            return seen
        marker = response["NextPartNumberMarker"]


def walk_feed(server: Server, path: str, limit: int, identifier: str, **filters) -> list[dict]:
    items, cursor = [], None
    for _ in range(100_000):
        query = {"limit": str(limit), **filters}
        if cursor:
            query.update(after_time=cursor[0], after_id=cursor[1])
        page = server.api("GET", f"{path}?{urllib.parse.urlencode(query)}")
        items += page.get("events", [])
        if not page.get("next_time") or not page.get("next_id") or not page.get("events"):
            return items
        cursor = (page["next_time"], page["next_id"])
    raise AssertionError("feed did not terminate")


def exactly_once(gate: Gate, label: str, seen: list, expected: set) -> None:
    duplicates = len(seen) - len(set(seen))
    missing = expected - set(seen)
    extra = set(seen) - expected
    gate.check(label, not duplicates and not missing and not extra,
               {"missing": len(missing), "duplicated": duplicates, "unexpected": len(extra),
                "missing_sample": sorted(map(str, missing))[:5]})


def main(gate: Gate) -> None:
    artifact, _ = artifact_from_arguments()
    identify_artifact(gate, artifact)
    server = Server(artifact, gate.work_directory / "data", gate.evidence_directory / "logs",
                    credentials=Secrets(), encrypted=False, name="pagination")
    server.start()
    client = server.s3(max_pool_connections=64)
    client.create_bucket(Bucket="pages")
    keys = [f"{prefix}/{index:04d}" for prefix in ("a", "b", "c/x") for index in range(400)] + SPECIAL
    with concurrent.futures.ThreadPoolExecutor(16) as pool:
        list(pool.map(lambda key: client.put_object(Bucket="pages", Key=key, Body=b"x"), keys))
    truth = sorted(keys)
    gate.context["objects"] = len(truth)

    for page in (1, 7, 1000):
        if page == 1:
            listed, _ = walk_objects(client, page, Prefix="a/0")
            expected = [k for k in truth if k.startswith("a/0")]
        else:
            listed, _ = walk_objects(client, page)
            expected = truth
        gate.check(f"ListObjectsV2 at MaxKeys={page} returns every key once, in order",
                   listed == expected, {"listed": len(listed), "expected": len(expected)})
    listed, _ = walk_objects(client, 13, Prefix="c/")
    gate.check("ListObjectsV2 with a prefix returns exactly the prefixed keys", listed == [k for k in truth if k.startswith("c/")])
    listed, prefixes = walk_objects(client, 3, Delimiter="/")
    expected_prefixes = sorted({k.split("/", 1)[0] + "/" for k in truth if "/" in k})
    gate.check("ListObjectsV2 with a delimiter pages its common prefixes and top-level keys",
               sorted(prefixes) == expected_prefixes and listed == [k for k in truth if "/" not in k],
               {"prefixes": prefixes, "keys": listed})
    listed, _ = walk_objects(client, 50, StartAfter="b/0199")
    gate.check("ListObjectsV2 StartAfter resumes strictly after the given key", listed == [k for k in truth if k > "b/0199"])

    client.create_bucket(Bucket="pages-versions")
    client.put_bucket_versioning(Bucket="pages-versions", VersioningConfiguration={"Status": "Enabled"})
    versions = set()
    for index in range(40):
        for _ in range(3):
            versions.add((f"v/{index:03d}", client.put_object(Bucket="pages-versions", Key=f"v/{index:03d}", Body=b"x")["VersionId"]))
        if index % 4 == 0:
            versions.add((f"v/{index:03d}", client.delete_object(Bucket="pages-versions", Key=f"v/{index:03d}")["VersionId"]))
    for page in (1, 7, 1000):
        exactly_once(gate, f"ListObjectVersions at MaxKeys={page} returns every version and delete marker once",
                     walk_versions(client, page), versions)

    uploads = {client.create_multipart_upload(Bucket="pages", Key=f"upload/{index:02d}")["UploadId"] for index in range(25)}
    # Several uploads of one key, so a page boundary falls inside a key.
    uploads |= {client.create_multipart_upload(Bucket="pages", Key="upload/same")["UploadId"] for _ in range(4)}
    many = client.create_multipart_upload(Bucket="pages", Key="many-parts")["UploadId"]
    uploads.add(many)
    for number in range(1, 13):
        client.upload_part(Bucket="pages", Key="many-parts", UploadId=many, PartNumber=number, Body=b"p" * 1024)
    for page in (1, 3, 7, 1000):
        exactly_once(gate, f"ListMultipartUploads at MaxUploads={page} returns every upload once",
                     walk_uploads(client, page), uploads)
        name = f"ListMultipartUploads by KeyMarker and UploadIdMarker at MaxUploads={page} returns every upload once"
        try:
            exactly_once(gate, name, walk_uploads_by_markers(client, page), uploads)
        except AssertionError as error:
            gate.check(name, False, str(error))
    truncated = client.list_multipart_uploads(Bucket="pages", MaxUploads=2)
    gate.context["multipart_truncated_page_fields"] = sorted(k for k in truncated if k != "ResponseMetadata")
    gate.check("a truncated ListMultipartUploads page names NextKeyMarker and NextUploadIdMarker",
               truncated.get("IsTruncated") is True and {"NextKeyMarker", "NextUploadIdMarker"} <= set(truncated),
               gate.context["multipart_truncated_page_fields"])
    after_key = [u["Key"] for u in client.list_multipart_uploads(Bucket="pages", KeyMarker="upload/12").get("Uploads", [])]
    gate.check("KeyMarker alone resumes after every upload of that key",
               after_key == [f"upload/{index:02d}" for index in range(13, 25)] + ["upload/same"] * 4, after_key)
    for page in (1, 5, 1000):
        parts = walk_parts(client, many, page)
        gate.check(f"ListParts at MaxParts={page} returns every part once, in order", parts == list(range(1, 13)), parts)

    # Storage events: one object.created per key, published asynchronously.
    deadline = time.monotonic() + 30
    while True:
        feed = walk_feed(server, "/api/v1/events", 1000, "id", bucket="pages", type="object.created")
        if len({e.get("object") for e in feed}) >= len(keys) or time.monotonic() > deadline:
            break
        time.sleep(1)
    for limit in (1000, 500, 97, 7):
        events = walk_feed(server, "/api/v1/events", limit, "id", bucket="pages", type="object.created")
        exactly_once(gate, f"storage events at limit={limit} return every event once",
                     [e["object"] for e in events], set(keys))
    events = walk_feed(server, "/api/v1/events", 11, "id", bucket="pages", prefix="c/")
    exactly_once(gate, "storage events filtered by prefix at limit=11 return every matching event once",
                 [e["object"] for e in events if e["type"] == "object.created"], {k for k in keys if k.startswith("c/")})

    # Audit: reading the trail appends to it, so the walk is bounded by a
    # moment taken before it; every walk must then see the same records.
    until = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(seconds=1)).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    reference = None
    for limit in (1000, 100, 7):
        records = [r["event_id"] for r in walk_feed(server, "/api/v1/audit/events", limit, "event_id", until=until)]
        if reference is None:
            reference = set(records)
            gate.context["audit_records_bounded"] = len(reference)
            gate.check("the bounded audit walk returns records", len(reference) > limit)
        exactly_once(gate, f"audit events at limit={limit} return every record once", records, reference)
    server.stop()


if __name__ == "__main__":
    Gate("COR-PAGINATION", "every paginated listing returns every item exactly once").run(main)
