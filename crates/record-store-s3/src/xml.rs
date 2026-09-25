use chrono::{DateTime, Utc};
use record_store_core::{
    CoreError, CorsConfiguration, CorsMethod, CorsPattern, CorsRule, DefaultRetention,
    ObjectLockConfiguration, Retention, RetentionMode, RetentionPeriod,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(rename = "VersioningConfiguration")]
pub(crate) struct VersioningConfigurationDocument {
    #[serde(rename = "Status")]
    pub(crate) status: Option<String>,
}

#[derive(Serialize)]
#[serde(rename = "VersioningConfiguration")]
pub(crate) struct VersioningConfigurationResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Status", skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(rename = "CORSConfiguration")]
pub(crate) struct CorsConfigurationDocument {
    #[serde(rename = "CORSRule", default)]
    pub(crate) rules: Vec<CorsRuleDocument>,
}

#[derive(Deserialize)]
pub(crate) struct CorsRuleDocument {
    #[serde(rename = "ID")]
    pub(crate) id: Option<String>,
    #[serde(rename = "AllowedOrigin", default)]
    pub(crate) allowed_origins: Vec<String>,
    #[serde(rename = "AllowedMethod", default)]
    pub(crate) allowed_methods: Vec<String>,
    #[serde(rename = "AllowedHeader", default)]
    pub(crate) allowed_headers: Vec<String>,
    #[serde(rename = "ExposeHeader", default)]
    pub(crate) expose_headers: Vec<String>,
    #[serde(rename = "MaxAgeSeconds")]
    pub(crate) max_age_seconds: Option<u32>,
}

impl TryFrom<CorsConfigurationDocument> for CorsConfiguration {
    type Error = record_store_core::CoreError;

    fn try_from(document: CorsConfigurationDocument) -> Result<Self, Self::Error> {
        let rules = document
            .rules
            .into_iter()
            .map(|document| {
                let rule = CorsRule {
                    id: document.id,
                    allowed_origins: document
                        .allowed_origins
                        .iter()
                        .map(|origin| CorsPattern::origin(origin))
                        .collect::<Result<Vec<_>, _>>()?,
                    allowed_methods: document
                        .allowed_methods
                        .iter()
                        .map(|method| CorsMethod::parse(method))
                        .collect::<Result<Vec<_>, _>>()?,
                    allowed_headers: document
                        .allowed_headers
                        .iter()
                        .map(|header| CorsPattern::header(header))
                        .collect::<Result<Vec<_>, _>>()?,
                    expose_headers: document.expose_headers,
                    max_age_seconds: document.max_age_seconds,
                };
                rule.validate()?;
                Ok(rule)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let configuration = Self { rules };
        configuration.validate()?;
        Ok(configuration)
    }
}

#[derive(Serialize)]
#[serde(rename = "CORSConfiguration")]
pub(crate) struct CorsConfigurationResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "CORSRule")]
    pub(crate) rules: Vec<CorsRuleResult<'a>>,
}

#[derive(Serialize)]
pub(crate) struct CorsRuleResult<'a> {
    #[serde(rename = "ID", skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<&'a str>,
    #[serde(rename = "AllowedOrigin")]
    pub(crate) allowed_origins: Vec<&'a str>,
    #[serde(rename = "AllowedMethod")]
    pub(crate) allowed_methods: Vec<&'static str>,
    #[serde(rename = "AllowedHeader", skip_serializing_if = "Vec::is_empty")]
    pub(crate) allowed_headers: Vec<&'a str>,
    #[serde(rename = "ExposeHeader", skip_serializing_if = "Vec::is_empty")]
    pub(crate) expose_headers: Vec<&'a str>,
    #[serde(rename = "MaxAgeSeconds", skip_serializing_if = "Option::is_none")]
    pub(crate) max_age_seconds: Option<u32>,
}

impl<'a> From<&'a CorsConfiguration> for CorsConfigurationResult<'a> {
    fn from(configuration: &'a CorsConfiguration) -> Self {
        Self {
            xmlns: "http://s3.amazonaws.com/doc/2006-03-01/",
            rules: configuration
                .rules
                .iter()
                .map(|rule| CorsRuleResult {
                    id: rule.id.as_deref(),
                    allowed_origins: rule
                        .allowed_origins
                        .iter()
                        .map(CorsPattern::as_str)
                        .collect(),
                    allowed_methods: rule
                        .allowed_methods
                        .iter()
                        .map(|method| method.as_str())
                        .collect(),
                    allowed_headers: rule
                        .allowed_headers
                        .iter()
                        .map(CorsPattern::as_str)
                        .collect(),
                    expose_headers: rule.expose_headers.iter().map(String::as_str).collect(),
                    max_age_seconds: rule.max_age_seconds,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename = "InitiateMultipartUploadResult")]
pub(crate) struct InitiateMultipartUploadResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Bucket")]
    pub(crate) bucket: String,
    #[serde(rename = "Key")]
    pub(crate) key: String,
    #[serde(rename = "UploadId")]
    pub(crate) upload_id: String,
}

#[derive(Deserialize)]
#[serde(rename = "CompleteMultipartUpload")]
pub(crate) struct CompleteMultipartUploadDocument {
    #[serde(rename = "Part", default)]
    pub(crate) parts: Vec<CompletedPartDocument>,
}

#[derive(Deserialize)]
pub(crate) struct CompletedPartDocument {
    #[serde(rename = "PartNumber")]
    pub(crate) part_number: u16,
    #[serde(rename = "ETag")]
    pub(crate) etag: String,
}

#[derive(Serialize)]
#[serde(rename = "CompleteMultipartUploadResult")]
pub(crate) struct CompleteMultipartUploadResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Location")]
    pub(crate) location: String,
    #[serde(rename = "Bucket")]
    pub(crate) bucket: String,
    #[serde(rename = "Key")]
    pub(crate) key: String,
    #[serde(rename = "ETag")]
    pub(crate) etag: String,
    #[serde(rename = "VersionId")]
    pub(crate) version_id: String,
}

#[derive(Serialize)]
#[serde(rename = "CopyObjectResult")]
pub(crate) struct CopyObjectResult {
    #[serde(rename = "LastModified")]
    pub(crate) last_modified: String,
    #[serde(rename = "ETag")]
    pub(crate) etag: String,
    #[serde(rename = "VersionId")]
    pub(crate) version_id: String,
}

#[derive(Serialize)]
#[serde(rename = "ListPartsResult")]
pub(crate) struct ListPartsResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Bucket")]
    pub(crate) bucket: String,
    #[serde(rename = "Key")]
    pub(crate) key: String,
    #[serde(rename = "UploadId")]
    pub(crate) upload_id: String,
    #[serde(rename = "PartNumberMarker")]
    pub(crate) part_number_marker: u16,
    #[serde(
        rename = "NextPartNumberMarker",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) next_part_number_marker: Option<u16>,
    #[serde(rename = "MaxParts")]
    pub(crate) max_parts: usize,
    #[serde(rename = "IsTruncated")]
    pub(crate) is_truncated: bool,
    #[serde(rename = "Part", default)]
    pub(crate) parts: Vec<ListedPart>,
}

#[derive(Serialize)]
pub(crate) struct ListedPart {
    #[serde(rename = "PartNumber")]
    pub(crate) part_number: u16,
    #[serde(rename = "LastModified")]
    pub(crate) last_modified: String,
    #[serde(rename = "ETag")]
    pub(crate) etag: String,
    #[serde(rename = "Size")]
    pub(crate) size: u64,
}

#[derive(Serialize)]
#[serde(rename = "ListMultipartUploadsResult")]
pub(crate) struct ListMultipartUploadsResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Bucket")]
    pub(crate) bucket: String,
    #[serde(rename = "Prefix")]
    pub(crate) prefix: String,
    #[serde(rename = "KeyMarker", skip_serializing_if = "Option::is_none")]
    pub(crate) key_marker: Option<String>,
    #[serde(rename = "UploadIdMarker", skip_serializing_if = "Option::is_none")]
    pub(crate) upload_id_marker: Option<String>,
    #[serde(rename = "NextKeyMarker", skip_serializing_if = "Option::is_none")]
    pub(crate) next_key_marker: Option<String>,
    #[serde(rename = "NextUploadIdMarker", skip_serializing_if = "Option::is_none")]
    pub(crate) next_upload_id_marker: Option<String>,
    #[serde(rename = "MaxUploads")]
    pub(crate) max_uploads: usize,
    #[serde(rename = "IsTruncated")]
    pub(crate) is_truncated: bool,
    #[serde(rename = "Upload", default)]
    pub(crate) uploads: Vec<ListedUpload>,
}

#[derive(Serialize)]
pub(crate) struct ListedUpload {
    #[serde(rename = "Key")]
    pub(crate) key: String,
    #[serde(rename = "UploadId")]
    pub(crate) upload_id: String,
    #[serde(rename = "Initiated")]
    pub(crate) initiated: String,
}

#[derive(Serialize)]
#[serde(rename = "ListVersionsResult")]
pub(crate) struct ListVersionsResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Name")]
    pub(crate) name: String,
    #[serde(rename = "Prefix")]
    pub(crate) prefix: String,
    #[serde(rename = "KeyMarker", skip_serializing_if = "Option::is_none")]
    pub(crate) key_marker: Option<String>,
    #[serde(rename = "VersionIdMarker", skip_serializing_if = "Option::is_none")]
    pub(crate) version_id_marker: Option<String>,
    #[serde(rename = "NextKeyMarker", skip_serializing_if = "Option::is_none")]
    pub(crate) next_key_marker: Option<String>,
    #[serde(
        rename = "NextVersionIdMarker",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) next_version_id_marker: Option<String>,
    #[serde(rename = "MaxKeys")]
    pub(crate) max_keys: usize,
    #[serde(rename = "IsTruncated")]
    pub(crate) is_truncated: bool,
    #[serde(rename = "Version", default)]
    pub(crate) versions: Vec<VersionEntry>,
    #[serde(rename = "DeleteMarker", default)]
    pub(crate) delete_markers: Vec<DeleteMarkerEntry>,
}

#[derive(Serialize)]
pub(crate) struct VersionEntry {
    #[serde(rename = "Key")]
    pub(crate) key: String,
    #[serde(rename = "VersionId")]
    pub(crate) version_id: String,
    #[serde(rename = "IsLatest")]
    pub(crate) is_latest: bool,
    #[serde(rename = "LastModified")]
    pub(crate) last_modified: String,
    #[serde(rename = "ETag")]
    pub(crate) etag: String,
    #[serde(rename = "Size")]
    pub(crate) size: u64,
    #[serde(rename = "StorageClass")]
    pub(crate) storage_class: &'static str,
}

#[derive(Serialize)]
pub(crate) struct DeleteMarkerEntry {
    #[serde(rename = "Key")]
    pub(crate) key: String,
    #[serde(rename = "VersionId")]
    pub(crate) version_id: String,
    #[serde(rename = "IsLatest")]
    pub(crate) is_latest: bool,
    #[serde(rename = "LastModified")]
    pub(crate) last_modified: String,
}

#[derive(Serialize)]
#[serde(rename = "ListAllMyBucketsResult")]
pub(crate) struct ListBucketsResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Owner")]
    pub(crate) owner: Owner<'a>,
    #[serde(rename = "Buckets")]
    pub(crate) buckets: Buckets,
}

#[derive(Serialize)]
pub(crate) struct Owner<'a> {
    #[serde(rename = "ID")]
    pub(crate) id: &'a str,
    #[serde(rename = "DisplayName")]
    pub(crate) display_name: &'a str,
}

#[derive(Serialize)]
pub(crate) struct Buckets {
    #[serde(rename = "Bucket")]
    pub(crate) bucket: Vec<BucketEntry>,
}

#[derive(Serialize)]
pub(crate) struct BucketEntry {
    #[serde(rename = "Name")]
    pub(crate) name: String,
    #[serde(rename = "CreationDate")]
    pub(crate) creation_date: String,
}

#[derive(Serialize)]
#[serde(rename = "ListBucketResult")]
pub(crate) struct ListBucketResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Name")]
    pub(crate) name: String,
    #[serde(rename = "Prefix")]
    pub(crate) prefix: String,
    #[serde(rename = "Delimiter", skip_serializing_if = "Option::is_none")]
    pub(crate) delimiter: Option<String>,
    #[serde(rename = "KeyCount")]
    pub(crate) key_count: usize,
    #[serde(rename = "MaxKeys")]
    pub(crate) max_keys: usize,
    #[serde(rename = "IsTruncated")]
    pub(crate) is_truncated: bool,
    #[serde(rename = "ContinuationToken", skip_serializing_if = "Option::is_none")]
    pub(crate) continuation_token: Option<String>,
    #[serde(rename = "StartAfter", skip_serializing_if = "Option::is_none")]
    pub(crate) start_after: Option<String>,
    #[serde(
        rename = "NextContinuationToken",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) next_continuation_token: Option<String>,
    #[serde(rename = "Contents")]
    pub(crate) contents: Vec<ObjectEntry<'a>>,
    #[serde(rename = "CommonPrefixes")]
    pub(crate) common_prefixes: Vec<CommonPrefix>,
}

#[derive(Serialize)]
pub(crate) struct ObjectEntry<'a> {
    #[serde(rename = "Key")]
    pub(crate) key: String,
    #[serde(rename = "LastModified")]
    pub(crate) last_modified: String,
    #[serde(rename = "ETag")]
    pub(crate) etag: String,
    #[serde(rename = "Size")]
    pub(crate) size: u64,
    #[serde(rename = "StorageClass")]
    pub(crate) storage_class: &'a str,
}

#[derive(Serialize)]
pub(crate) struct CommonPrefix {
    #[serde(rename = "Prefix")]
    pub(crate) prefix: String,
}

/// Bucket Object Lock configuration as S3 sends it.
///
/// `Days` and `Years` are mutually exclusive in S3, and a document naming both
/// or neither is a malformed request rather than a value with a default.
#[derive(Deserialize)]
#[serde(rename = "ObjectLockConfiguration")]
pub(crate) struct ObjectLockConfigurationDocument {
    #[serde(rename = "ObjectLockEnabled")]
    pub(crate) object_lock_enabled: Option<String>,
    #[serde(rename = "Rule")]
    pub(crate) rule: Option<ObjectLockRuleDocument>,
}

#[derive(Deserialize)]
pub(crate) struct ObjectLockRuleDocument {
    #[serde(rename = "DefaultRetention")]
    pub(crate) default_retention: Option<DefaultRetentionDocument>,
}

#[derive(Deserialize)]
pub(crate) struct DefaultRetentionDocument {
    #[serde(rename = "Mode")]
    pub(crate) mode: Option<String>,
    #[serde(rename = "Days")]
    pub(crate) days: Option<u16>,
    #[serde(rename = "Years")]
    pub(crate) years: Option<u16>,
}

impl TryFrom<ObjectLockConfigurationDocument> for ObjectLockConfiguration {
    type Error = CoreError;

    fn try_from(document: ObjectLockConfigurationDocument) -> Result<Self, Self::Error> {
        if document
            .object_lock_enabled
            .as_deref()
            .is_some_and(|value| value != "Enabled")
        {
            return Err(CoreError::InvalidObjectLock(
                "ObjectLockEnabled must be Enabled".into(),
            ));
        }
        let default_retention = document
            .rule
            .and_then(|rule| rule.default_retention)
            .map(|retention| {
                let mode = RetentionMode::parse(retention.mode.as_deref().unwrap_or_default())?;
                let period = match (retention.days, retention.years) {
                    (Some(days), None) => RetentionPeriod::days(days)?,
                    (None, Some(years)) => RetentionPeriod::years(years)?,
                    _ => {
                        return Err(CoreError::InvalidObjectLock(
                            "a default retention names exactly one of Days or Years".into(),
                        ));
                    }
                };
                Ok(DefaultRetention { mode, period })
            })
            .transpose()?;
        Self { default_retention }.validate()
    }
}

#[derive(Serialize)]
#[serde(rename = "ObjectLockConfiguration")]
pub(crate) struct ObjectLockConfigurationResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "ObjectLockEnabled")]
    pub(crate) object_lock_enabled: &'a str,
    #[serde(rename = "Rule", skip_serializing_if = "Option::is_none")]
    pub(crate) rule: Option<ObjectLockRuleResult>,
}

#[derive(Serialize)]
pub(crate) struct ObjectLockRuleResult {
    #[serde(rename = "DefaultRetention")]
    pub(crate) default_retention: DefaultRetentionResult,
}

#[derive(Serialize)]
pub(crate) struct DefaultRetentionResult {
    #[serde(rename = "Mode")]
    pub(crate) mode: &'static str,
    #[serde(rename = "Days", skip_serializing_if = "Option::is_none")]
    pub(crate) days: Option<u16>,
    #[serde(rename = "Years", skip_serializing_if = "Option::is_none")]
    pub(crate) years: Option<u16>,
}

impl<'a> ObjectLockConfigurationResult<'a> {
    pub(crate) fn new(xmlns: &'a str, configuration: &ObjectLockConfiguration) -> Self {
        Self {
            xmlns,
            object_lock_enabled: "Enabled",
            rule: configuration
                .default_retention
                .map(|default| ObjectLockRuleResult {
                    default_retention: match default.period {
                        RetentionPeriod::Days(days) => DefaultRetentionResult {
                            mode: default.mode.as_str(),
                            days: Some(days),
                            years: None,
                        },
                        RetentionPeriod::Years(years) => DefaultRetentionResult {
                            mode: default.mode.as_str(),
                            days: None,
                            years: Some(years),
                        },
                    },
                }),
        }
    }
}

/// Per-version retention as S3 sends it.
///
/// An empty document removes the retention, which is how S3 expresses a
/// governance release, so both fields are optional and their absence is
/// meaningful rather than an error.
#[derive(Deserialize)]
#[serde(rename = "Retention")]
pub(crate) struct RetentionDocument {
    #[serde(rename = "Mode")]
    pub(crate) mode: Option<String>,
    #[serde(rename = "RetainUntilDate")]
    pub(crate) retain_until_date: Option<String>,
}

impl TryFrom<RetentionDocument> for Option<Retention> {
    type Error = CoreError;

    fn try_from(document: RetentionDocument) -> Result<Self, Self::Error> {
        match (document.mode, document.retain_until_date) {
            (None, None) => Ok(None),
            (Some(mode), Some(date)) => Ok(Some(Retention {
                mode: RetentionMode::parse(&mode)?,
                retain_until: parse_retain_until(&date)?,
            })),
            _ => Err(CoreError::InvalidObjectLock(
                "a retention names both Mode and RetainUntilDate, or neither".into(),
            )),
        }
    }
}

#[derive(Serialize)]
#[serde(rename = "Retention")]
pub(crate) struct RetentionResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Mode")]
    pub(crate) mode: &'static str,
    #[serde(rename = "RetainUntilDate")]
    pub(crate) retain_until_date: String,
}

/// Per-version legal hold as S3 sends it.
#[derive(Deserialize)]
#[serde(rename = "LegalHold")]
pub(crate) struct LegalHoldDocument {
    #[serde(rename = "Status")]
    pub(crate) status: Option<String>,
}

#[derive(Serialize)]
#[serde(rename = "LegalHold")]
pub(crate) struct LegalHoldResult<'a> {
    #[serde(rename = "@xmlns")]
    pub(crate) xmlns: &'a str,
    #[serde(rename = "Status")]
    pub(crate) status: &'static str,
}

/// Renders the legal-hold status S3 uses on the wire.
pub(crate) const fn legal_hold_status(legal_hold: bool) -> &'static str {
    if legal_hold { "ON" } else { "OFF" }
}

/// Parses the legal-hold status S3 sends.
pub(crate) fn parse_legal_hold(value: &str) -> Result<bool, CoreError> {
    match value {
        "ON" => Ok(true),
        "OFF" => Ok(false),
        other => Err(CoreError::InvalidObjectLock(format!(
            "legal hold status must be ON or OFF, not {other}"
        ))),
    }
}

/// Parses an Object Lock date.
///
/// S3 sends ISO 8601 here rather than the RFC 1123 form used by HTTP date
/// headers, and clients vary in how many fractional digits they include, so
/// this accepts any RFC 3339 instant and normalizes it to UTC.
pub(crate) fn parse_retain_until(value: &str) -> Result<DateTime<Utc>, CoreError> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|_| {
            CoreError::InvalidObjectLock(format!(
                "retain-until date is not an RFC 3339 instant: {value}"
            ))
        })
}

/// Renders an Object Lock date the way S3 does, with millisecond precision.
pub(crate) fn format_retain_until(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
