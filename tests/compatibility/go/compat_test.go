package compatibility

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
)

func TestOESCompatibility(t *testing.T) {
	accessKey := os.Getenv("OES_ROOT_ACCESS_KEY")
	secretKey := os.Getenv("OES_ROOT_SECRET_KEY")
	if accessKey == "" || secretKey == "" {
		t.Fatal("OES_ROOT_ACCESS_KEY and OES_ROOT_SECRET_KEY are required")
	}
	endpoint := os.Getenv("OES_COMPAT_ENDPOINT")
	if endpoint == "" {
		endpoint = "http://127.0.0.1:7600"
	}
	ctx := context.Background()
	cfg, err := config.LoadDefaultConfig(
		ctx,
		config.WithRegion("us-east-1"),
		config.WithCredentialsProvider(credentials.NewStaticCredentialsProvider(accessKey, secretKey, "")),
	)
	if err != nil {
		t.Fatal(err)
	}
	client := s3.NewFromConfig(cfg, func(options *s3.Options) {
		options.BaseEndpoint = aws.String(endpoint)
		options.UsePathStyle = true
	})
	bucket := fmt.Sprintf("oes-go-%d", time.Now().UnixNano())
	if _, err = client.CreateBucket(ctx, &s3.CreateBucketInput{Bucket: aws.String(bucket)}); err != nil {
		t.Fatal(err)
	}
	if _, err = client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(bucket), Key: aws.String("single.txt"), Body: strings.NewReader("go-single"),
	}); err != nil {
		t.Fatal(err)
	}
	get, err := client.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String(bucket), Key: aws.String("single.txt")})
	if err != nil {
		t.Fatal(err)
	}
	body, err := io.ReadAll(get.Body)
	get.Body.Close()
	if err != nil || string(body) != "go-single" {
		t.Fatalf("download mismatch: %q, %v", body, err)
	}
	listed, err := client.ListObjectsV2(ctx, &s3.ListObjectsV2Input{Bucket: aws.String(bucket)})
	if err != nil || len(listed.Contents) != 1 || *listed.Contents[0].Key != "single.txt" {
		t.Fatalf("list mismatch: %#v, %v", listed, err)
	}

	created, err := client.CreateMultipartUpload(ctx, &s3.CreateMultipartUploadInput{
		Bucket: aws.String(bucket), Key: aws.String("multipart.bin"),
	})
	if err != nil {
		t.Fatal(err)
	}
	first, err := client.UploadPart(ctx, &s3.UploadPartInput{
		Bucket: aws.String(bucket), Key: aws.String("multipart.bin"), UploadId: created.UploadId,
		PartNumber: aws.Int32(1), Body: bytes.NewReader(bytes.Repeat([]byte{'a'}, 5*1024*1024)),
	})
	if err != nil {
		t.Fatal(err)
	}
	second, err := client.UploadPart(ctx, &s3.UploadPartInput{
		Bucket: aws.String(bucket), Key: aws.String("multipart.bin"), UploadId: created.UploadId,
		PartNumber: aws.Int32(2), Body: strings.NewReader("tail"),
	})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = client.CompleteMultipartUpload(ctx, &s3.CompleteMultipartUploadInput{
		Bucket: aws.String(bucket), Key: aws.String("multipart.bin"), UploadId: created.UploadId,
		MultipartUpload: &types.CompletedMultipartUpload{Parts: []types.CompletedPart{
			{PartNumber: aws.Int32(1), ETag: first.ETag},
			{PartNumber: aws.Int32(2), ETag: second.ETag},
		}},
	}); err != nil {
		t.Fatal(err)
	}

	presigner := s3.NewPresignClient(client)
	putURL, err := presigner.PresignPutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(bucket), Key: aws.String("presigned.txt"),
	}, s3.WithPresignExpires(time.Minute))
	if err != nil || putURL.URL == "" {
		t.Fatalf("presigned PUT failed: %v", err)
	}
	getURL, err := presigner.PresignGetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(bucket), Key: aws.String("single.txt"),
	}, s3.WithPresignExpires(time.Minute))
	if err != nil || getURL.URL == "" {
		t.Fatalf("presigned GET failed: %v", err)
	}

	if _, err = client.PutBucketVersioning(ctx, &s3.PutBucketVersioningInput{
		Bucket: aws.String(bucket), VersioningConfiguration: &types.VersioningConfiguration{Status: types.BucketVersioningStatusEnabled},
	}); err != nil {
		t.Fatal(err)
	}
	v1, err := client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(bucket), Key: aws.String("versioned.txt"), Body: strings.NewReader("one"),
	})
	if err != nil {
		t.Fatal(err)
	}
	v2, err := client.PutObject(ctx, &s3.PutObjectInput{
		Bucket: aws.String(bucket), Key: aws.String("versioned.txt"), Body: strings.NewReader("two"),
	})
	if err != nil || aws.ToString(v1.VersionId) == aws.ToString(v2.VersionId) {
		t.Fatalf("versioning mismatch: %v", err)
	}
	historical, err := client.GetObject(ctx, &s3.GetObjectInput{
		Bucket: aws.String(bucket), Key: aws.String("versioned.txt"), VersionId: v1.VersionId,
	})
	if err != nil {
		t.Fatal(err)
	}
	historicalBody, err := io.ReadAll(historical.Body)
	historical.Body.Close()
	if err != nil || string(historicalBody) != "one" {
		t.Fatalf("historical version mismatch: %q, %v", historicalBody, err)
	}
}
