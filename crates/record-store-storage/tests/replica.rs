//! Local replica primitive tests.
//!
//! These exercise the operations distributed replication is built on: streaming
//! a replica in with independent verification, streaming it out with integrity
//! checking, detecting corruption, and being safe to retry.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::{TryStreamExt, stream};
use oes_core::{Checksum, ObjectId, PayloadFormat};
use oes_metadata::RedbMetadataRepository;
use oes_storage::{
    DownloadStream, LocalFilesystemStore, ReadReplicaRequest, ReplicaCommitment, ReplicaStore,
    StorageError, WriteReplicaRequest, upload_stream,
};
use sha2::{Digest, Sha256};
use tempfile::tempdir;

async fn store() -> (tempfile::TempDir, LocalFilesystemStore) {
    let directory = tempdir().expect("temporary directory");
    let repository = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata/catalog.redb"))
            .await
            .expect("metadata repository"),
    );
    let storage =
        LocalFilesystemStore::open(directory.path(), directory.path().join("tmp"), repository)
            .await
            .expect("local store");
    (directory, storage)
}

fn body(chunks: &[&'static [u8]]) -> oes_storage::UploadStream {
    let chunks = chunks
        .iter()
        .map(|chunk| Ok(Bytes::from_static(chunk)))
        .collect::<Vec<_>>();
    upload_stream(stream::iter(chunks))
}

fn checksum(payload: &[u8]) -> Checksum {
    Checksum::sha256(Sha256::digest(payload).into())
}

async fn collect(stream: DownloadStream) -> Result<Vec<u8>, StorageError> {
    stream
        .try_fold(Vec::new(), |mut collected, chunk| async move {
            collected.extend_from_slice(&chunk);
            Ok(collected)
        })
        .await
}

fn payload_path(root: &std::path::Path, object_id: ObjectId) -> std::path::PathBuf {
    let encoded = object_id.as_uuid().simple().to_string();
    root.join("objects")
        .join(&encoded[0..2])
        .join(&encoded[2..4])
        .join(encoded)
}

#[tokio::test]
async fn a_replica_is_written_verified_and_streamed_back() {
    let (_directory, storage) = store().await;
    let object_id = ObjectId::new();
    let payload = b"replicated-payload".to_vec();
    let result = storage
        .write_replica(WriteReplicaRequest::known(
            "operation-1",
            object_id,
            payload.len() as u64,
            checksum(&payload),
            body(&[b"replicated-", b"payload"]),
        ))
        .await
        .expect("write replica");
    assert_eq!(result.object_id, object_id);
    assert_eq!(result.size, payload.len() as u64);
    assert_eq!(result.checksum, checksum(&payload));
    assert!(!result.already_present);

    let read = storage
        .read_replica(ReadReplicaRequest {
            object_id,
            size: payload.len() as u64,
            payload_format: PayloadFormat::Plaintext,
            range: None,
            expected_checksum: Some(checksum(&payload)),
        })
        .await
        .expect("read replica");
    assert_eq!(collect(read.body).await.expect("stream replica"), payload);

    let verification = storage
        .verify_replica(
            object_id,
            payload.len() as u64,
            PayloadFormat::Plaintext,
            checksum(&payload),
        )
        .await
        .expect("verify replica");
    assert!(verification.present);
    assert!(verification.matches);
}

#[tokio::test]
async fn a_replica_whose_bytes_do_not_match_is_refused_before_commit() {
    let (directory, storage) = store().await;
    let object_id = ObjectId::new();
    let error = storage
        .write_replica(WriteReplicaRequest::known(
            "operation-2",
            object_id,
            4,
            checksum(b"good"),
            body(&[b"evil"]),
        ))
        .await
        .expect_err("a mismatched replica must be refused");
    assert!(matches!(error, StorageError::ChecksumMismatch { .. }));
    assert!(
        !payload_path(directory.path(), object_id).exists(),
        "a refused replica must never be published"
    );
}

#[tokio::test]
async fn retrying_a_transfer_does_not_create_a_second_replica() {
    let (_directory, storage) = store().await;
    let object_id = ObjectId::new();
    let payload = b"idempotent".to_vec();
    for attempt in 0..3 {
        let result = storage
            .write_replica(WriteReplicaRequest::known(
                "operation-3",
                object_id,
                payload.len() as u64,
                checksum(&payload),
                body(&[b"idempotent"]),
            ))
            .await
            .expect("write replica");
        assert_eq!(result.size, payload.len() as u64);
        assert_eq!(
            result.already_present,
            attempt > 0,
            "only the first attempt writes bytes"
        );
    }
}

#[tokio::test]
async fn a_corrupt_replica_is_detected_and_never_served_silently() {
    let (directory, storage) = store().await;
    let object_id = ObjectId::new();
    let payload = b"original-content".to_vec();
    storage
        .write_replica(WriteReplicaRequest::known(
            "operation-4",
            object_id,
            payload.len() as u64,
            checksum(&payload),
            body(&[b"original-content"]),
        ))
        .await
        .expect("write replica");

    // Corrupt the stored bytes behind the storage layer's back.
    let path = payload_path(directory.path(), object_id);
    let mut corrupted = std::fs::read(&path).expect("read replica");
    corrupted[0] ^= 0xff;
    std::fs::write(&path, &corrupted).expect("corrupt replica");

    let verification = storage
        .verify_replica(
            object_id,
            payload.len() as u64,
            PayloadFormat::Plaintext,
            checksum(&payload),
        )
        .await
        .expect("verify replica");
    assert!(verification.present);
    assert!(!verification.matches, "corruption must be detected");

    let read = storage
        .read_replica(ReadReplicaRequest {
            object_id,
            size: payload.len() as u64,
            payload_format: PayloadFormat::Plaintext,
            range: None,
            expected_checksum: Some(checksum(&payload)),
        })
        .await
        .expect("open corrupt replica");
    let error = collect(read.body)
        .await
        .expect_err("a corrupt replica must fail the read rather than return bad bytes");
    assert!(matches!(error, StorageError::IntegrityMismatch));
}

#[tokio::test]
async fn a_missing_replica_reports_absence_rather_than_failing() {
    let (_directory, storage) = store().await;
    let object_id = ObjectId::new();
    let verification = storage
        .verify_replica(
            object_id,
            10,
            PayloadFormat::Plaintext,
            checksum(b"nothing"),
        )
        .await
        .expect("verify missing replica");
    assert!(!verification.present);
    assert!(!verification.matches);
    assert!(
        storage
            .stat_replica(object_id)
            .await
            .expect("stat")
            .is_none()
    );
    assert!(!storage.delete_replica(object_id).await.expect("delete"));
}

#[tokio::test]
async fn local_payloads_can_be_enumerated_for_reconciliation() {
    let (_directory, storage) = store().await;
    let mut written = Vec::new();
    for index in 0_u8..5 {
        let object_id = ObjectId::new();
        let payload = vec![index; 16];
        let chunk = Bytes::from(payload.clone());
        storage
            .write_replica(WriteReplicaRequest::known(
                format!("operation-{index}"),
                object_id,
                payload.len() as u64,
                checksum(&payload),
                upload_stream(stream::iter(vec![Ok(chunk)])),
            ))
            .await
            .expect("write replica");
        written.push(object_id);
    }
    written.sort();
    let listed = storage
        .list_local_payloads(None, 100)
        .await
        .expect("list payloads");
    assert_eq!(listed, written);

    let page = storage
        .list_local_payloads(Some(written[1]), 2)
        .await
        .expect("list payloads");
    assert_eq!(page, written[2..4].to_vec());
}

#[tokio::test]
async fn encrypted_nodes_store_ciphertext_but_replicate_logical_bytes() {
    let directory = tempdir().expect("temporary directory");
    let repository = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata/catalog.redb"))
            .await
            .expect("metadata repository"),
    );
    let storage = LocalFilesystemStore::open_encrypted(
        directory.path(),
        directory.path().join("tmp"),
        repository,
        b"cluster-master-key-that-is-long-enough",
    )
    .await
    .expect("encrypted store");

    let object_id = ObjectId::new();
    let payload = b"encrypted-at-rest".to_vec();
    let result = storage
        .write_replica(WriteReplicaRequest::known(
            "operation-encrypted",
            object_id,
            payload.len() as u64,
            checksum(&payload),
            body(&[b"encrypted-at-rest"]),
        ))
        .await
        .expect("write replica");
    // The logical checksum is over plaintext, so replicas remain comparable
    // across nodes even when each node encrypts with its own data key.
    assert_eq!(result.checksum, checksum(&payload));

    let stored = std::fs::read(payload_path(directory.path(), object_id)).expect("read replica");
    assert!(
        !stored
            .windows(payload.len())
            .any(|window| window == payload),
        "durable bytes must not contain the plaintext"
    );

    let read = storage
        .read_replica(ReadReplicaRequest {
            object_id,
            size: payload.len() as u64,
            payload_format: PayloadFormat::Aes256GcmEnvelopeV1,
            range: None,
            expected_checksum: Some(checksum(&payload)),
        })
        .await
        .expect("read replica");
    assert_eq!(collect(read.body).await.expect("stream replica"), payload);
}

#[tokio::test]
async fn a_trailing_commitment_is_verified_after_the_last_byte() {
    let (directory, storage) = store().await;
    let object_id = ObjectId::new();
    let payload = b"streamed-while-unknown".to_vec();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let write = tokio::spawn({
        let storage = storage.clone();
        async move {
            storage
                .write_replica(WriteReplicaRequest::trailing(
                    "operation-trailing",
                    object_id,
                    receiver,
                    body(&[b"streamed-", b"while-unknown"]),
                ))
                .await
        }
    });
    sender
        .send(Ok(ReplicaCommitment {
            size: payload.len() as u64,
            checksum: checksum(&payload),
        }))
        .expect("send commitment");
    let result = write.await.expect("join").expect("write replica");
    assert_eq!(result.size, payload.len() as u64);
    assert_eq!(result.checksum, checksum(&payload));

    // A commitment that disagrees with the received bytes must be refused.
    let other = ObjectId::new();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let write = tokio::spawn({
        let storage = storage.clone();
        async move {
            storage
                .write_replica(WriteReplicaRequest::trailing(
                    "operation-trailing-bad",
                    other,
                    receiver,
                    body(&[b"different"]),
                ))
                .await
        }
    });
    sender
        .send(Ok(ReplicaCommitment {
            size: payload.len() as u64,
            checksum: checksum(&payload),
        }))
        .expect("send commitment");
    let error = write
        .await
        .expect("join")
        .expect_err("a mismatched commitment must be refused");
    assert!(matches!(error, StorageError::ChecksumMismatch { .. }));
    assert!(!payload_path(directory.path(), other).exists());
}

#[tokio::test]
async fn an_aborted_upload_never_publishes_a_replica() {
    let (directory, storage) = store().await;
    let object_id = ObjectId::new();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let write = tokio::spawn({
        let storage = storage.clone();
        async move {
            storage
                .write_replica(WriteReplicaRequest::trailing(
                    "operation-aborted",
                    object_id,
                    receiver,
                    body(&[b"partial"]),
                ))
                .await
        }
    });
    sender
        .send(Err("client disconnected".to_owned()))
        .expect("send failure");
    let error = write
        .await
        .expect("join")
        .expect_err("an aborted upload must not commit");
    assert!(matches!(error, StorageError::UploadStream(_)));
    assert!(!payload_path(directory.path(), object_id).exists());
}
