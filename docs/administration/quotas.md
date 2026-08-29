# Quotas

A quota bounds how much a bucket may hold. Both dimensions default to unlimited.

| Dimension | Limits |
| --- | --- |
| `bytes` | Current logical bytes in the bucket |
| `objects` | Currently visible objects in the bucket |

## Setting a quota

Send both dimensions. Each is either `unlimited` or a `limit` with a value:

```bash
curl -X PUT https://management.example.com/api/v1/buckets/uploads/quota \
  -H "Authorization: Bearer <your-management-token>" \
  -H "Content-Type: application/json" \
  -d '{"quota":{"bytes":{"mode":"limit","bytes":10737418240},"objects":{"mode":"unlimited"}}}'
```

That caps the bucket at 10 GiB with no object-count limit.

To remove a quota, set both back to unlimited:

```bash
curl -X PUT https://management.example.com/api/v1/buckets/uploads/quota \
  -H "Authorization: Bearer <your-management-token>" \
  -H "Content-Type: application/json" \
  -d '{"quota":{"bytes":{"mode":"unlimited"},"objects":{"mode":"unlimited"}}}'
```

The bucket record, including its quota, comes back in the response.

## Enforcement

A write that would take the bucket past either limit is refused. The check happens
before the object is committed, so a rejected upload leaves nothing behind.

Enforcement is on **current** usage. On a versioned bucket, non-current versions do not
count toward the quota — which means a bucket can grow on disk while staying inside
its quota. Watch both:

```bash
record-store storage inspect --endpoint https://management.example.com
```

See [Capacity Planning](../operations/capacity-planning.md).

## Lowering a quota below current usage

This is refused. Setting a quota is itself validated against the bucket's present
usage: if the bucket already holds more than the proposed limit, the request fails and
the existing quota stays in place.

To shrink a bucket, delete objects first — or add a
[lifecycle rule](lifecycle-rules.md) and lower the quota once it has run.

## What a quota does not do

- It does not reserve space. Two buckets each capped at 10 GiB on a 15 GiB disk will
  still fill the disk.
- It does not bound in-progress multipart uploads before completion.
- It is per bucket. There is no deployment-wide quota.

Use quotas to stop one bucket consuming a shared deployment, and disk monitoring to
stop the deployment running out. They answer different questions.
