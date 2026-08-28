use std::collections::BTreeMap;

use axum::http::{HeaderMap, Method, Uri, header::HeaderName};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, NaiveDateTime, Utc};
use hmac::{Hmac, Mac};
use percent_encoding::percent_decode_str;
use record_store_auth::{Principal, SigningSecret};
use record_store_core::Checksum;
use sha2::Sha256;
use uuid::Uuid;

use crate::error::S3ErrorKind;
use crate::handlers::listing::decode_query_component;

pub(crate) struct ParsedPresign {
    pub(crate) algorithm: String,
    pub(crate) access_key: String,
    pub(crate) scope_date: String,
    pub(crate) region: String,
    pub(crate) service: String,
    pub(crate) terminal: String,
    pub(crate) date: String,
    pub(crate) expires: i64,
    pub(crate) signed_headers: Vec<String>,
    pub(crate) signature: String,
}

impl ParsedPresign {
    pub(crate) fn parse(query: &str) -> Result<Self, S3ErrorKind> {
        let mut values = BTreeMap::new();
        for item in query.split('&').filter(|item| !item.is_empty()) {
            let (name, value) = item.split_once('=').unwrap_or((item, ""));
            let name = decode_query_component(name)?;
            let value = decode_query_component(value)?;
            if name.starts_with("X-Amz-") && values.insert(name, value).is_some() {
                return Err(S3ErrorKind::AuthorizationHeaderMalformed);
            }
        }
        let algorithm = values
            .remove("X-Amz-Algorithm")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        let credential = values
            .remove("X-Amz-Credential")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        let date = values
            .remove("X-Amz-Date")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        let expires = values
            .remove("X-Amz-Expires")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
            .parse::<i64>()
            .map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
        if !(1..=604_800).contains(&expires) {
            return Err(S3ErrorKind::InvalidRequest);
        }
        let signed_headers = values
            .remove("X-Amz-SignedHeaders")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
            .split(';')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let signature = values
            .remove("X-Amz-Signature")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        if values
            .remove("X-Amz-Content-Sha256")
            .is_some_and(|value| value != "UNSIGNED-PAYLOAD")
        {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        if !values.is_empty()
            || signed_headers.is_empty()
            || !signed_headers.windows(2).all(|pair| pair[0] < pair[1])
            || !signed_headers.iter().any(|name| name == "host")
            || signed_headers
                .iter()
                .any(|name| name.is_empty() || name.bytes().any(|byte| byte.is_ascii_uppercase()))
            || signature.len() != 64
            || !signature.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let mut scope = credential.split('/');
        let access_key = scope.next().filter(|value| !value.is_empty());
        let scope_date = scope.next();
        let region = scope.next();
        let service = scope.next();
        let terminal = scope.next();
        if scope.next().is_some() {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        Ok(Self {
            algorithm,
            access_key: access_key
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            scope_date: scope_date
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            region: region
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            service: service
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            terminal: terminal
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            date,
            expires,
            signed_headers,
            signature: signature.to_ascii_lowercase(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct S3RequestId(pub(crate) String);

impl S3RequestId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4().simple().to_string())
    }
}

pub(crate) struct Authenticated {
    pub(crate) principal: Principal,
    pub(crate) payload: PayloadHash,
}

#[derive(Clone)]
pub(crate) enum PayloadHash {
    Sha256(Checksum),
    Unsigned,
}

impl PayloadHash {
    pub(crate) fn canonical_value(&self) -> String {
        match self {
            Self::Sha256(checksum) => hex::encode(checksum.digest()),
            Self::Unsigned => "UNSIGNED-PAYLOAD".into(),
        }
    }

    pub(crate) fn expected_checksum(&self) -> Option<Checksum> {
        match self {
            Self::Sha256(checksum) => Some(checksum.clone()),
            Self::Unsigned => None,
        }
    }
}

pub(crate) fn parse_payload_hash(headers: &HeaderMap) -> Result<PayloadHash, S3ErrorKind> {
    let value = headers
        .get("x-amz-content-sha256")
        .and_then(|value| value.to_str().ok())
        .ok_or(S3ErrorKind::InvalidRequest)?;
    if value == "UNSIGNED-PAYLOAD" {
        return Ok(PayloadHash::Unsigned);
    }
    let digest = hex::decode(value).map_err(|_| S3ErrorKind::InvalidRequest)?;
    let digest: [u8; 32] = digest.try_into().map_err(|_| S3ErrorKind::InvalidRequest)?;
    Ok(PayloadHash::Sha256(Checksum::sha256(digest)))
}

pub(crate) fn request_checksum(
    headers: &HeaderMap,
    payload_hash: &PayloadHash,
) -> Result<Option<Checksum>, S3ErrorKind> {
    let signed_payload = payload_hash.expected_checksum();
    let Some(encoded) = headers.get("x-amz-checksum-sha256") else {
        return Ok(signed_payload);
    };
    let encoded = encoded.to_str().map_err(|_| S3ErrorKind::InvalidRequest)?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| S3ErrorKind::InvalidRequest)?;
    let digest: [u8; 32] = decoded
        .try_into()
        .map_err(|_| S3ErrorKind::InvalidRequest)?;
    let supplied = Checksum::sha256(digest);
    if signed_payload
        .as_ref()
        .is_some_and(|signed| signed != &supplied)
    {
        return Err(S3ErrorKind::BadDigest);
    }
    Ok(Some(supplied))
}

pub(crate) struct ParsedAuthorization {
    pub(crate) access_key: String,
    pub(crate) scope_date: String,
    pub(crate) region: String,
    pub(crate) service: String,
    pub(crate) terminal: String,
    pub(crate) signed_headers: Vec<String>,
    pub(crate) signature: String,
}

impl ParsedAuthorization {
    pub(crate) fn parse(value: &str) -> Result<Self, S3ErrorKind> {
        let parameters = value
            .strip_prefix("AWS4-HMAC-SHA256 ")
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        let mut credential = None;
        let mut signed_headers = None;
        let mut signature = None;
        for parameter in parameters.split(',') {
            let (name, value) = parameter
                .trim()
                .split_once('=')
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
            match name {
                "Credential" if credential.is_none() => credential = Some(value),
                "SignedHeaders" if signed_headers.is_none() => signed_headers = Some(value),
                "Signature" if signature.is_none() => signature = Some(value),
                _ => return Err(S3ErrorKind::AuthorizationHeaderMalformed),
            }
        }
        let mut credential = credential
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
            .split('/');
        let access_key = credential.next().filter(|value| !value.is_empty());
        let scope_date = credential.next();
        let region = credential.next();
        let service = credential.next();
        let terminal = credential.next();
        if credential.next().is_some() {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let signed_headers = signed_headers
            .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
            .split(';')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if signed_headers.is_empty()
            || !signed_headers.windows(2).all(|pair| pair[0] < pair[1])
            || signed_headers
                .iter()
                .any(|name| name.is_empty() || name.bytes().any(|byte| byte.is_ascii_uppercase()))
            || !signed_headers.iter().any(|name| name == "host")
        {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let signature = signature.ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?;
        if signature.len() != 64 || !signature.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        Ok(Self {
            access_key: access_key
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            scope_date: scope_date
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            region: region
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            service: service
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            terminal: terminal
                .ok_or(S3ErrorKind::AuthorizationHeaderMalformed)?
                .to_owned(),
            signed_headers,
            signature: signature.to_ascii_lowercase(),
        })
    }
}

pub(crate) fn parse_request_time(headers: &HeaderMap) -> Result<DateTime<Utc>, S3ErrorKind> {
    let value = headers
        .get("x-amz-date")
        .and_then(|value| value.to_str().ok())
        .ok_or(S3ErrorKind::AccessDenied)?;
    parse_amz_date(value)
}

pub(crate) fn parse_amz_date(value: &str) -> Result<DateTime<Utc>, S3ErrorKind> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
    Ok(naive.and_utc())
}

pub(crate) fn canonical_request(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    signed_headers: &[String],
    payload_hash: String,
) -> Result<String, S3ErrorKind> {
    let canonical_uri = aws_encode(&percent_decode_str(uri.path()).collect::<Vec<_>>(), false);
    let canonical_query = canonical_query(uri.query().unwrap_or_default());
    let mut canonical_headers = String::new();
    for name in signed_headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
        let values = headers.get_all(header_name);
        if values.iter().next().is_none() {
            return Err(S3ErrorKind::AuthorizationHeaderMalformed);
        }
        let mut joined = Vec::new();
        for value in values {
            let value = value
                .to_str()
                .map_err(|_| S3ErrorKind::AuthorizationHeaderMalformed)?;
            joined.push(collapse_whitespace(value));
        }
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&joined.join(","));
        canonical_headers.push('\n');
    }
    Ok(format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.as_str(),
        canonical_uri,
        canonical_query,
        canonical_headers,
        signed_headers.join(";"),
        payload_hash
    ))
}

pub(crate) fn canonical_query(query: &str) -> String {
    let mut pairs = query
        .split('&')
        .filter(|item| !item.is_empty())
        .map(|item| {
            let (name, value) = item.split_once('=').unwrap_or((item, ""));
            (
                aws_encode(&percent_decode_str(name).collect::<Vec<_>>(), true),
                aws_encode(&percent_decode_str(value).collect::<Vec<_>>(), true),
            )
        })
        .filter(|(name, _)| name != "X-Amz-Signature")
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn aws_encode(value: &[u8], encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (!encode_slash && *byte == b'/')
        {
            encoded.push(char::from(*byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

pub(crate) fn collapse_whitespace(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn calculate_signature(
    secret: &SigningSecret,
    date: &str,
    region: &str,
    service: &str,
    string_to_sign: &[u8],
) -> Result<Vec<u8>, S3ErrorKind> {
    let mut initial = b"AWS4".to_vec();
    initial.extend_from_slice(secret.expose());
    let date_key = hmac_sha256(&initial, date.as_bytes())?;
    let region_key = hmac_sha256(&date_key, region.as_bytes())?;
    let service_key = hmac_sha256(&region_key, service.as_bytes())?;
    let signing_key = hmac_sha256(&service_key, b"aws4_request")?;
    hmac_sha256(&signing_key, string_to_sign)
}

pub(crate) fn hmac_sha256(key: &[u8], value: &[u8]) -> Result<Vec<u8>, S3ErrorKind> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| S3ErrorKind::InternalError)?;
    mac.update(value);
    Ok(mac.finalize().into_bytes().to_vec())
}
