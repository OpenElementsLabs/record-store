//! Durable single-node metadata catalog.

mod commands;
mod error;
mod keys;
mod redb_store;
mod repository;
mod schema;
mod snapshot;
mod tx;
mod types;

#[cfg(test)]
mod tests;

pub use commands::{MetadataCommand, MetadataOutcome, NewDeleteMarker, apply_command_tx};
pub use error::MetadataError;
pub use redb_store::RedbMetadataRepository;
pub use repository::MetadataRepository;
pub use schema::METADATA_SCHEMA_VERSION;
pub use snapshot::{MetadataEntry, export_tx, import_tx};
pub use types::{
    BucketUsageSummary, DeleteObjectResult, DeleteVersionResult, ListMultipartUploadsRequest,
    ListObjectVersionsRequest, ListObjectsRequest, ListedObjectVersion, MultipartCleanupResult,
    MultipartUploadPage, ObjectCommitResult, ObjectMetadataPage, ObjectVersionPage,
    PayloadReferencePage,
};
