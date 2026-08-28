//! Durable single-node metadata catalog.

use record_store_core::{
    BucketId, MultipartUpload, ObjectKey, ObjectVersionRecord, PartNumber, UploadId,
};

pub(crate) fn bucket_key(id: BucketId) -> Vec<u8> {
    id.as_uuid().as_bytes().as_slice().to_vec()
}
pub(crate) fn object_key(bucket: BucketId, key: &ObjectKey) -> Vec<u8> {
    object_prefix(bucket, key.as_str())
}
pub(crate) fn object_prefix(bucket: BucketId, prefix: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + prefix.len());
    out.extend_from_slice(bucket.as_uuid().as_bytes().as_slice());
    out.extend_from_slice(prefix.as_bytes());
    out
}
pub(crate) fn exact_version_prefix(bucket: BucketId, key: &ObjectKey) -> Vec<u8> {
    let mut out = object_key(bucket, key);
    out.push(0);
    out
}
pub(crate) fn version_order_key(record: &ObjectVersionRecord) -> Vec<u8> {
    let bucket = match record {
        ObjectVersionRecord::Object { metadata, .. } => metadata.bucket_id,
        ObjectVersionRecord::DeleteMarker { marker, .. } => marker.bucket_id,
    };
    let mut out = exact_version_prefix(bucket, record.key());
    let inverted = u64::MAX - record.created_at().timestamp_micros().max(0) as u64;
    out.extend_from_slice(&inverted.to_be_bytes());
    out.extend_from_slice(record.version_id().as_uuid().as_bytes().as_slice());
    out
}
pub(crate) fn multipart_order_key(upload: &MultipartUpload) -> Vec<u8> {
    let mut out = object_key(upload.bucket_id, &upload.key);
    out.push(0);
    out.extend_from_slice(&(upload.initiated_at.timestamp_micros().max(0) as u64).to_be_bytes());
    out.extend_from_slice(upload.id.as_uuid().as_bytes().as_slice());
    out
}
pub(crate) fn part_key(id: UploadId, number: PartNumber) -> Vec<u8> {
    let mut out = id.as_uuid().as_bytes().as_slice().to_vec();
    out.extend_from_slice(&number.get().to_be_bytes());
    out
}
pub(crate) fn prefix_successor(prefix: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    for index in (0..out.len()).rev() {
        if out[index] != u8::MAX {
            out[index] += 1;
            out.truncate(index + 1);
            return out;
        }
    }
    out.push(u8::MAX);
    out
}
