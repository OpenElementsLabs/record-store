#!/usr/bin/env python3
"""CMP-UNSUPPORTED: operations outside the declared S3 subset are refused, not absorbed.

docs/reference/s3-compatibility.md: "Requests for an unsupported operation
return 501 NotImplemented." A silently accepted unsupported request is worse
than a refusal -- an ACL the client believes it set, an encryption header it
believes was honoured -- so each case checks both the answer and that nothing
was created or changed.
"""

from __future__ import annotations

import http.client
import urllib.parse

from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials
from botocore.exceptions import ClientError

from gatelib import Gate, Secrets, Server, artifact_from_arguments, identify_artifact, read_all


def refusal(call) -> tuple[int | None, str]:
    try:
        response = call()
        return response.get("ResponseMetadata", {}).get("HTTPStatusCode"), "accepted"
    except ClientError as error:
        return error.response["ResponseMetadata"]["HTTPStatusCode"], error.response["Error"]["Code"]


def exists(client, key: str) -> bool:
    try:
        client.head_object(Bucket="unsupported", Key=key)
        return True
    except ClientError:
        return False


def raw_put(server: Server, key: str, body: bytes, headers: dict[str, str]) -> tuple[int, bytes]:
    url = f"{server.s3_endpoint}/unsupported/{key}"
    request = AWSRequest(method="PUT", url=url, data=body, headers={**headers, "Content-Length": str(len(body))})
    SigV4Auth(Credentials(server.credentials.root_access_key, server.credentials.root_secret_key), "s3",
              "us-east-1").add_auth(request)
    parsed = urllib.parse.urlsplit(url)
    connection = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=30)
    connection.request("PUT", parsed.path, body=body, headers=dict(request.headers))
    response = connection.getresponse()
    content = response.read()
    connection.close()
    return response.status, content


def main(gate: Gate) -> None:
    artifact, _ = artifact_from_arguments()
    identify_artifact(gate, artifact)
    server = Server(artifact, gate.work_directory / "data", gate.evidence_directory / "logs",
                    credentials=Secrets(), encrypted=False, name="unsupported")
    server.start()
    client = server.s3()
    client.create_bucket(Bucket="unsupported")
    client.put_object(Bucket="unsupported", Key="source", Body=b"source-bytes")
    upload = client.create_multipart_upload(Bucket="unsupported", Key="multipart")["UploadId"]
    outcomes = {}

    cases = [
        ("UploadPartCopy", lambda: client.upload_part_copy(Bucket="unsupported", Key="multipart", UploadId=upload,
                                                           PartNumber=1, CopySource={"Bucket": "unsupported", "Key": "source"}), None),
        ("PutObject with SSE-S3 header", lambda: client.put_object(Bucket="unsupported", Key="sse", Body=b"x",
                                                                   ServerSideEncryption="AES256"), "sse"),
        ("PutObject with SSE-KMS header", lambda: client.put_object(Bucket="unsupported", Key="kms", Body=b"x",
                                                                    ServerSideEncryption="aws:kms"), "kms"),
        ("PutObject with a canned ACL", lambda: client.put_object(Bucket="unsupported", Key="acl", Body=b"x",
                                                                  ACL="public-read"), "acl"),
        ("PutObjectAcl", lambda: client.put_object_acl(Bucket="unsupported", Key="source", ACL="public-read"), None),
        # A copy is a write: it refuses what a PUT refuses, rather than copying
        # and dropping the header, and refuses a precondition it cannot check.
        ("CopyObject with SSE-S3 header", lambda: client.copy_object(Bucket="unsupported", Key="copy-sse",
                                                                     CopySource={"Bucket": "unsupported", "Key": "source"},
                                                                     ServerSideEncryption="AES256"), "copy-sse"),
        ("CopyObject with tagging", lambda: client.copy_object(Bucket="unsupported", Key="copy-tagged",
                                                               CopySource={"Bucket": "unsupported", "Key": "source"},
                                                               Tagging="a=b", TaggingDirective="REPLACE"), "copy-tagged"),
        ("CopyObject with CopySourceIfMatch", lambda: client.copy_object(Bucket="unsupported", Key="copy-conditional",
                                                                         CopySource={"Bucket": "unsupported", "Key": "source"},
                                                                         CopySourceIfMatch='"not-the-etag"'), "copy-conditional"),
        ("GetBucketAcl", lambda: client.get_bucket_acl(Bucket="unsupported"), None),
    ]
    for name, call, created_key in cases:
        status, code = refusal(call)
        outcomes[name] = {"status": status, "code": code}
        gate.check(f"{name} is refused with 501 NotImplemented", (status, code) == (501, "NotImplemented"),
                   outcomes[name])
        if created_key:
            gate.check(f"{name} created nothing", not exists(client, created_key))
    parts = client.list_parts(Bucket="unsupported", Key="multipart", UploadId=upload).get("Parts", [])
    gate.check("a refused UploadPartCopy added no part", parts == [], parts)
    gate.check("a refused ACL change left the object readable by its owner and unchanged",
               read_all(client, "unsupported", "source") == b"source-bytes")

    payload = b"hello world"
    framed = b"%x\r\n" % len(payload) + payload + b"\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n"
    status, body = raw_put(server, "framed", framed, {
        "Content-Encoding": "aws-chunked",
        "x-amz-content-sha256": "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
        "x-amz-trailer": "x-amz-checksum-crc32",
        "x-amz-decoded-content-length": str(len(payload)),
    })
    outcomes["aws-chunked PutObject"] = {"status": status, "body": body[:160].decode(errors="replace")}
    gate.check("an aws-chunked upload is refused with 501 NotImplemented",
               status == 501 and b"NotImplemented" in body, outcomes["aws-chunked PutObject"])
    gate.check("a refused aws-chunked upload created nothing", not exists(client, "framed"))

    # Flexible checksums (x-amz-checksum-crc32 and friends) are not in the
    # declared subset. Current AWS SDKs send one by default, and a client that
    # sent one believes the server compared it. Either verifying it or refusing
    # it is acceptable; storing a body that contradicts it is silent acceptance
    # of an unsupported feature.
    # Well-formed but wrong values (base64 of all-zero digests of the right
    # length), so a refusal cannot be a parse error standing in for a check.
    wrong_values = {"x-amz-checksum-crc32": "AAAAAA==", "x-amz-checksum-crc32c": "AAAAAA==",
                    "x-amz-checksum-sha1": "A" * 27 + "=", "x-amz-checksum-sha256": "A" * 43 + "="}
    for header, value in wrong_values.items():
        key = f"flexible-{header.rsplit('-', 1)[-1]}"
        status, body = raw_put(server, key, b"checksum-bytes", {header: value, "x-amz-content-sha256": "UNSIGNED-PAYLOAD"})
        stored = exists(client, key)
        outcomes[f"PutObject with a wrong {header}"] = {"status": status, "stored": stored,
                                                          "body": body[:160].decode(errors="replace")}
        gate.check(f"a PUT whose {header} contradicts its body is not stored", not stored and status >= 400,
                   outcomes[f"PutObject with a wrong {header}"])
    gate.context["outcomes"] = outcomes
    server.stop()


if __name__ == "__main__":
    Gate("CMP-UNSUPPORTED", "unsupported S3 operations are refused explicitly and change nothing").run(main)
