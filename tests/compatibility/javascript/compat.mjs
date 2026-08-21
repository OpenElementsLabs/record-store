import {
  CompleteMultipartUploadCommand,
  CreateBucketCommand,
  CreateMultipartUploadCommand,
  GetObjectCommand,
  ListObjectVersionsCommand,
  ListObjectsV2Command,
  PutBucketVersioningCommand,
  PutObjectCommand,
  S3Client,
  UploadPartCommand,
} from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import crypto from "node:crypto";

const endpoint = process.env.OES_COMPAT_ENDPOINT ?? "http://127.0.0.1:7600";
const client = new S3Client({
  endpoint,
  region: "us-east-1",
  forcePathStyle: true,
  requestChecksumCalculation: "WHEN_REQUIRED",
  responseChecksumValidation: "WHEN_REQUIRED",
  credentials: {
    accessKeyId: process.env.OES_ROOT_ACCESS_KEY,
    secretAccessKey: process.env.OES_ROOT_SECRET_KEY,
  },
});
const bucket = `oes-js-${crypto.randomUUID().replaceAll("-", "").slice(0, 16)}`;

function require(condition, message) {
  if (!condition) throw new Error(message);
}

await client.send(new CreateBucketCommand({ Bucket: bucket }));
await client.send(new PutObjectCommand({ Bucket: bucket, Key: "single.txt", Body: "javascript-single" }));
const downloaded = await client.send(new GetObjectCommand({ Bucket: bucket, Key: "single.txt" }));
require(await downloaded.Body.transformToString() === "javascript-single", "download mismatch");
const listed = await client.send(new ListObjectsV2Command({ Bucket: bucket }));
require(listed.Contents.some(({ Key }) => Key === "single.txt"), "list mismatch");

const initiated = await client.send(new CreateMultipartUploadCommand({ Bucket: bucket, Key: "multipart.bin" }));
const first = await client.send(new UploadPartCommand({
  Bucket: bucket,
  Key: "multipart.bin",
  UploadId: initiated.UploadId,
  PartNumber: 1,
  Body: Buffer.alloc(5 * 1024 * 1024, 0x61),
}));
const second = await client.send(new UploadPartCommand({
  Bucket: bucket,
  Key: "multipart.bin",
  UploadId: initiated.UploadId,
  PartNumber: 2,
  Body: "tail",
}));
await client.send(new CompleteMultipartUploadCommand({
  Bucket: bucket,
  Key: "multipart.bin",
  UploadId: initiated.UploadId,
  MultipartUpload: { Parts: [
    { PartNumber: 1, ETag: first.ETag },
    { PartNumber: 2, ETag: second.ETag },
  ] },
}));

const putUrl = await getSignedUrl(
  client,
  new PutObjectCommand({ Bucket: bucket, Key: "presigned.txt" }),
  { expiresIn: 60 },
);
const presignedPut = await fetch(putUrl, { method: "PUT", body: "presigned" });
if (!presignedPut.ok) {
  const queryNames = [...new URL(putUrl).searchParams.keys()].join(",");
  throw new Error(`presigned PUT failed (${presignedPut.status}; query=${queryNames}): ${await presignedPut.text()}`);
}
const getUrl = await getSignedUrl(
  client,
  new GetObjectCommand({ Bucket: bucket, Key: "presigned.txt" }),
  { expiresIn: 60 },
);
require(await (await fetch(getUrl)).text() === "presigned", "presigned GET failed");

await client.send(new PutBucketVersioningCommand({
  Bucket: bucket,
  VersioningConfiguration: { Status: "Enabled" },
}));
const v1 = await client.send(new PutObjectCommand({ Bucket: bucket, Key: "versioned.txt", Body: "one" }));
const v2 = await client.send(new PutObjectCommand({ Bucket: bucket, Key: "versioned.txt", Body: "two" }));
require(v1.VersionId !== v2.VersionId, "version IDs were reused");
const historical = await client.send(new GetObjectCommand({
  Bucket: bucket,
  Key: "versioned.txt",
  VersionId: v1.VersionId,
}));
require(await historical.Body.transformToString() === "one", "historical version mismatch");
const versions = await client.send(new ListObjectVersionsCommand({ Bucket: bucket, Prefix: "versioned.txt" }));
require(versions.Versions.length === 2, "version listing mismatch");
console.log("AWS SDK for JavaScript v3 compatibility: ok");
