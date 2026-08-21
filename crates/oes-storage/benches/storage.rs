use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use chrono::Utc;
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use futures_util::{TryStreamExt, stream};
use oes_core::{Bucket, BucketId, BucketName, ObjectKey, OrganizationId};
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

async fn fixture() -> Fixture {
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
    };
    repository
        .create_bucket(&bucket)
        .await
        .expect("create benchmark bucket");
    let store =
        LocalFilesystemStore::open(directory.path(), directory.path().join("tmp"), repository)
            .await
            .expect("filesystem store");
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
        body: upload_stream(stream::once(async move { Ok(payload) })),
    }
}

fn storage_benchmarks(criterion: &mut Criterion) {
    let runtime = Runtime::new().expect("Tokio runtime");
    let fixture = runtime.block_on(fixture());

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
    reads.finish();
}

criterion_group!(benches, storage_benchmarks);
criterion_main!(benches);
