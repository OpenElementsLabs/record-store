use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use chrono::Utc;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures_util::{TryStreamExt, stream};
use oes_core::{
    Bucket, BucketId, BucketName, BucketQuota, ByteRange, ObjectKey, OrganizationId,
    VersioningState,
};
use oes_metadata::{MetadataRepository, RedbMetadataRepository};
use oes_storage::{
    GetObjectRequest, LocalFilesystemStore, ObjectStore, PutObjectRequest, upload_stream,
};
use tempfile::TempDir;
use tokio::runtime::Runtime;

struct Fixture {
    _directory: TempDir,
    store: LocalFilesystemStore,
    bucket_id: BucketId,
}

async fn create_fixture(encrypted: bool) -> Fixture {
    let directory = tempfile::tempdir().expect("temporary benchmark directory");
    let repository = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let bucket = Bucket {
        id: BucketId::new(),
        organization_id: OrganizationId::new(),
        name: BucketName::new("benchmark-bucket").expect("bucket name"),
        created_at: Utc::now(),
        versioning: VersioningState::Disabled,
        quota: BucketQuota::default(),
        durability_policy: None,
    };
    repository
        .create_bucket(&bucket)
        .await
        .expect("create benchmark bucket");
    let store = if encrypted {
        LocalFilesystemStore::open_encrypted(
            directory.path(),
            directory.path().join("tmp"),
            repository,
            b"benchmark-master-key-material-at-least-32-bytes",
        )
        .await
        .expect("encrypted filesystem store")
    } else {
        LocalFilesystemStore::open(directory.path(), directory.path().join("tmp"), repository)
            .await
            .expect("filesystem store")
    };
    Fixture {
        _directory: directory,
        store,
        bucket_id: bucket.id,
    }
}

fn request(bucket_id: BucketId, key: &str, payload: Bytes) -> PutObjectRequest {
    PutObjectRequest {
        bucket_id,
        key: ObjectKey::new(key).expect("object key"),
        content_type: None,
        custom_metadata: BTreeMap::new(),
        expected_checksum: None,
        object_id: None,
        protocol_etag: None,
        body: upload_stream(stream::once(async move { Ok(payload) })),
    }
}

fn storage_benchmarks(criterion: &mut Criterion) {
    let runtime = Runtime::new().expect("Tokio runtime");
    let fixture = runtime.block_on(create_fixture(false));
    let encrypted_fixture = runtime.block_on(create_fixture(true));

    let mut small = criterion.benchmark_group("local_filesystem_small_object");
    small.throughput(Throughput::Bytes(4 * 1024));
    small.bench_function("put_4_kib", |benchmark| {
        benchmark.to_async(&runtime).iter_batched(
            || Bytes::from(vec![0x5a; 4 * 1024]),
            |payload| {
                fixture
                    .store
                    .put(request(fixture.bucket_id, "small", payload))
            },
            BatchSize::SmallInput,
        );
    });
    small.finish();

    let mut encryption = criterion.benchmark_group("local_filesystem_encryption_overhead");
    encryption.throughput(Throughput::Bytes(1024 * 1024));
    for (name, target) in [
        ("plaintext", &fixture),
        ("aes_256_gcm_envelope_v1", &encrypted_fixture),
    ] {
        encryption.bench_with_input(
            BenchmarkId::new("put_1_mib", name),
            target,
            |bench, target| {
                bench.to_async(&runtime).iter_batched(
                    || Bytes::from(vec![0xa5; 1024 * 1024]),
                    |payload| {
                        target
                            .store
                            .put(request(target.bucket_id, "encryption", payload))
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    encryption.finish();

    runtime
        .block_on(fixture.store.put(request(
            fixture.bucket_id,
            "read-fixture",
            Bytes::from(vec![0x35; 1024 * 1024]),
        )))
        .expect("put read fixture");
    let mut reads = criterion.benchmark_group("local_filesystem_streaming_read");
    reads.throughput(Throughput::Bytes(1024 * 1024));
    reads.bench_function("get_1_mib", |benchmark| {
        benchmark.to_async(&runtime).iter(|| async {
            let result = fixture
                .store
                .get(GetObjectRequest {
                    bucket_id: fixture.bucket_id,
                    key: ObjectKey::new("read-fixture").expect("object key"),
                    range: None,
                })
                .await
                .expect("open read stream");
            result
                .body
                .try_fold(
                    0_usize,
                    |total, chunk| async move { Ok(total + chunk.len()) },
                )
                .await
                .expect("consume read stream")
        });
    });
    reads.throughput(Throughput::Bytes(64 * 1024));
    reads.bench_function("range_64_kib", |benchmark| {
        benchmark.to_async(&runtime).iter(|| async {
            let result = fixture
                .store
                .get(GetObjectRequest {
                    bucket_id: fixture.bucket_id,
                    key: ObjectKey::new("read-fixture").expect("object key"),
                    range: Some(ByteRange::new(256 * 1024, 64 * 1024).expect("range")),
                })
                .await
                .expect("open range stream");
            result
                .body
                .try_fold(
                    0_usize,
                    |total, chunk| async move { Ok(total + chunk.len()) },
                )
                .await
                .expect("consume range stream")
        });
    });
    reads.finish();
}

criterion_group!(benches, storage_benchmarks);
criterion_main!(benches);
