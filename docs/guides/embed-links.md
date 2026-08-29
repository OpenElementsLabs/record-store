# Embed Links

An embed link gives a website or an application a read-only URL for one object's
bytes. It is delivered by the **S3 API on port 7600** at `/e/<token>`.

```text
https://storage.example.com/e/<opaque-token>
```

## Why the storage endpoint

An embed serves object bytes into somebody else's page, so it is published where
object bytes already live. That is what lets a deployment expose storage to the
internet while the management plane and the console stay closed.

Set `sharing.embed_base_url` when storage is published under its own hostname. If it
is unset, Record Store falls back to the cluster S3 endpoint and then to the S3
listener address — correct for a local install, wrong behind any proxy.

## Using one

```html
<img src="https://storage.example.com/e/<token>" alt="Product photo">
```

```html
<video controls src="https://storage.example.com/e/<token>"></video>
```

Only media Record Store is prepared to be responsible for may be served inline:
images, video, and audio. See [Object Preview](object-preview.md) for the exact list.

!!! warning "HTML, SVG, and script are never embeddable inline"
    Creating an embed for one of those types is refused at creation time, not at
    delivery. The object stays downloadable as an attachment.

## Creating one

=== "Console"

    Open the object and choose **Embed**. Set a label and the origins that may load it.

=== "Management API"

    ```bash
    curl -X POST \
      -H "Authorization: Bearer $RECORD_STORE_MANAGEMENT_TOKEN" \
      -H 'content-type: application/json' \
      -d '{
            "label": "Marketing site hero",
            "allowed_origins": ["https://www.example.com"],
            "disposition": "inline"
          }' \
      https://console.example.com/api/v1/buckets/assets/object-embeds/hero.png
    ```

## Origin restrictions

`allowed_origins` is the access control. Every entry is validated; one malformed
origin fails the whole request rather than being stored alongside good ones and
quietly widening the policy.

Narrow it later without reissuing the link:

```bash
curl -X PATCH \
  -H "Authorization: Bearer $RECORD_STORE_MANAGEMENT_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"allowed_origins":["https://www.example.com"]}' \
  https://console.example.com/api/v1/embeds/<embed-id>
```

## Options

| Option | Effect |
| --- | --- |
| `allowed_origins` | Which web origins may load the bytes |
| `disposition` | `inline` or `attachment` |
| `expires_at` | The link stops working at this time |
| Version | Follow the current version, or pin a specific `VersionId` |

## Caching

Embed responses use a short, bounded revalidation window rather than `no-store`, so a
busy site is not re-fetching every byte on every page view. That is a deliberate
trade: revocation takes effect on the next revalidation rather than the next request.

Share links, which are for people and may be sensitive, use `no-store` instead.

## Revoking

```bash
curl -X POST \
  -H "Authorization: Bearer $RECORD_STORE_MANAGEMENT_TOKEN" \
  https://console.example.com/api/v1/embeds/<embed-id>/revoke
```

`DELETE /api/v1/embeds/<embed-id>` removes the record entirely.

## Deployment-wide policy

`sharing.embeds_enabled` turns the feature off. `sharing.maximum_lifetime_days` and
`sharing.require_expiration` apply to embeds as well as shares.

See [Sharing Security](../security/sharing-security.md).
