//! Streaming object storage boundary and local filesystem implementation.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use record_store_core::{BucketId, ObjectId, ObjectKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Safe directory layout for the local filesystem backend.
#[derive(Debug, Clone)]
pub(crate) struct StorageLayout {
    pub(crate) objects: PathBuf,
    pub(crate) temporary: PathBuf,
    pub(crate) system: PathBuf,
}

impl StorageLayout {
    pub(crate) fn new(data_directory: &Path, temporary_directory: &Path) -> Self {
        Self {
            objects: data_directory.join("objects"),
            temporary: temporary_directory.to_path_buf(),
            system: data_directory.join("system"),
        }
    }

    pub(crate) fn payload_path(&self, object_id: ObjectId) -> PathBuf {
        let encoded = object_id.as_uuid().simple().to_string();
        self.objects
            .join(&encoded[0..2])
            .join(&encoded[2..4])
            .join(encoded)
    }

    pub(crate) fn temporary_path(&self, object_id: ObjectId) -> PathBuf {
        self.temporary
            .join(format!("{}.upload", object_id.as_uuid().simple()))
    }

    pub(crate) fn publication_path(&self, object_id: ObjectId) -> PathBuf {
        self.temporary
            .join(format!("{}.publish", object_id.as_uuid().simple()))
    }

    pub(crate) fn replica_temporary_path(
        &self,
        object_id: ObjectId,
        operation_id: &str,
    ) -> PathBuf {
        // The operation identity is hashed so a peer-supplied value can never
        // influence the filesystem path.
        let scope = hex::encode(&Sha256::digest(operation_id.as_bytes())[..8]);
        self.temporary
            .join(format!("{}-{scope}.replica", object_id.as_uuid().simple()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PublicationRecord {
    pub(crate) object_id: ObjectId,
    /// Present for object commits, absent for replica transfers.
    #[serde(default)]
    pub(crate) bucket_id: Option<BucketId>,
    /// Present for object commits, absent for replica transfers.
    #[serde(default)]
    pub(crate) key: Option<ObjectKey>,
}

pub(crate) const STORAGE_FORMAT_VERSION: u32 = 1;
pub(crate) const OBJECT_ENCRYPTION_FORMAT_VERSION: u32 = 1;
pub(crate) const OBJECT_ENCRYPTION_ALGORITHM: &str = "AES-256-GCM-ENVELOPE-CHUNKED";
pub(crate) const ENCRYPTED_PAYLOAD_MAGIC: &[u8; 8] = b"RSOBJV01";
pub(crate) const ENCRYPTED_PAYLOAD_HEADER_LEN: usize = 124;
pub(crate) const ENCRYPTED_PAYLOAD_CHUNK_SIZE: usize = 64 * 1024;
pub(crate) const AES_GCM_TAG_LEN: usize = 16;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StorageFormatRecord {
    pub(crate) storage_format_version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ObjectEncryptionRecord {
    pub(crate) encryption_format_version: u32,
    pub(crate) algorithm: String,
    pub(crate) key_reference: String,
}

#[derive(Clone)]
pub(crate) struct ObjectEncryption {
    pub(crate) key_encryption_key: Arc<Zeroizing<[u8; 32]>>,
    pub(crate) key_reference: [u8; 16],
}
