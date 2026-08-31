# Error Reference

Two error formats, because the two planes are separate.

## S3 API errors

XML in the S3 shape:

```xml
<Error>
  <Code>NoSuchKey</Code>
  <Message>The specified key does not exist</Message>
  <Resource>/uploads/missing.txt</Resource>
  <RequestId>...</RequestId>
</Error>
```

The request ID is also in `x-amz-request-id`.

### Authentication and authorization — `403`

| Code | Cause | Fix |
| --- | --- | --- |
| `AccessDenied` | Authenticated, but no policy allows it | Check the account's [policies](../administration/policies.md) |
| `InvalidAccessKeyId` | The access key is not known | Check the key; check the account is not deleted |
| `SignatureDoesNotMatch` | The signature does not verify | Usually a proxy rewriting `Host`, or a wrong secret |
| `RequestTimeTooSkewed` | Client clock is too far off | Sync time |

`SignatureDoesNotMatch` on requests that used to work is almost always a proxy. See
[Reverse Proxy and TLS](../deployment/reverse-proxy.md).

### Not found — `404`

| Code | Cause |
| --- | --- |
| `NoSuchBucket` | The bucket does not exist |
| `NoSuchKey` | The key does not exist, or its current version is a delete marker |
| `NoSuchUpload` | The multipart upload ID is unknown or already completed |
| `NoSuchCORSConfiguration` | No CORS configuration on this bucket |

### Conflict — `409`

| Code | Cause |
| --- | --- |
| `BucketAlreadyExists` | Name already in use |
| `BucketNotEmpty` | Delete the objects first, including old versions |

### Bad request — `400`

| Code | Cause |
| --- | --- |
| `InvalidBucketName` | See the [naming rules](s3-compatibility.md#bucket-names) |
| `InvalidRequest` | Malformed parameters, or a duplicated query parameter |
| `InvalidPart` | A part in the completion list does not match what was uploaded |
| `InvalidPartOrder` | Parts are not in ascending part-number order |
| `EntityTooSmall` | A non-final part is below the minimum part size |
| `QuotaExceeded` | The write would exceed the bucket's [quota](../administration/quotas.md) |
| `MalformedXML` | The request body is not valid XML |
| `BadDigest` | The received bytes do not match the supplied checksum |
| `AuthorizationHeaderMalformed` | The `Authorization` header could not be parsed |

### Other

| Status | Code | Cause |
| --- | --- | --- |
| `412` | `PreconditionFailed` | An `If-Match` or `If-Unmodified-Since` condition failed |
| `416` | `InvalidRange` | The requested range starts at or past the end of the object |
| `501` | `NotImplemented` | An [unsupported operation](s3-compatibility.md#unsupported) |
| `503` | `ServiceUnavailable` | The server is not ready |
| `500` | `InternalError` | Check the logs with the request ID |

## Management API errors

JSON:

```json
{
  "error": {
    "code": "BUCKET_NOT_FOUND",
    "message": "Bucket was not found",
    "request_id": "..."
  }
}
```

The request ID is also in `x-request-id`.

### Authentication

| Status | Code | Cause |
| --- | --- | --- |
| `401` | `UNAUTHORIZED` | No credential, or one that is not recognised |
| `403` | `FORBIDDEN` | Authenticated, but the role does not permit this route |

`FORBIDDEN` usually means a storage or auditor token on a route only the system role may
call. See [Authorization](../security/authorization.md).

### Resources

| Status | Code |
| --- | --- |
| `404` | `BUCKET_NOT_FOUND`, `OBJECT_NOT_FOUND`, `SHARE_NOT_FOUND`, `EMBED_NOT_FOUND`, `LIFECYCLE_RULE_NOT_FOUND`, `MULTIPART_UPLOAD_NOT_FOUND`, `ROUTE_NOT_FOUND` |
| `409` | `BUCKET_ALREADY_EXISTS`, `BUCKET_NOT_EMPTY`, `POLICY_ALREADY_EXISTS` |
| `404` | `OBJECT_DELETED` — the current version is a delete marker, reported distinctly from a missing key so a caller knows history exists and can be restored |

### Validation

| Code | Cause |
| --- | --- |
| `INVALID_BUCKET_NAME` | Naming rules |
| `INVALID_OBJECT_KEY` | Key rules |
| `INVALID_SERVICE_ACCOUNT_ID`, `INVALID_CREDENTIAL_ID`, `INVALID_POLICY_ID`, `INVALID_WEBHOOK_ID`, `INVALID_SHARE_ID`, `INVALID_EMBED_ID`, `INVALID_LIFECYCLE_RULE_ID` | Not a valid identifier |
| `INVALID_EXPIRATION` | Outside 60–86400 seconds |
| `INVALID_LIFECYCLE_RULE` | No expiration action, or the prefix is too long |
| `INVALID_LIMIT` | Outside the accepted range |
| `INVALID_AUDIT_CURSOR`, `INVALID_EVENT_CURSOR`, `INVALID_VERSION_CURSOR` | Only one of the two cursor fields was sent |
| `INVALID_CONTINUATION_TOKEN` | Malformed pagination token |
| `INVALID_ORIGIN` | Not a valid origin for an embed |
| `INVALID_WEBHOOK` | Configuration invalid, or the target is disallowed |
| `INVALID_SERVICE_ACCOUNT` | Name or description rejected |
| `QUOTA_EXCEEDED` | The operation would exceed a quota |

`INVALID_WEBHOOK` covers a target refused by the SSRF guards — a plain-HTTP URL with
`allow_http` off, or a private address with `allow_private_networks` off. See
[Events and Webhooks](../administration/events-and-webhooks.md).

### Sharing

| Code | Cause |
| --- | --- |
| `SHARING_UNAVAILABLE` | Sharing is not configured on this deployment |
| `CAPABILITY_REFUSED` | The request exceeds a deployment-wide sharing ceiling |
| `INVALID_CAPABILITY_REQUEST` | The share or embed request is malformed |
| `SHARE_UNAVAILABLE`, `EMBED_UNAVAILABLE` | Revoked, expired, or exhausted |
| `SHARE_PASSWORD_REQUIRED` | The share needs a password |
| `SHARE_PASSWORD_INCORRECT` | Wrong password |
| `SHARE_NOT_PERMITTED` | The share does not permit this operation |
| `SHARE_PREVIEW_UNSUPPORTED`, `PREVIEW_UNSUPPORTED` | The media type cannot be previewed |
| `EMBED_ORIGIN_DENIED` | The requesting origin is not on the allowlist |
| `EMBED_CONTENT_CHANGED` | The object is no longer the media type the embed was created for |
| `EMBED_WOULD_BROADEN` | Removing every origin restriction widens access |
| `SHARE_STILL_ACTIVE`, `EMBED_STILL_ACTIVE` | Revoke it before deleting the record |
| `RATE_LIMITED` | Too many attempts — try again shortly |

Three of these are refusals by design rather than bugs:

- **`EMBED_WOULD_BROADEN`** — stripping every origin from a restricted embed silently
  widens what an already-distributed URL can do. Revoke it and create a new one.
- **`SHARE_STILL_ACTIVE` / `EMBED_STILL_ACTIVE`** — deleting the record of a live
  capability would leave the URL working with no way to see it. Revoke first.
- **`EMBED_CONTENT_CHANGED`** — the object was replaced with a different media type,
  possibly one that must not be served inline.

### Service

| Status | Code | Cause |
| --- | --- | --- |
| `503` | `SERVICE_NOT_READY` | A subsystem is not ready — check `/ready` and the logs |
| `500` | `INTERNAL_ERROR` | Check the logs with the request ID |

## Tracing an error

Every response carries a request ID, and the same ID is on the log line and the audit
event:

```bash
curl -G https://management.example.com/api/v1/audit/events \
  -H "Authorization: Bearer <your-management-token>" \
  --data-urlencode "request_id=<request id from the response header>"
```

That is the fastest path from a user's error report to what the server decided. See
[Monitoring](../operations/monitoring.md).
