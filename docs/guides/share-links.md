# Share Links

A share link gives a person read access to exactly one object, through a page Record
Store renders. It is delivered by the **console on port 7602** at `/s/<token>`.

```text
https://console.example.com/s/<opaque-token>
```

## A capability, not a credential

The opaque token in the path is the entire authorization. It names one object and one
version policy and can express nothing else. A share link cannot list, write, delete,
or reach any other object, and it is never an S3 credential.

Every request re-resolves the token against durable state, so revoking a link takes
effect on the very next request. Share responses are served `no-store` so a revoked
link cannot be replayed from a cache.

Tokens carry 256 bits of entropy from the operating system's cryptographic generator.
They are stored as a lookup digest plus an AES-256-GCM-sealed copy under the
deployment's master key, so an administrator can copy the link again later without
Record Store holding it in the clear.

## Creating one

=== "Console"

    Open the object and choose **Share**. Set a label, and optionally an expiry, a
    password, an access budget, and whether to pin a version.

=== "Management API"

    ```bash
    curl -X POST \
      -H "Authorization: Bearer $RECORD_STORE_MANAGEMENT_TOKEN" \
      -H 'content-type: application/json' \
      -d '{
            "label": "Q1 board review",
            "permission": "view_and_download",
            "expires_at": "2026-12-31T23:59:59Z"
          }' \
      https://console.example.com/api/v1/buckets/reports/object-shares/q1.pdf
    ```

The response contains the share record **and the URL**. That is the only time the URL
is returned as part of creation; later reads of the share do not include the token.
An administrator can request it again from `GET /api/v1/shares/{id}/url`.

!!! note "There is no CLI command for share links"
    Shares and embeds are managed through the console or the management API.

## Options

| Option | Effect |
| --- | --- |
| `permission` | `view`, `download`, or `view_and_download` |
| `expires_at` | The link stops working at this time |
| `password` | The recipient must enter a password before anything is disclosed |
| `maximum_access_count` | A strict ceiling on successful accesses |
| Version | Follow the current version, or pin a specific `VersionId` |

### Version pinning

A share either **follows the current version** or is **pinned** to one `VersionId`.

Pinning is the right choice when the link refers to a document that must not change
under the recipient — a signed contract, an invoice. Following the current version is
right when the link refers to "the latest" of something.

The choice is recorded when the link is created and cannot be inferred later, so
Record Store asks for it rather than guessing.

### Passwords

A password-protected share discloses nothing before it is unlocked — not the file
name, not the size, not the media type. The recipient exchanges the password for a
short-lived unlock ticket, valid for `sharing.unlock_lifetime_hours`.

Passwords are stored as salted Argon2 hashes, never as a digest. Repeated attempts are
throttled per link **and** per client, so one attacker cannot lock a public link for
everybody else.

An unlock ticket is bound to the share it was issued for. It is useless against a
different one.

## Revoking

```bash
curl -X POST \
  -H "Authorization: Bearer $RECORD_STORE_MANAGEMENT_TOKEN" \
  https://console.example.com/api/v1/shares/<share-id>/revoke
```

Revocation keeps the record visible to administrators so you can see what you
withdrew. Deleting the share (`DELETE /api/v1/shares/<share-id>`) removes it entirely.

Both take effect on the next request.

## Deployment-wide policy

An operator can constrain what administrators are allowed to create:

| Setting | Effect |
| --- | --- |
| `sharing.shares_enabled` | Turn the feature off entirely |
| `sharing.maximum_lifetime_days` | Ceiling on how long a new link may live |
| `sharing.require_expiration` | Every new link must have an expiry |
| `sharing.require_share_password` | Every new link must have a password |
| `sharing.maximum_access_count` | Ceiling on the access budget |
| `sharing.share_base_url` | The origin links are written against |

See [Sharing Security](../security/sharing-security.md).

## Share links versus presigned URLs

| | Share link | [Presigned URL](presigned-urls.md) |
| --- | --- | --- |
| Created by | An administrator | Anyone holding an S3 credential |
| Revocable | Yes, immediately | No — valid until it expires |
| Delivered by | Console `:7602` | S3 API `:7600` |
| Audience | A person, via a rendered page | A program, via a raw HTTP request |
| Extra controls | Password, access budget | Expiry only |

If you need to withdraw access after handing something out, use a share link.
