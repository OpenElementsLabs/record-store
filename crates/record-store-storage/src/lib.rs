//! Streaming object storage boundary and local filesystem implementation.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use tokio::sync::RwLock;

mod encryption;
mod error;
mod layout;
mod local_store;
mod maintenance;
mod object_store;
mod replica;
mod types;

#[cfg(test)]
mod test_support;

pub use error::StorageError;
pub use local_store::LocalFilesystemStore;
pub use object_store::ObjectStore;
pub use replica::{
    ReadReplicaRequest, ReplicaCommitment, ReplicaExpectation, ReplicaReadResult, ReplicaStat,
    ReplicaStore, ReplicaVerification, ReplicaWriteResult, WriteReplicaRequest,
};
pub use types::{
    CompleteMultipartRequest, DeleteObjectRequest, DeleteObjectVersionRequest, DownloadStream,
    GetObjectRequest, GetObjectResult, GetObjectVersionRequest, HeadObjectRequest,
    PutMultipartPartRequest, PutObjectRequest, PutObjectResult, StorageInspection,
    StorageRepairRequest, StorageRepairResult, StorageStatus, UploadStream, VerifyObjectRequest,
    upload_stream,
};

pub(crate) type KeyLockRegistry = Arc<Mutex<HashMap<Vec<u8>, Weak<RwLock<()>>>>>;
