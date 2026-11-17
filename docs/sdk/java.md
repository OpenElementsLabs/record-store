# Java

Use AWS SDK for Java v2. The compatibility suite pins version **2.54.12** and
runs with Java 21.

```xml
<dependency>
  <groupId>software.amazon.awssdk</groupId>
  <artifactId>s3</artifactId>
  <version>2.54.12</version>
</dependency>
```

## Client

The client reads its settings from the environment variables described in
[SDKs](index.md#environment-variables):

```java
import java.net.URI;
import software.amazon.awssdk.auth.credentials.AwsBasicCredentials;
import software.amazon.awssdk.auth.credentials.StaticCredentialsProvider;
import software.amazon.awssdk.regions.Region;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.S3Configuration;

S3Client s3 = S3Client.builder()
    .endpointOverride(URI.create(System.getenv("RECORD_STORE_ENDPOINT")))
    .region(Region.US_EAST_1)
    .credentialsProvider(StaticCredentialsProvider.create(AwsBasicCredentials.create(
        System.getenv("RECORD_STORE_ACCESS_KEY"), System.getenv("RECORD_STORE_SECRET_KEY"))))
    .forcePathStyle(true)
    .serviceConfiguration(S3Configuration.builder().chunkedEncodingEnabled(false).build())
    .build();
```

The following settings are specific to Record Store:

- `endpointOverride`: the URL of the Record Store S3 API, for example
  `https://storage.example.com`. The S3 API listens on port 7600 by default.
- `credentialsProvider`: a service account's S3 access key pair. The management
  token doesn't work with the S3 API.
- `region`: Record Store has no regions, but SigV4 signs a region into every
  request. Any value works if every client uses the same one.
- `forcePathStyle(true)`: required. Puts the bucket name in the URL path instead
  of the hostname.
- `chunkedEncodingEnabled(false)`: required. Stops the SDK from sending
  `aws-chunked` payloads, which Record Store rejects with `NotImplemented`.
  The SDK can keep its default checksum calculation.

## Payload signing

Header-authenticated `UNSIGNED-PAYLOAD` requests are accepted, including
CreateBucket, PutObject, and UploadPart. SigV4 credentials, signatures, and
authorization are still verified; the body is not covered by an unsigned payload
signature. Use HTTPS in production. Supplied SHA-256 checksums are still validated.

AWS streaming payloads and trailing checksums remain unsupported and return
HTTP 501 with S3 code `NotImplemented` and a message identifying the encoding.
Disabling chunked encoding is sufficient; no payload-signing override is needed.

Record Store **0.1.1** additionally rejects unsigned PUT requests. When using that
release, both disable chunked encoding and enable payload signing:

```java
.authSchemeProvider(params ->
    software.amazon.awssdk.services.s3.auth.scheme.S3AuthSchemeProvider.defaultProvider()
        .resolveAuthScheme(params).stream()
        .map(option -> option.toBuilder()
            .putSignerProperty(
                software.amazon.awssdk.http.auth.aws.signer.AwsV4FamilyHttpSigner.PAYLOAD_SIGNING_ENABLED,
                true)
            .build())
        .toList())
```

The compatibility test covers bucket creation, object upload/download, ranged
reads, HEAD, listing, multipart completion/abort, deletion, and the default
chunked-upload error.
