//! Body digests a client supplies: `Content-MD5` and `x-amz-checksum-*`.
//!
//! A client that sends a digest believes the server compared it with the body,
//! and current AWS SDKs send one by default. So every digest this server
//! accepts is verified, and every algorithm it cannot verify is refused with
//! `NotImplemented`; none is accepted and ignored (RSG-007).
//!
//! Streamed uploads (PutObject, UploadPart) are verified while the body
//! streams: on a mismatch the stream ends in an error, the storage layer aborts
//! the write before anything is committed, and the shared flag lets the handler
//! answer `BadDigest` rather than a generic failure. SHA-256 on a streamed
//! upload is left to the storage layer, which computes that digest anyway (see
//! `sigv4::request_checksum`). Buffered bodies -- the small XML documents of
//! configuration and completion requests -- are checked whole, SHA-256
//! included.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use md5::Md5;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::error::S3ErrorKind;

/// A digest of the request body that the client supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestDigest {
    Md5([u8; 16]),
    Crc32([u8; 4]),
    Crc32c([u8; 4]),
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}

impl RequestDigest {
    /// The response header echoing a verified `x-amz-checksum-*`, as S3 does.
    pub(crate) fn echo(&self) -> Option<(&'static str, String)> {
        match self {
            Self::Crc32(value) => Some(("x-amz-checksum-crc32", STANDARD.encode(value))),
            Self::Crc32c(value) => Some(("x-amz-checksum-crc32c", STANDARD.encode(value))),
            Self::Sha1(value) => Some(("x-amz-checksum-sha1", STANDARD.encode(value))),
            Self::Md5(_) | Self::Sha256(_) => None,
        }
    }
}

fn decode<const N: usize>(value: &axum::http::HeaderValue) -> Result<[u8; N], S3ErrorKind> {
    value
        .to_str()
        .ok()
        .and_then(|value| STANDARD.decode(value.trim()).ok())
        .and_then(|bytes| <[u8; N]>::try_from(bytes).ok())
        .ok_or(S3ErrorKind::InvalidRequest)
}

/// Every digest a request carries for its body.
///
/// At most one `x-amz-checksum-*` algorithm is allowed, as in S3; `Content-MD5`
/// may accompany it. A malformed value is an invalid request, and an algorithm
/// this server does not verify (CRC64NVME today) is not implemented.
/// `x-amz-checksum-type` and `-mode` describe a checksum rather than being one.
pub(crate) fn request_digests(headers: &HeaderMap) -> Result<Vec<RequestDigest>, S3ErrorKind> {
    let mut digests = Vec::new();
    let mut flexible = 0;
    if let Some(value) = headers.get("content-md5") {
        digests.push(RequestDigest::Md5(decode(value)?));
    }
    for (name, value) in headers {
        let Some(algorithm) = name.as_str().strip_prefix("x-amz-checksum-") else {
            continue;
        };
        if matches!(algorithm, "type" | "mode") {
            continue;
        }
        flexible += 1;
        digests.push(match algorithm {
            "crc32" => RequestDigest::Crc32(decode(value)?),
            "crc32c" => RequestDigest::Crc32c(decode(value)?),
            "sha1" => RequestDigest::Sha1(decode(value)?),
            "sha256" => RequestDigest::Sha256(decode(value)?),
            _ => return Err(S3ErrorKind::NotImplemented),
        });
    }
    if flexible > 1 {
        return Err(S3ErrorKind::InvalidRequest);
    }
    Ok(digests)
}

enum Hasher {
    Md5(Md5),
    Crc32(crc32fast::Hasher),
    Crc32c(u32),
    Sha1(Sha1),
    Sha256(Sha256),
}

impl Hasher {
    fn new(expected: &RequestDigest) -> Self {
        match expected {
            RequestDigest::Md5(_) => Self::Md5(Md5::new()),
            RequestDigest::Crc32(_) => Self::Crc32(crc32fast::Hasher::new()),
            RequestDigest::Crc32c(_) => Self::Crc32c(0),
            RequestDigest::Sha1(_) => Self::Sha1(Sha1::new()),
            RequestDigest::Sha256(_) => Self::Sha256(Sha256::new()),
        }
    }

    fn update(&mut self, chunk: &[u8]) {
        match self {
            Self::Md5(hasher) => hasher.update(chunk),
            Self::Crc32(hasher) => hasher.update(chunk),
            Self::Crc32c(state) => *state = crc32c::crc32c_append(*state, chunk),
            Self::Sha1(hasher) => hasher.update(chunk),
            Self::Sha256(hasher) => hasher.update(chunk),
        }
    }

    fn matches(self, expected: &RequestDigest) -> bool {
        match (self, expected) {
            (Self::Md5(hasher), RequestDigest::Md5(value)) => {
                hasher.finalize().as_slice() == value.as_slice()
            }
            (Self::Crc32(hasher), RequestDigest::Crc32(value)) => {
                hasher.finalize().to_be_bytes() == *value
            }
            (Self::Crc32c(state), RequestDigest::Crc32c(value)) => state.to_be_bytes() == *value,
            (Self::Sha1(hasher), RequestDigest::Sha1(value)) => {
                hasher.finalize().as_slice() == value.as_slice()
            }
            (Self::Sha256(hasher), RequestDigest::Sha256(value)) => {
                hasher.finalize().as_slice() == value.as_slice()
            }
            _ => false,
        }
    }
}

/// Checks a whole, buffered body against every digest the request carries.
pub(crate) fn verify_buffered(digests: &[RequestDigest], body: &[u8]) -> Result<(), S3ErrorKind> {
    for expected in digests {
        let mut hasher = Hasher::new(expected);
        hasher.update(body);
        if !hasher.matches(expected) {
            return Err(S3ErrorKind::BadDigest);
        }
    }
    Ok(())
}

/// Reads a small request body whole and checks it against every digest the
/// request carries.
pub(crate) async fn read_verified(
    body: axum::body::Body,
    limit: usize,
    headers: &HeaderMap,
) -> Result<Bytes, S3ErrorKind> {
    let digests = request_digests(headers)?;
    let bytes = axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| S3ErrorKind::InvalidRequest)?;
    verify_buffered(&digests, &bytes)?;
    Ok(bytes)
}

/// Set when a streamed body turned out not to match a digest it carried.
#[derive(Debug, Clone, Default)]
pub(crate) struct Mismatch(Arc<AtomicBool>);

impl Mismatch {
    pub(crate) fn happened(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Wraps a streamed request body so that it ends in an error, and records the
/// mismatch, when it does not match every one of `digests`. SHA-256 digests are
/// skipped here: the storage layer verifies those.
pub(crate) fn verify_streamed<S>(
    body: S,
    digests: Vec<RequestDigest>,
) -> (impl Stream<Item = io::Result<Bytes>> + Send, Mismatch)
where
    S: Stream<Item = io::Result<Bytes>> + Send + Unpin + 'static,
{
    let digests: Vec<_> = digests
        .into_iter()
        .filter(|d| !matches!(d, RequestDigest::Sha256(_)))
        .collect();
    let mismatch = Mismatch::default();
    let flag = mismatch.clone();
    let hashers: Vec<Hasher> = digests.iter().map(Hasher::new).collect();
    let verified = stream::unfold(
        (body, Some(hashers), digests, flag),
        |(mut body, mut hashers, digests, flag)| async move {
            let mut state = hashers.take()?;
            match body.next().await {
                Some(Ok(chunk)) => {
                    for hasher in &mut state {
                        hasher.update(&chunk);
                    }
                    Some((Ok(chunk), (body, Some(state), digests, flag)))
                }
                Some(Err(error)) => Some((Err(error), (body, None, digests, flag))),
                None => {
                    let intact = state
                        .into_iter()
                        .zip(&digests)
                        .all(|(hasher, expected)| hasher.matches(expected));
                    if intact {
                        None
                    } else {
                        flag.0.store(true, Ordering::SeqCst);
                        let error = io::Error::new(
                            io::ErrorKind::InvalidData,
                            "request digest does not match the body",
                        );
                        Some((Err(error), (body, None, digests, flag)))
                    }
                }
            }
        },
    );
    (verified, mismatch)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use futures_util::TryStreamExt;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).expect("header"));
        }
        map
    }

    async fn run(
        digests: Vec<RequestDigest>,
        chunks: &[&'static [u8]],
    ) -> (Result<Vec<Bytes>, io::Error>, bool) {
        let body = stream::iter(
            chunks
                .iter()
                .map(|chunk| Ok(Bytes::from_static(chunk)))
                .collect::<Vec<_>>(),
        );
        let (verified, mismatch) = verify_streamed(body, digests);
        (verified.try_collect::<Vec<_>>().await, mismatch.happened())
    }

    /// Known answers for "123456789", the standard check input.
    fn correct() -> Vec<RequestDigest> {
        vec![
            RequestDigest::Md5(Md5::digest(b"123456789").into()),
            RequestDigest::Crc32(0xCBF4_3926_u32.to_be_bytes()),
            RequestDigest::Crc32c(0xE306_9283_u32.to_be_bytes()),
            RequestDigest::Sha1(Sha1::digest(b"123456789").into()),
            RequestDigest::Sha256(Sha256::digest(b"123456789").into()),
        ]
    }

    #[tokio::test]
    async fn matching_bodies_pass_through_unchanged() {
        for digest in correct() {
            let (collected, mismatch) = run(vec![digest.clone()], &[b"1234", b"", b"56789"]).await;
            assert_eq!(collected.expect("passes").concat(), b"123456789");
            assert!(!mismatch);
            assert!(verify_buffered(&[digest], b"123456789").is_ok());
        }
    }

    #[tokio::test]
    async fn a_body_that_contradicts_a_digest_is_refused() {
        for digest in [
            RequestDigest::Md5([0; 16]),
            RequestDigest::Crc32([0; 4]),
            RequestDigest::Crc32c([0; 4]),
            RequestDigest::Sha1([0; 20]),
        ] {
            let (collected, mismatch) = run(vec![digest.clone()], &[b"checksum-bytes"]).await;
            assert!(collected.is_err());
            assert!(
                mismatch,
                "the handler must be able to tell this was a bad digest"
            );
            assert!(matches!(
                verify_buffered(&[digest], b"checksum-bytes"),
                Err(S3ErrorKind::BadDigest)
            ));
        }
        assert!(matches!(
            verify_buffered(&[RequestDigest::Sha256([0; 32])], b"x"),
            Err(S3ErrorKind::BadDigest)
        ));
        // Content-MD5 right, CRC32 wrong: every digest must hold.
        let mixed = vec![correct()[0].clone(), RequestDigest::Crc32([0; 4])];
        assert!(run(mixed, &[b"123456789"]).await.1);
    }

    #[test]
    fn digest_headers_are_read_verified_or_refused() {
        assert!(matches!(request_digests(&headers(&[])), Ok(digests) if digests.is_empty()));
        assert!(matches!(
            request_digests(&headers(&[("x-amz-checksum-crc32", "yZRlqg==")])).as_deref(),
            Ok([RequestDigest::Crc32([0xC9, 0x94, 0x65, 0xAA])])
        ));
        assert!(
            matches!(
                request_digests(&headers(&[
                    ("content-md5", "1B2M2Y8AsgTpgAmY7PhCfg=="),
                    ("x-amz-checksum-crc32", "AAAAAA==")
                ]))
                .as_deref(),
                Ok([_, _])
            ),
            "Content-MD5 may accompany one x-amz-checksum"
        );
        assert!(
            matches!(
                request_digests(&headers(&[("x-amz-checksum-crc64nvme", "AAAAAAAAAAA=")])),
                Err(S3ErrorKind::NotImplemented)
            ),
            "an algorithm that is not verified is refused, never ignored"
        );
        assert!(matches!(
            request_digests(&headers(&[("x-amz-checksum-crc32", "not base64!")])),
            Err(S3ErrorKind::InvalidRequest)
        ));
        assert!(matches!(
            request_digests(&headers(&[("x-amz-checksum-crc32", "AAAAAAAA")])),
            Err(S3ErrorKind::InvalidRequest)
        ));
        assert!(matches!(
            request_digests(&headers(&[("content-md5", "AA==")])),
            Err(S3ErrorKind::InvalidRequest)
        ));
        assert!(
            matches!(
                request_digests(&headers(&[
                    ("x-amz-checksum-crc32", "AAAAAA=="),
                    ("x-amz-checksum-sha1", "AA==")
                ])),
                Err(S3ErrorKind::InvalidRequest)
            ),
            "two checksum algorithms on one request are refused"
        );
        assert!(
            matches!(request_digests(&headers(&[("x-amz-checksum-type", "FULL_OBJECT")])), Ok(digests) if digests.is_empty())
        );
    }
}
