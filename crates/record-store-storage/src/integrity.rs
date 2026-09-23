//! Read-path integrity checks shared by object, multipart, and replica reads.
//!
//! Two different checks live here and they establish different things, so the
//! distinction is drawn explicitly rather than left to the reader.
//!
//! [`verify_physical_length`] runs **before any byte is released**. It compares
//! what the filesystem says the payload occupies against what committed
//! metadata says it should, which is what catches a truncated or extended file
//! — the ordinary shape of storage corruption — while the caller can still be
//! handed a clean error instead of a short body.
//!
//! [`verifying_stream`] runs **while bytes are streaming**. It recomputes the
//! payload digest and fails the stream before its final chunk is delivered when
//! the digest disagrees with the one recorded at commit. That is detection, not
//! prevention: bytes have already left by the time the mismatch is known, so
//! the guarantee is that a corrupt read *fails* rather than that no corrupt byte
//! is ever observed. Verifying before release would mean reading every payload
//! twice, which for object storage is not a trade worth making.
//!
//! A ranged read gets the length check but no digest check, because a digest
//! over part of a payload cannot be compared with a digest over all of it.

use bytes::Bytes;
use futures_util::{TryStreamExt, stream};
use record_store_core::Checksum;
use sha2::{Digest, Sha256};

use crate::{DownloadStream, StorageError};

/// Refuses a payload whose physical size cannot hold the committed logical size.
///
/// `physical` is what the file occupies on disk and `expected` is what the
/// payload format says a payload of `logical` bytes must occupy. They are
/// computed by the caller because only it knows the format's framing overhead.
pub(crate) const fn verify_physical_length(
    physical: u64,
    expected: u64,
) -> Result<(), StorageError> {
    if physical == expected {
        Ok(())
    } else {
        Err(StorageError::IntegrityMismatch)
    }
}

/// Wraps a download stream so a digest mismatch fails the read instead of the
/// client silently receiving corrupt bytes.
///
/// Every chunk is released one step late: the stream always holds the most
/// recent chunk back, and the last one is released only after the digest over
/// the whole payload has matched. On a mismatch the last chunk is never sent,
/// so a client reading against `Content-Length` sees a short body -- a failed
/// read -- rather than a complete response of the wrong bytes (RSG-001). The
/// cost is one chunk of latency on each read.
pub(crate) fn verifying_stream(body: DownloadStream, expected: Checksum) -> DownloadStream {
    struct State {
        body: DownloadStream,
        hasher: Sha256,
        expected: Checksum,
        held: Option<Bytes>,
        finished: bool,
    }
    let state = State {
        body,
        hasher: Sha256::new(),
        expected,
        held: None,
        finished: false,
    };
    Box::pin(stream::try_unfold(state, |mut state| async move {
        if state.finished {
            return Ok(None);
        }
        loop {
            match state.body.try_next().await? {
                Some(chunk) if chunk.is_empty() => {}
                Some(chunk) => {
                    state.hasher.update(&chunk);
                    if let Some(previous) = state.held.replace(chunk) {
                        return Ok(Some((previous, state)));
                    }
                }
                None => {
                    state.finished = true;
                    let digest = std::mem::take(&mut state.hasher).finalize();
                    if Checksum::sha256(digest.into()) != state.expected {
                        return Err(StorageError::IntegrityMismatch);
                    }
                    return Ok(state.held.take().map(|last| (last, state)));
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    fn stream_of(chunks: Vec<&'static [u8]>) -> DownloadStream {
        Box::pin(stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok(Bytes::from_static(chunk))),
        ))
    }

    fn digest(bytes: &[u8]) -> Checksum {
        Checksum::sha256(Sha256::digest(bytes).into())
    }

    async fn drain(mut stream: DownloadStream) -> (Vec<u8>, Option<StorageError>) {
        let mut received = Vec::new();
        loop {
            match stream.next().await {
                Some(Ok(chunk)) => received.extend_from_slice(&chunk),
                Some(Err(error)) => return (received, Some(error)),
                None => return (received, None),
            }
        }
    }

    #[tokio::test]
    async fn an_intact_payload_is_released_whole_and_in_order() {
        for chunks in [
            vec![],
            vec![&b"one chunk"[..]],
            vec![&b"a"[..], b"", b"bc", b"def"],
        ] {
            let whole: Vec<u8> = chunks.concat();
            let (received, error) =
                drain(verifying_stream(stream_of(chunks), digest(&whole))).await;
            assert!(error.is_none());
            assert_eq!(received, whole);
        }
    }

    /// The final chunk is what makes a response look complete. A mismatch must
    /// withhold it, so the client receives fewer bytes than the object's length
    /// -- for a one-chunk object, none at all.
    #[tokio::test]
    async fn a_mismatch_withholds_the_final_chunk() {
        let (received, error) =
            drain(verifying_stream(stream_of(vec![b"only"]), digest(b"else"))).await;
        assert!(matches!(error, Some(StorageError::IntegrityMismatch)));
        assert!(
            received.is_empty(),
            "no byte of a one-chunk corrupt payload is released"
        );

        let (received, error) = drain(verifying_stream(
            stream_of(vec![b"first", b"second", b"third"]),
            digest(b"other"),
        ))
        .await;
        assert!(matches!(error, Some(StorageError::IntegrityMismatch)));
        assert_eq!(
            received, b"firstsecond",
            "everything but the final chunk, then the error"
        );
    }
}
