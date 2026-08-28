//! Shared bucket and object application services.

mod admin;
mod bucket;
mod error;
mod events;
mod metrics;
mod multipart;
mod object;
mod services;
mod types;

#[cfg(test)]
mod test_support;

pub use bucket::BucketService;
pub use error::ServiceError;
pub use metrics::{ServiceMetrics, ServiceMetricsSnapshot};
pub use object::ObjectService;
pub use services::{ServiceLimits, Services};
pub use types::{
    CopyMetadataDirective, ServiceCompleteMultipartRequest, ServiceCopyRequest,
    ServiceCreateMultipartRequest, ServiceDeleteResult, ServiceGetResult,
    ServiceListMultipartUploadsRequest, ServiceListMultipartUploadsResult, ServiceListRequest,
    ServiceListResult, ServiceListVersionsRequest, ServiceListVersionsResult, ServicePutRequest,
    ServiceUploadPartRequest,
};
