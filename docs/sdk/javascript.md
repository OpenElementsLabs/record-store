# JavaScript and TypeScript

Use the AWS SDK for JavaScript v3.

```bash
npm install @aws-sdk/client-s3
```

For presigned URLs, also install:

```bash
npm install @aws-sdk/s3-request-presigner
```

## Client

```ts title="lib/record-store.ts"
import { S3Client } from "@aws-sdk/client-s3";

export const recordStore = new S3Client({
  endpoint: process.env.RECORD_STORE_ENDPOINT!, // (1)!
  region: "us-east-1",                          // (2)!
  forcePathStyle: true,                         // (3)!
  credentials: {
    accessKeyId: process.env.RECORD_STORE_ACCESS_KEY!,
    secretAccessKey: process.env.RECORD_STORE_SECRET_KEY!,
  },
});
```

1. Your Record Store S3 endpoint, for example `https://storage.example.com`.
2. Record Store has no regions, but SigV4 requires a region and the signature must
   match. Any consistent value works; `us-east-1` is conventional.
3. Required. Puts the bucket in the path rather than the hostname.

!!! danger "Server-side only"
    This module reads secrets from the environment. Never import it into browser code.
    See [Application Integration](../guides/application-integration.md).

## Upload

```ts
import { PutObjectCommand } from "@aws-sdk/client-s3";

await recordStore.send(
  new PutObjectCommand({
    Bucket: "uploads",
    Key: "invoices/2026/03/inv-1.pdf",
    Body: fileBuffer,
    ContentType: "application/pdf",
  }),
);
```

For large files, `@aws-sdk/lib-storage` handles multipart automatically:

```bash
npm install @aws-sdk/lib-storage
```

```ts
import { Upload } from "@aws-sdk/lib-storage";

await new Upload({
  client: recordStore,
  params: { Bucket: "uploads", Key: "big.bin", Body: stream },
}).done();
```

## Download

```ts
import { GetObjectCommand } from "@aws-sdk/client-s3";

const result = await recordStore.send(
  new GetObjectCommand({ Bucket: "uploads", Key: "invoices/2026/03/inv-1.pdf" }),
);

const bytes = await result.Body!.transformToByteArray();
```

`Body` is a stream. Pipe it rather than buffering when the object may be large.

## List

```ts
import { ListObjectsV2Command } from "@aws-sdk/client-s3";

let token: string | undefined;
do {
  const page = await recordStore.send(
    new ListObjectsV2Command({
      Bucket: "uploads",
      Prefix: "invoices/2026/",
      ContinuationToken: token,
    }),
  );
  for (const object of page.Contents ?? []) {
    console.log(object.Key, object.Size);
  }
  token = page.NextContinuationToken;
} while (token);
```

Listing is always paginated. A bucket may hold millions of objects, so no caller is
handed the whole keyspace.

## Delete

```ts
import { DeleteObjectCommand } from "@aws-sdk/client-s3";

await recordStore.send(
  new DeleteObjectCommand({ Bucket: "uploads", Key: "invoices/2026/03/inv-1.pdf" }),
);
```

Delete is idempotent — deleting an absent key succeeds. In a
[versioned bucket](../concepts/versioning.md) this writes a delete marker rather than
removing anything.

## Presigned URLs

```ts
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import { PutObjectCommand } from "@aws-sdk/client-s3";

const url = await getSignedUrl(
  recordStore,
  new PutObjectCommand({ Bucket: "uploads", Key: key, ContentType: contentType }),
  { expiresIn: 900 },
);
```

See [Presigned URLs](../guides/presigned-urls.md) and the
[Next.js guide](nextjs.md) for the browser-upload pattern.

## Range requests

```ts
const result = await recordStore.send(
  new GetObjectCommand({ Bucket: "uploads", Key: "big.bin", Range: "bytes=0-1023" }),
);
```

Bounded, open-ended (`bytes=1024-`), and suffix (`bytes=-1024`) ranges are supported.
An unsatisfiable range returns `416`.

## Checksums

Newer SDK versions may default to AWS's `aws-chunked` trailing-checksum encoding,
which Record Store reports as unsupported. If uploads fail with `NotImplemented`, set:

```bash
AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
```

Record Store validates SHA-256 checksums supplied the ordinary way, via the
`x-amz-checksum-sha256` header.
