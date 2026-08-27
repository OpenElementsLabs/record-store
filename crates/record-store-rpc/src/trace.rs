//! W3C Trace Context propagation across internal RPC.
//!
//! Distributed operations are only debuggable if a single identifier follows a
//! request across every node it touches. The W3C header format is used directly
//! so the trace identifiers OES emits line up with any OpenTelemetry collector
//! without pulling an SDK into the data path.

use std::fmt::{self, Display, Formatter};

use tonic::metadata::{MetadataMap, MetadataValue};
use uuid::Uuid;

/// The W3C Trace Context header name.
pub const TRACEPARENT: &str = "traceparent";

/// A propagated trace context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    /// 16-byte trace identifier in lowercase hexadecimal.
    pub trace_id: String,
    /// 8-byte span identifier in lowercase hexadecimal.
    pub span_id: String,
    /// Sampling and other trace flags.
    pub flags: u8,
}

impl TraceContext {
    /// Creates a new root context.
    #[must_use]
    pub fn root() -> Self {
        Self {
            trace_id: hex::encode(Uuid::new_v4().as_bytes()),
            span_id: new_span_id(),
            flags: 1,
        }
    }

    /// Creates a child context that keeps the trace but starts a new span.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            trace_id: self.trace_id.clone(),
            span_id: new_span_id(),
            flags: self.flags,
        }
    }

    /// Parses a `traceparent` header value.
    ///
    /// A malformed value is ignored rather than failing the request: losing a
    /// trace is never a reason to reject internal traffic.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split('-');
        let version = parts.next()?;
        let trace_id = parts.next()?;
        let span_id = parts.next()?;
        let flags = parts.next()?;
        if parts.next().is_some() || version.len() != 2 {
            return None;
        }
        if trace_id.len() != 32
            || span_id.len() != 16
            || flags.len() != 2
            || !trace_id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !span_id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || trace_id.bytes().all(|byte| byte == b'0')
            || span_id.bytes().all(|byte| byte == b'0')
        {
            return None;
        }
        Some(Self {
            trace_id: trace_id.to_ascii_lowercase(),
            span_id: span_id.to_ascii_lowercase(),
            flags: u8::from_str_radix(flags, 16).ok()?,
        })
    }

    /// Extracts the context from request metadata, or starts a new trace.
    #[must_use]
    pub fn from_metadata(metadata: &MetadataMap) -> Self {
        metadata
            .get(TRACEPARENT)
            .and_then(|value| value.to_str().ok())
            .and_then(Self::parse)
            .unwrap_or_else(Self::root)
    }

    /// Writes the context into outgoing request metadata.
    pub fn write(&self, metadata: &mut MetadataMap) {
        if let Ok(value) = MetadataValue::try_from(self.to_string()) {
            metadata.insert(TRACEPARENT, value);
        }
    }
}

impl Display for TraceContext {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "00-{}-{}-{:02x}",
            self.trace_id, self.span_id, self.flags
        )
    }
}

fn new_span_id() -> String {
    hex::encode(&Uuid::new_v4().as_bytes()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_round_trip_through_the_header_format() {
        let context = TraceContext::root();
        let encoded = context.to_string();
        assert_eq!(TraceContext::parse(&encoded), Some(context.clone()));
        assert!(encoded.starts_with("00-"));
        assert_eq!(encoded.len(), 55);
    }

    #[test]
    fn a_child_keeps_the_trace_and_changes_the_span() {
        let parent = TraceContext::root();
        let child = parent.child();
        assert_eq!(parent.trace_id, child.trace_id);
        assert_ne!(parent.span_id, child.span_id);
    }

    #[test]
    fn malformed_headers_are_ignored_rather_than_fatal() {
        for value in [
            "",
            "00",
            "00-abc-def-01",
            "00-00000000000000000000000000000000-0000000000000001-01",
            "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01-extra",
            "00-0af7651916cd43dd8448eb211c8031zz-b7ad6b7169203331-01",
        ] {
            assert!(TraceContext::parse(value).is_none(), "accepted {value}");
        }
        assert!(
            TraceContext::parse("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
                .is_some()
        );
    }

    #[test]
    fn metadata_extraction_falls_back_to_a_fresh_trace() {
        let mut metadata = MetadataMap::new();
        let first = TraceContext::from_metadata(&metadata);
        first.write(&mut metadata);
        let second = TraceContext::from_metadata(&metadata);
        assert_eq!(first, second);
    }
}
