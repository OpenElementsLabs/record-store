//! Shared bucket and object application services.

mod admin;
mod admission;
mod bucket;
mod error;
mod event_pump;
mod lock;
#[cfg(test)]
mod lock_tests;
mod metrics;
mod multipart;
mod object;
mod services;
mod types;

#[cfg(test)]
mod test_support;

pub use bucket::BucketService;
pub use error::ServiceError;
pub use event_pump::{EventPumpGate, StorageEventPump};
pub use lock::{
    LockContext, LockedBucket, ObjectLockService, RetainedVersion, RetentionReport,
    RetentionStatus, VersionLock,
};
pub use metrics::{ServiceMetrics, ServiceMetricsSnapshot};
pub use object::ObjectService;
pub use services::{ObjectLockLimits, ServiceLimits, Services};
pub use types::{
    CopyMetadataDirective, ServiceCompleteMultipartRequest, ServiceCopyRequest,
    ServiceCreateMultipartRequest, ServiceDeleteResult, ServiceGetResult,
    ServiceListMultipartUploadsRequest, ServiceListMultipartUploadsResult, ServiceListRequest,
    ServiceListResult, ServiceListVersionsRequest, ServiceListVersionsResult, ServicePutRequest,
    ServiceUploadPartRequest,
};
