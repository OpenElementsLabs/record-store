//! Shared fixtures for catalog tests.

use std::collections::BTreeMap;

use chrono::Utc;
use record_store_core::{
    Bucket, BucketId, BucketName, BucketQuota, Checksum, CorsConfiguration, CorsMethod,
    CorsPattern, CorsRule, ETag, ObjectId, ObjectKey, ObjectMetadata, OrganizationId, VersionId,
    VersioningState,
};

pub(crate) fn bucket(name: &str) -> Bucket {
    Bucket {
        id: BucketId::new(),
        organization_id: OrganizationId::new(),
        name: BucketName::new(name).expect("bucket"),
        created_at: Utc::now(),
        versioning: VersioningState::Disabled,
        quota: BucketQuota::default(),
        durability_policy: None,
        cors: None,
    }
}
pub(crate) fn object(bucket: BucketId, key: &str, size: u64) -> ObjectMetadata {
    let now = Utc::now();
    ObjectMetadata {
        id: ObjectId::new(),
        bucket_id: bucket,
        key: ObjectKey::new(key).expect("key"),
        version_id: VersionId::new(),
        size,
        checksum: Checksum::sha256([1; 32]),
        payload_format: record_store_core::PayloadFormat::Plaintext,
        durability: record_store_core::DurabilityProfile::Single,
        etag: ETag::from_md5([2; 16]),
        content_type: None,
        custom_metadata: BTreeMap::new(),
        created_at: now,
        modified_at: now,
    }
}

pub(crate) fn cors_configuration() -> CorsConfiguration {
    CorsConfiguration {
        rules: vec![CorsRule {
            id: Some("browser-upload".into()),
            allowed_origins: vec![CorsPattern::origin("https://app.example.com").expect("origin")],
            allowed_methods: vec![CorsMethod::Put, CorsMethod::Get],
            allowed_headers: vec![CorsPattern::header("x-amz-*").expect("header")],
            expose_headers: vec!["ETag".into()],
            max_age_seconds: Some(600),
        }],
    }
}
