# Go

Use the AWS SDK for Go v2. The compatibility suite pins
`github.com/aws/aws-sdk-go-v2/service/s3 v1.107.3` and runs it against a real
Record Store server.

```bash
go get github.com/aws/aws-sdk-go-v2/config
go get github.com/aws/aws-sdk-go-v2/credentials
go get github.com/aws/aws-sdk-go-v2/service/s3
```

## Client

```go
package storage

import (
	"context"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

func New(ctx context.Context, endpoint, accessKey, secretKey string) (*s3.Client, error) {
	cfg, err := config.LoadDefaultConfig(
		ctx,
		config.WithRegion("us-east-1"),
		config.WithCredentialsProvider(
			credentials.NewStaticCredentialsProvider(accessKey, secretKey, ""),
		),
	)
	if err != nil {
		return nil, err
	}

	return s3.NewFromConfig(cfg, func(options *s3.Options) {
		options.BaseEndpoint = aws.String(endpoint)
		options.UsePathStyle = true
		options.RequestChecksumCalculation = aws.RequestChecksumCalculationWhenRequired
		options.ResponseChecksumValidation = aws.ResponseChecksumValidationWhenRequired
	}), nil
}
```

To create the client from the environment variables described in
[SDKs](index.md#environment-variables), call `New` as follows:

```go
client, err := storage.New(ctx,
	os.Getenv("RECORD_STORE_ENDPOINT"),
	os.Getenv("RECORD_STORE_ACCESS_KEY"),
	os.Getenv("RECORD_STORE_SECRET_KEY"),
)
```

The following settings are specific to Record Store:

- `BaseEndpoint`: the URL of the Record Store S3 API, for example
  `https://storage.example.com`. The S3 API listens on port 7600 by default.
- `accessKey` and `secretKey`: a service account's S3 access key pair. The
  management token doesn't work with the S3 API.
- `WithRegion("us-east-1")`: Record Store has no regions, but SigV4 signs a
  region into every request. Any value works if every client uses the same one.
- `UsePathStyle`: required. Puts the bucket name in the URL path instead of the
  hostname.
- `RequestChecksumCalculation` and `ResponseChecksumValidation`: required. Stop
  the SDK from sending upload checksums as `aws-chunked` trailers, which Record
  Store rejects with `NotImplemented`.

## Upload

```go
_, err := client.PutObject(ctx, &s3.PutObjectInput{
	Bucket:      aws.String("uploads"),
	Key:         aws.String("invoices/2026/03/inv-1.pdf"),
	Body:        file,
	ContentType: aws.String("application/pdf"),
})
```

For large objects use the manager, which does multipart automatically:

```go
import "github.com/aws/aws-sdk-go-v2/feature/s3/manager"

uploader := manager.NewUploader(client)
_, err := uploader.Upload(ctx, &s3.PutObjectInput{
	Bucket: aws.String("uploads"),
	Key:    aws.String("big.bin"),
	Body:   reader,
})
```

## Download

```go
object, err := client.GetObject(ctx, &s3.GetObjectInput{
	Bucket: aws.String("uploads"),
	Key:    aws.String("invoices/2026/03/inv-1.pdf"),
})
if err != nil {
	return err
}
defer object.Body.Close()

bytes, err := io.ReadAll(object.Body)
```

## List

```go
paginator := s3.NewListObjectsV2Paginator(client, &s3.ListObjectsV2Input{
	Bucket: aws.String("uploads"),
	Prefix: aws.String("invoices/2026/"),
})

for paginator.HasMorePages() {
	page, err := paginator.NextPage(ctx)
	if err != nil {
		return err
	}
	for _, object := range page.Contents {
		fmt.Println(*object.Key, *object.Size)
	}
}
```

## Delete

```go
_, err := client.DeleteObject(ctx, &s3.DeleteObjectInput{
	Bucket: aws.String("uploads"),
	Key:    aws.String("invoices/2026/03/inv-1.pdf"),
})
```

## Presigned URLs

```go
presigner := s3.NewPresignClient(client)

request, err := presigner.PresignPutObject(ctx, &s3.PutObjectInput{
	Bucket: aws.String("uploads"),
	Key:    aws.String(key),
}, s3.WithPresignExpires(15*time.Minute))
// request.URL is the presigned URL
```

## Ranges

```go
object, err := client.GetObject(ctx, &s3.GetObjectInput{
	Bucket: aws.String("uploads"),
	Key:    aws.String("big.bin"),
	Range:  aws.String("bytes=0-1023"),
})
```

## Checksums

The [client](#client) sets `RequestChecksumCalculation` and
`ResponseChecksumValidation` to their `WhenRequired` values. If you can't change
the client code, set the equivalent environment variables instead:

```bash
export AWS_REQUEST_CHECKSUM_CALCULATION=WHEN_REQUIRED
export AWS_RESPONSE_CHECKSUM_VALIDATION=WHEN_REQUIRED
```
