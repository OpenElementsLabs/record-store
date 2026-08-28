//! Shared fixtures for capability service tests.

use std::str::FromStr;

use record_store_core::{BucketId, BucketName, ObjectKey};

use tempfile::tempdir;

use crate::model::{EmbedDisposition, SharePermission, VersionMode};
use crate::store::CapabilityStore;
use crate::ticket::TicketIssuer;
use crate::{CreateEmbedRequest, CreateShareRequest, SharingPolicy, SharingService};

pub(crate) const KEY: &[u8] = b"capability-test-master-key-at-least-32-bytes";

pub(crate) async fn service(policy: SharingPolicy) -> SharingService {
    let directory = tempdir().expect("temporary directory");
    let store = CapabilityStore::open(directory.path().join("sharing.redb"), KEY)
        .await
        .expect("open store");
    // The directory is intentionally leaked for the lifetime of the test
    // process so the open database file outlives this helper.
    std::mem::forget(directory);
    SharingService::new(store, policy, TicketIssuer::derive(KEY).expect("tickets"))
}

pub(crate) fn share_request() -> CreateShareRequest {
    CreateShareRequest {
        label: "Board review".to_owned(),
        bucket_id: BucketId::new(),
        bucket: BucketName::from_str("reports").expect("bucket"),
        key: ObjectKey::new("q1/summary.pdf").expect("key"),
        version: VersionMode::FollowCurrent,
        permission: SharePermission::ViewAndDownload,
        expires_at: None,
        password: None,
        maximum_access_count: None,
        created_by: "management:system-administrator".to_owned(),
    }
}

pub(crate) fn embed_request(content_type: &str) -> CreateEmbedRequest {
    CreateEmbedRequest {
        label: "Company website".to_owned(),
        bucket_id: BucketId::new(),
        bucket: BucketName::from_str("assets").expect("bucket"),
        key: ObjectKey::new("brand/logo.png").expect("key"),
        version: VersionMode::FollowCurrent,
        expires_at: None,
        allowed_origins: vec!["https://example.com".to_owned()],
        disposition: EmbedDisposition::Inline,
        content_type: Some(content_type.to_owned()),
        created_by: "management:system-administrator".to_owned(),
    }
}
