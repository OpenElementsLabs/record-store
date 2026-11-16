# Buckets, objects, and service accounts

This page explains the buckets and objects that you created in the
[Quick start](quick-start.md). It also shows you how to create the credentials
that an application uses.

## Buckets

A *bucket* is a named container for objects. A bucket name must follow
S3-compatible rules:

- It's 3–63 bytes long.
- It contains only lowercase letters, digits, hyphens, and dots.
- It starts and ends with a letter or a digit.
- It isn't formatted as an IP address.

The following names are reserved:

- Names that start with `xn--` or `sthree-`.
- Names that end with `-s3alias` or `--ol-s3`.
- The internal names `record-store-system` and `record-store-internal`.

To create a bucket named `photos` and list your buckets, run the following
commands:

```bash
record-store bucket create photos
record-store bucket list
```

To delete a bucket, first delete every object in it, and then run the following
command:

```bash
record-store bucket delete BUCKET_NAME
```

Replace `BUCKET_NAME` with the name of the bucket.

## Objects and keys

An *object* is a sequence of bytes plus metadata. A *key* identifies an object
within a bucket.

Keys look like file paths, but Record Store has no directories. For example,
`invoices/2026/03/inv-1.pdf` is one key that contains slashes. When you list
objects with a prefix and a delimiter, the results look like a folder tree. For
more information, see [Buckets and objects](../concepts/buckets-and-objects.md).

Record Store rejects keys like the ones in the following table:

| Key | Reason |
| --- | --- |
| `/leading` | Starts with a slash. |
| `../secret` | Contains path traversal. |
| `a//b` | Contains an empty segment. |
| `a\b` | Contains a backslash. |

Record Store never uses a bucket name or a key as a file system path. It stores
each payload under a generated UUID.

## Create a service account for your application

The root credentials exist to set up Record Store. Applications must use a
*service account* that has only the permissions that the application needs.

1.  Create a service account:

    ```bash
    record-store service-account create my-app
    ```

    The output is similar to the following:

    ```text
    Account ID     0195f0c8-....
    Access key     SA4D8F0BEE8270423EA5D1
    Secret key     <shown once>
    ```
 

> !!! warning "Save the secret key now"
>
>  Record Store encrypts the secret key and never shows it again. If you lose the secret key, rotate the credential. You can't recover it.


1.  A new service account has no policies, so it can't perform any actions.
    Create a file named `read-write-photos.json` that defines a policy:

    ```json title="read-write-photos.json"
    {
      "name": "read-write-photos",
      "description": "Full object access to the photos bucket",
      "statements": [
        {
          "effect": "allow",
          "actions": ["s3:ListBucket", "s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
          "resources": ["bucket:photos", "bucket:photos/*"]
        }
      ]
    }
    ```

1.  Create the policy and attach it to the service account:

    ```bash
    record-store policy create ./read-write-photos.json
    record-store policy attach POLICY_ID ACCOUNT_ID
    ```

    Replace the following:

    - `POLICY_ID`: the ID that the `policy create` command returns.
    - `ACCOUNT_ID`: the account ID that the `service-account create` command
      returned.

For more information about actions and resources, see
[Policies](../administration/policies.md).

## Turn off root access to the S3 API

After your applications use service accounts, stop the root credentials from
accessing the S3 API. Set the following environment variable for the Record
Store server:

```bash
RECORD_STORE_ROOT_S3_ENABLED=false
```

You can still use the root credentials for management. For more information,
see [Authentication](../security/authentication.md).

## What's next

- To design how your application uses Record Store, see
  [Application integration](../guides/application-integration.md).
- To connect from code, see the SDK guide for
  [JavaScript and TypeScript](../sdk/javascript.md), [Python](../sdk/python.md),
  [Go](../sdk/go.md), or [Rust](../sdk/rust.md).
- Before you go live, complete the
  [Production checklist](../deployment/production-checklist.md).
