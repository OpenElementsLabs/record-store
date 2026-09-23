# RSG-009: A truncated ListMultipartUploads page carries no NextKeyMarker

| | |
| --- | --- |
| Status | open |
| Severity | low — the AWS SDK paginator copes; a spec-following client may not |
| Blocks release | no |
| Gate | COR-PAGINATION (recorded as a note) |
| Found | 2026-09-23, candidate `417091a` |

A truncated `ListMultipartUploads` response returns `NextUploadIdMarker` but not
`NextKeyMarker`, and the server ignores `key-marker`: a request carrying only a
key marker starts from the first upload again. S3 returns both markers on a
truncated page and resumes from the pair. boto3's paginator resumes from
`NextUploadIdMarker` alone and sees every upload exactly once, which is what the
gate asserts; a client that follows the specification and sends only
`KeyMarker` would loop over the first page.
