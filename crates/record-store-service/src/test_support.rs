//! Shared fixtures for service-layer tests.
//!
//! The services are built over a real catalog and a real filesystem store in a
//! throwaway directory. Using the genuine backends keeps these tests honest
//! about the behaviour callers actually get.

use std::sync::Arc;

use record_store_core::OrganizationId;
use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
use record_store_storage::{LocalFilesystemStore, ObjectStore};
use tempfile::TempDir;

use crate::{ServiceLimits, Services};

/// Builds services backed by a temporary catalog and object store.
pub(crate) async fn services_with(limits: ServiceLimits) -> (TempDir, Services) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let metadata: Arc<dyn MetadataRepository> = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let storage: Arc<dyn ObjectStore> = Arc::new(
        LocalFilesystemStore::open(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("filesystem store"),
    );
    let services = Services::new(storage, metadata, OrganizationId::new(), limits);
    (directory, services)
}

/// Builds services with generous limits, for tests that do not exercise them.
pub(crate) async fn services() -> (TempDir, Services) {
    services_with(ServiceLimits {
        maximum_concurrent_operations: 8,
        admission_wait_limit_seconds: 5,
        maximum_custom_metadata_entries: 8,
        maximum_custom_metadata_bytes: 1_024,
        object_lock: crate::ObjectLockLimits::default(),
    })
    .await
}

/// Builds services with a durable audit trail, returning it for inspection.
///
/// Object Lock bypass records are the reason this exists: a test that only
/// checked the operation succeeded would not notice the audit trail going
/// missing, which is the part an auditor actually relies on.
pub(crate) async fn services_with_audit() -> (
    TempDir,
    Services,
    Arc<record_store_audit::RedbAuditRepository>,
) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let metadata: Arc<dyn MetadataRepository> = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let storage: Arc<dyn ObjectStore> = Arc::new(
        LocalFilesystemStore::open(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("filesystem store"),
    );
    let audit = Arc::new(
        record_store_audit::RedbAuditRepository::open(directory.path().join("audit.redb"))
            .await
            .expect("audit repository"),
    );
    let services = Services::new_with_audit(
        storage,
        metadata,
        OrganizationId::new(),
        ServiceLimits {
            maximum_concurrent_operations: 8,
            admission_wait_limit_seconds: 5,
            maximum_custom_metadata_entries: 8,
            maximum_custom_metadata_bytes: 1_024,
            object_lock: crate::ObjectLockLimits::default(),
        },
        audit.clone(),
    );
    (directory, services, audit)
}

/// Wraps bytes as an upload stream the object services accept.
pub(crate) fn body(bytes: &[u8]) -> record_store_storage::UploadStream {
    let owned = bytes::Bytes::copy_from_slice(bytes);
    record_store_storage::upload_stream(futures_util::stream::once(async move { Ok(owned) }))
}
