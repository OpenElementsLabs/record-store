use axum::{
    http::{
        HeaderValue, StatusCode,
        header::{self},
    },
    response::{IntoResponse, Response},
};
use record_store_service::ServiceError;
use serde::Serialize;

use crate::sigv4::S3RequestId;
use crate::*;

pub(crate) fn service_error(
    error: ServiceError,
    request_id: S3RequestId,
    resource: &str,
) -> S3Error {
    let kind = match error {
        ServiceError::BucketNotFound => S3ErrorKind::NoSuchBucket,
        ServiceError::BucketAlreadyExists => S3ErrorKind::BucketAlreadyExists,
        ServiceError::BucketNotEmpty => S3ErrorKind::BucketNotEmpty,
        ServiceError::ObjectNotFound => S3ErrorKind::NoSuchKey,
        ServiceError::DeleteMarker(_) => S3ErrorKind::NoSuchKey,
        ServiceError::MultipartUploadNotFound => S3ErrorKind::NoSuchUpload,
        ServiceError::InvalidPart => S3ErrorKind::InvalidPart,
        ServiceError::InvalidPartOrder => S3ErrorKind::InvalidPartOrder,
        ServiceError::EntityTooSmall => S3ErrorKind::EntityTooSmall,
        ServiceError::QuotaExceeded => S3ErrorKind::QuotaExceeded,
        ServiceError::Core(_) => S3ErrorKind::InvalidRequest,
        ServiceError::MetadataTooLarge | ServiceError::InvalidRequest(_) => {
            S3ErrorKind::InvalidRequest
        }
        ServiceError::Storage(record_store_storage::StorageError::ChecksumMismatch { .. }) => {
            S3ErrorKind::BadDigest
        }
        ServiceError::ClusterUnavailable(_) | ServiceError::DurabilityNotMet(_) => {
            S3ErrorKind::ServiceUnavailable
        }
        ServiceError::Metadata(_)
        | ServiceError::Storage(_)
        | ServiceError::Coordination
        | ServiceError::Unavailable => S3ErrorKind::InternalError,
    };
    S3Error::new(kind, request_id, resource)
}

pub(crate) struct S3Error {
    pub(crate) kind: S3ErrorKind,
    pub(crate) request_id: S3RequestId,
    pub(crate) resource: String,
}

impl S3Error {
    pub(crate) fn new(kind: S3ErrorKind, request_id: S3RequestId, resource: &str) -> Self {
        Self {
            kind,
            request_id,
            resource: resource.to_owned(),
        }
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        let body = ErrorDocument {
            code: self.kind.code(),
            message: self.kind.message(),
            resource: &self.resource,
            request_id: &self.request_id.0,
        };
        let xml = quick_xml::se::to_string(&body).unwrap_or_else(|_| {
            "<Error><Code>InternalError</Code><Message>Internal error</Message></Error>".into()
        });
        (
            self.kind.status(),
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(XML_CONTENT_TYPE),
            )],
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>{xml}"),
        )
            .into_response()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum S3ErrorKind {
    AccessDenied,
    InvalidAccessKeyId,
    SignatureDoesNotMatch,
    AuthorizationHeaderMalformed,
    RequestTimeTooSkewed,
    NoSuchBucket,
    NoSuchCorsConfiguration,
    NoSuchKey,
    NoSuchUpload,
    BucketAlreadyExists,
    BucketNotEmpty,
    InvalidBucketName,
    InvalidRequest,
    InvalidRange,
    PreconditionFailed,
    InvalidPart,
    InvalidPartOrder,
    EntityTooSmall,
    QuotaExceeded,
    MalformedXml,
    BadDigest,
    NotImplemented,
    ServiceUnavailable,
    InternalError,
}

impl S3ErrorKind {
    const fn code(self) -> &'static str {
        match self {
            Self::AccessDenied => "AccessDenied",
            Self::InvalidAccessKeyId => "InvalidAccessKeyId",
            Self::SignatureDoesNotMatch => "SignatureDoesNotMatch",
            Self::AuthorizationHeaderMalformed => "AuthorizationHeaderMalformed",
            Self::RequestTimeTooSkewed => "RequestTimeTooSkewed",
            Self::NoSuchBucket => "NoSuchBucket",
            Self::NoSuchCorsConfiguration => "NoSuchCORSConfiguration",
            Self::NoSuchKey => "NoSuchKey",
            Self::NoSuchUpload => "NoSuchUpload",
            Self::BucketAlreadyExists => "BucketAlreadyExists",
            Self::BucketNotEmpty => "BucketNotEmpty",
            Self::InvalidBucketName => "InvalidBucketName",
            Self::InvalidRequest => "InvalidRequest",
            Self::InvalidRange => "InvalidRange",
            Self::PreconditionFailed => "PreconditionFailed",
            Self::InvalidPart => "InvalidPart",
            Self::InvalidPartOrder => "InvalidPartOrder",
            Self::EntityTooSmall => "EntityTooSmall",
            Self::QuotaExceeded => "QuotaExceeded",
            Self::MalformedXml => "MalformedXML",
            Self::BadDigest => "BadDigest",
            Self::NotImplemented => "NotImplemented",
            Self::ServiceUnavailable => "ServiceUnavailable",
            Self::InternalError => "InternalError",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::AccessDenied => "Access Denied",
            Self::InvalidAccessKeyId => "The AWS access key ID does not exist",
            Self::SignatureDoesNotMatch => "The request signature does not match",
            Self::AuthorizationHeaderMalformed => "The authorization header is malformed",
            Self::RequestTimeTooSkewed => {
                "The difference between request time and server time is too large"
            }
            Self::NoSuchBucket => "The specified bucket does not exist",
            Self::NoSuchCorsConfiguration => "The CORS configuration does not exist",
            Self::NoSuchKey => "The specified key does not exist",
            Self::NoSuchUpload => "The specified multipart upload does not exist",
            Self::BucketAlreadyExists => "The requested bucket name is not available",
            Self::BucketNotEmpty => "The bucket is not empty",
            Self::InvalidBucketName => "The specified bucket is not valid",
            Self::InvalidRequest => "Invalid Request",
            Self::InvalidRange => "The requested range is not satisfiable",
            Self::PreconditionFailed => "At least one precondition failed",
            Self::InvalidPart => "One or more specified parts could not be found",
            Self::InvalidPartOrder => "The list of parts was not in ascending order",
            Self::EntityTooSmall => "A non-final multipart part is too small",
            Self::QuotaExceeded => "The storage quota would be exceeded",
            Self::MalformedXml => "The XML document was not well formed",
            Self::BadDigest => "The Content-MD5 or checksum did not match the received data",
            Self::NotImplemented => "A requested operation is not implemented",
            Self::ServiceUnavailable => {
                "The cluster cannot currently satisfy this request; retry shortly"
            }
            Self::InternalError => "We encountered an internal error",
        }
    }

    const fn status(self) -> StatusCode {
        match self {
            Self::AccessDenied
            | Self::InvalidAccessKeyId
            | Self::SignatureDoesNotMatch
            | Self::RequestTimeTooSkewed => StatusCode::FORBIDDEN,
            Self::NoSuchBucket
            | Self::NoSuchCorsConfiguration
            | Self::NoSuchKey
            | Self::NoSuchUpload => StatusCode::NOT_FOUND,
            Self::BucketAlreadyExists => StatusCode::CONFLICT,
            Self::BucketNotEmpty => StatusCode::CONFLICT,
            Self::InvalidRange => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            Self::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            Self::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::AuthorizationHeaderMalformed
            | Self::InvalidBucketName
            | Self::InvalidRequest
            | Self::InvalidPart
            | Self::InvalidPartOrder
            | Self::EntityTooSmall
            | Self::QuotaExceeded
            | Self::MalformedXml
            | Self::BadDigest => StatusCode::BAD_REQUEST,
        }
    }
}

#[derive(Serialize)]
#[serde(rename = "Error")]
pub(crate) struct ErrorDocument<'a> {
    #[serde(rename = "Code")]
    pub(crate) code: &'a str,
    #[serde(rename = "Message")]
    pub(crate) message: &'a str,
    #[serde(rename = "Resource")]
    pub(crate) resource: &'a str,
    #[serde(rename = "RequestId")]
    pub(crate) request_id: &'a str,
}
