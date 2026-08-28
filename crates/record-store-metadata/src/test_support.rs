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

/// Opens a fresh catalog in a throwaway directory.
pub(crate) async fn catalog() -> (tempfile::TempDir, crate::RedbMetadataRepository) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let repository = crate::RedbMetadataRepository::open(directory.path().join("metadata.redb"))
        .await
        .expect("catalog");
    (directory, repository)
}

/// Opens a catalog and registers one bucket, returning both.
pub(crate) async fn catalog_with_bucket(
    name: &str,
) -> (tempfile::TempDir, crate::RedbMetadataRepository, Bucket) {
    use crate::MetadataRepository;
    let (directory, repository) = catalog().await;
    let bucket = bucket(name);
    repository.create_bucket(&bucket).await.expect("bucket");
    (directory, repository, bucket)
}

/// Builds a multipart upload for the given bucket and key.
pub(crate) fn upload(bucket_id: BucketId, key: &str) -> record_store_core::MultipartUpload {
    record_store_core::MultipartUpload {
        id: record_store_core::UploadId::new(),
        bucket_id,
        key: ObjectKey::new(key).expect("key"),
        content_type: None,
        custom_metadata: BTreeMap::new(),
        initiated_at: Utc::now(),
        state: record_store_core::MultipartUploadState::Active,
    }
}

/// Builds a stored part for a multipart upload.
pub(crate) fn part(
    upload_id: record_store_core::UploadId,
    number: u16,
    size: u64,
) -> record_store_core::UploadedPart {
    record_store_core::UploadedPart {
        upload_id,
        number: record_store_core::PartNumber::new(number).expect("part number"),
        object_id: ObjectId::new(),
        size,
        checksum: Checksum::sha256([number as u8; 32]),
        payload_format: record_store_core::PayloadFormat::Plaintext,
        etag: ETag::from_md5([number as u8; 16]),
        modified_at: Utc::now(),
    }
}
