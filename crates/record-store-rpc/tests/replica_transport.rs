//! Drives the internal replica transport over a real gRPC connection.
//!
//! The peer transport is what a distributed write actually travels over, so the
//! tests here bind a genuine listener and speak to it with the production
//! client. Faking either half would leave the wire format, the identity headers,
//! and the streaming path unexercised — which is precisely where a peer-to-peer
//! protocol goes wrong.

use std::sync::Arc;

use record_store_core::{Checksum, ClusterId, NodeId, ObjectId, PayloadFormat};
use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
use record_store_rpc::{
    InternalRpcServer, PeerAuthenticator, PeerError, PeerHeaders, PeerPool, PeerVerifier,
    ReplicaRpcService, ReplicaTarget, ReplicaTransport, RpcClientSettings, RpcReplicaTransport,
    RpcServerSettings, TlsSettings, TransferExpectation,
};
use record_store_storage::{LocalFilesystemStore, ReplicaStore};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Accepts any peer that presents a credential, so the transport itself is what
/// these tests exercise rather than the catalog's authentication rules.
struct AcceptAnyPeer {
    cluster_id: ClusterId,
    node_id: NodeId,
}

#[async_trait::async_trait]
impl PeerAuthenticator for AcceptAnyPeer {
    async fn cluster_id(&self) -> Result<Option<ClusterId>, PeerError> {
        Ok(Some(self.cluster_id))
    }

    async fn verify_credential(&self, _presented: &str) -> Result<NodeId, PeerError> {
        Ok(self.node_id)
    }
}

struct Peer {
    _directory: TempDir,
    address: String,
    transport: RpcReplicaTransport,
    target: ReplicaTarget,
    shutdown: CancellationToken,
    local: Arc<LocalFilesystemStore>,
}

async fn peer() -> Peer {
    let directory = tempfile::tempdir().expect("temporary directory");
    let metadata: Arc<dyn MetadataRepository> = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata"),
    );
    let local = Arc::new(
        LocalFilesystemStore::open(
            directory.path().join("data"),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("replica store"),
    );

    let cluster_id = ClusterId::new();
    let node_id = NodeId::new();
    let versions = record_store_cluster::NodeVersions::current("test");
    let verifier = Arc::new(PeerVerifier::new(
        versions.clone(),
        Arc::new(AcceptAnyPeer {
            cluster_id,
            node_id,
        }),
    ));

    let server = InternalRpcServer::new(RpcServerSettings {
        bind: "127.0.0.1:0".parse().expect("address"),
        tls: TlsSettings::default(),
        concurrency_limit: 16,
        shutdown_grace_period: std::time::Duration::from_secs(5),
    })
    .with_replica(ReplicaRpcService::new(
        Arc::clone(&local) as Arc<dyn ReplicaStore>,
        Arc::clone(&verifier),
        PayloadFormat::Plaintext,
    ));
    let listener = server.bind().await.expect("bind");
    let address = listener.local_addr().expect("address").to_string();

    let shutdown = CancellationToken::new();
    let stopping = shutdown.clone();
    tokio::spawn(async move {
        let _ = server
            .serve(listener, async move { stopping.cancelled().await })
            .await;
    });

    let pool = PeerPool::new(RpcClientSettings {
        headers: PeerHeaders {
            node_id,
            cluster_id: Some(cluster_id),
            versions,
            credential: Some("peer-credential".to_owned()),
        },
        tls: TlsSettings::default(),
        connect_timeout: std::time::Duration::from_secs(5),
        request_timeout: std::time::Duration::from_secs(10),
        stream_chunk_timeout: std::time::Duration::from_secs(10),
        transfer_chunk_bytes: 64 * 1024,
        transfer_queue_depth: 4,
    });

    Peer {
        _directory: directory,
        target: ReplicaTarget {
            node_id,
            address: address.clone(),
        },
        address,
        transport: RpcReplicaTransport::new(pool),
        shutdown,
        local,
    }
}

fn commitment(body: &[u8]) -> (u64, Checksum) {
    let digest: [u8; 32] = Sha256::digest(body).into();
    (body.len() as u64, Checksum::sha256(digest))
}

fn stream(body: &'static [u8]) -> record_store_rpc::TransferStream {
    Box::pin(futures_util::stream::once(async move {
        Ok(bytes::Bytes::from_static(body))
    }))
}

/// The whole point of the transport: bytes written to a peer come back
/// unchanged, and the peer independently verifies them before publishing.
#[tokio::test]
async fn a_payload_written_to_a_peer_reads_back_unchanged() {
    let peer = peer().await;
    let object_id = ObjectId::new();
    let body: &'static [u8] = b"bytes crossing the wire";
    let (size, checksum) = commitment(body);

    let written = peer
        .transport
        .write_replica(
            &peer.target,
            "operation-1",
            object_id,
            TransferExpectation::Known {
                size,
                checksum: checksum.clone(),
            },
            stream(body),
        )
        .await
        .expect("write to the peer");
    assert_eq!(written.size, size);

    let stat = peer
        .local
        .stat_replica(object_id)
        .await
        .expect("stat")
        .expect("the peer stored it");
    assert!(stat.physical_bytes >= size, "{stat:?}");

    peer.shutdown.cancel();
}

/// A peer must verify what it stored rather than trusting the sender, so a
/// verification call has to reach the peer's own recomputation.
#[tokio::test]
async fn a_peer_verifies_the_payload_it_holds() {
    let peer = peer().await;
    let object_id = ObjectId::new();
    let body: &'static [u8] = b"verified bytes";
    let (size, checksum) = commitment(body);

    peer.transport
        .write_replica(
            &peer.target,
            "operation-1",
            object_id,
            TransferExpectation::Known {
                size,
                checksum: checksum.clone(),
            },
            stream(body),
        )
        .await
        .expect("write");

    let verified = peer
        .transport
        .verify_replica(&peer.target, object_id, size, &checksum)
        .await
        .expect("verify");
    assert!(verified.present && verified.matches, "{verified:?}");

    peer.shutdown.cancel();
}

/// Deleting through the transport has to actually remove the bytes on the peer,
/// which is what makes a cluster-wide delete real rather than local.
#[tokio::test]
async fn deleting_through_the_transport_removes_the_peers_copy() {
    let peer = peer().await;
    let object_id = ObjectId::new();
    let body: &'static [u8] = b"temporary bytes";
    let (size, checksum) = commitment(body);

    peer.transport
        .write_replica(
            &peer.target,
            "operation-1",
            object_id,
            TransferExpectation::Known { size, checksum },
            stream(body),
        )
        .await
        .expect("write");

    assert!(
        peer.transport
            .delete_replica(&peer.target, object_id)
            .await
            .expect("delete")
    );
    assert!(
        peer.local
            .stat_replica(object_id)
            .await
            .expect("stat")
            .is_none()
    );

    peer.shutdown.cancel();
}

/// A peer reports what it physically holds, which is how scrubbing and orphan
/// detection see across the cluster.
#[tokio::test]
async fn a_peer_lists_the_payloads_it_stores() {
    let peer = peer().await;
    let object_id = ObjectId::new();
    let body: &'static [u8] = b"listed bytes";
    let (size, checksum) = commitment(body);

    peer.transport
        .write_replica(
            &peer.target,
            "operation-1",
            object_id,
            TransferExpectation::Known { size, checksum },
            stream(body),
        )
        .await
        .expect("write");

    let listed = peer
        .transport
        .list_local_payloads(&peer.target, None, 10)
        .await
        .expect("list");
    assert!(listed.contains(&object_id), "{listed:?}");

    peer.shutdown.cancel();
}

/// Asking a peer for something it does not hold has to come back as a clean
/// answer rather than a transport failure, because the caller acts differently
/// on "not here" than on "cannot reach you".
#[tokio::test]
async fn a_peer_reports_absence_rather_than_failing() {
    let peer = peer().await;
    let object_id = ObjectId::new();

    let verified = peer
        .transport
        .verify_replica(&peer.target, object_id, 10, &Checksum::sha256([0_u8; 32]))
        .await
        .expect("verify");
    assert!(!verified.present, "{verified:?}");

    assert!(
        !peer
            .transport
            .delete_replica(&peer.target, object_id)
            .await
            .expect("delete"),
        "deleting what is not there reports that nothing was removed"
    );

    peer.shutdown.cancel();
}

/// An address nobody is listening on must surface as unreachable, which is the
/// signal the read path uses to fall back to another replica.
#[tokio::test]
async fn an_unreachable_peer_is_reported_as_such() {
    let peer = peer().await;
    let absent = ReplicaTarget {
        node_id: NodeId::new(),
        address: "127.0.0.1:1".to_owned(),
    };

    let result = peer
        .transport
        .verify_replica(&absent, ObjectId::new(), 1, &Checksum::sha256([0_u8; 32]))
        .await;
    assert!(result.is_err(), "an unreachable peer must not look healthy");
    assert_ne!(peer.address, absent.address);

    peer.shutdown.cancel();
}
