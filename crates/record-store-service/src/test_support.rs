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
        maximum_custom_metadata_entries: 8,
        maximum_custom_metadata_bytes: 1_024,
    })
    .await
}

/// Wraps bytes as an upload stream the object services accept.
pub(crate) fn body(bytes: &[u8]) -> record_store_storage::UploadStream {
    let owned = bytes::Bytes::copy_from_slice(bytes);
    record_store_storage::upload_stream(futures_util::stream::once(async move { Ok(owned) }))
}
