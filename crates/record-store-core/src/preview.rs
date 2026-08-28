use serde::{Deserialize, Serialize};

/// How an object's bytes may be presented to a browser.
///
/// This is a property of the object's validated media type, not of a filename:
/// an extension is a caller-supplied hint that carries no authority, whereas
/// this classification is what preview, share, and embed delivery all agree to
/// honour. Two of the variants deliberately describe *refusal* rather than a
/// viewer, because "we will not render this" is a decision that has to be
/// representable rather than an absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewKind {
    /// A raster image a browser decodes without scripting.
    Image,
    /// A container browsers play natively through `<video>`.
    Video,
    /// A container browsers play natively through `<audio>`.
    Audio,
    /// A PDF document, rendered only by an isolated viewer.
    Pdf,
    /// Text rendered as escaped characters, never as markup.
    Text,
    /// JSON rendered as escaped, optionally reformatted, text.
    Json,
    /// A media type Record Store declines to render inline because the format can carry
    /// active content. Such objects are download-only.
    UnsafeInline,
    /// A media type with no safe inline representation.
    Unsupported,
}

impl PreviewKind {
    /// Returns the classification for a stored media type.
    ///
    /// Parameters such as `; charset=utf-8` are ignored, and matching is
    /// case-insensitive, because both are presentation details of the same
    /// media type. Anything not explicitly listed is refused rather than
    /// guessed: an unknown type is exactly the case where guessing is unsafe.
    #[must_use]
    pub fn classify(content_type: Option<&str>) -> Self {
        let Some(content_type) = content_type else {
            return Self::Unsupported;
        };
        let essence = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match essence.as_str() {
            "image/jpeg" | "image/png" | "image/webp" | "image/gif" => Self::Image,
            "video/mp4" | "video/webm" => Self::Video,
            "audio/mpeg" | "audio/ogg" | "audio/wav" | "audio/x-wav" | "audio/webm" => Self::Audio,
            "application/pdf" => Self::Pdf,
            "text/plain" | "text/markdown" | "text/csv" => Self::Text,
            "application/json" => Self::Json,
            // Formats that can carry script or external references. They are
            // named explicitly so the refusal is a documented decision rather
            // than the fallback branch.
            "text/html"
            | "application/xhtml+xml"
            | "image/svg+xml"
            | "application/xml"
            | "text/xml"
            | "text/javascript"
            | "application/javascript"
            | "application/ecmascript"
            | "application/x-shockwave-flash"
            | "application/xslt+xml" => Self::UnsafeInline,
            _ => Self::Unsupported,
        }
    }

    /// Returns the canonical media type Record Store will actually emit for this type.
    ///
    /// Returning the normalised essence rather than the stored string means a
    /// caller-supplied `Content-Type` cannot smuggle parameters into a response
    /// header, and cannot differ in case from what classification agreed to.
    #[must_use]
    pub fn canonical_content_type(content_type: &str) -> Option<&'static str> {
        let essence = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        Some(match essence.as_str() {
            "image/jpeg" => "image/jpeg",
            "image/png" => "image/png",
            "image/webp" => "image/webp",
            "image/gif" => "image/gif",
            "video/mp4" => "video/mp4",
            "video/webm" => "video/webm",
            "audio/mpeg" => "audio/mpeg",
            "audio/ogg" => "audio/ogg",
            "audio/wav" | "audio/x-wav" => "audio/wav",
            "audio/webm" => "audio/webm",
            "application/pdf" => "application/pdf",
            // Text is always emitted as UTF-8 plain text. Markdown and CSV are
            // deliberately downgraded: a browser must never be invited to treat
            // stored text as a document format with its own fetch behaviour.
            "text/plain" | "text/markdown" | "text/csv" => "text/plain; charset=utf-8",
            "application/json" => "application/json; charset=utf-8",
            _ => return None,
        })
    }

    /// Whether this classification may be served with `Content-Disposition: inline`.
    #[must_use]
    pub const fn allows_inline(self) -> bool {
        matches!(
            self,
            Self::Image | Self::Video | Self::Audio | Self::Pdf | Self::Text | Self::Json
        )
    }

    /// Whether a direct `<img>`, `<video>`, or `<audio>` embed is meaningful.
    ///
    /// PDFs and text are previewable but are not element-embeddable without a
    /// framed viewer, which this milestone deliberately does not provide.
    #[must_use]
    pub const fn allows_element_embed(self) -> bool {
        matches!(self, Self::Image | Self::Video | Self::Audio)
    }

    /// Whether the format supports meaningful byte-range seeking in a browser.
    #[must_use]
    pub const fn seekable(self) -> bool {
        matches!(self, Self::Video | Self::Audio | Self::Pdf)
    }

    /// A stable, low-cardinality label safe to use as a metric dimension.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Pdf => "pdf",
            Self::Text => "text",
            Self::Json => "json",
            Self::UnsafeInline => "unsafe_inline",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Bytes Record Store needs from the front of an object to corroborate its media type.
///
/// Small and fixed: enough to reach an MP4 `ftyp` box or a WebM EBML header,
/// and never enough for the check itself to become a large read.
pub const CONTENT_SIGNATURE_PROBE_BYTES: usize = 64;

/// Whether the leading bytes of an object corroborate its declared media type.
///
/// A stored `Content-Type` is caller-supplied and therefore untrusted: an
/// attacker can upload HTML and label it `image/png`. Browsers that sniff, or
/// human readers who trust the label, then draw the wrong conclusion. This
/// check does not attempt full format validation — it asks only whether the
/// container signature is consistent with what the object claims to be, which
/// is exactly the mismatch that turns a stored file into a delivery vector.
///
/// Types with no reliable leading signature (plain text, JSON, WAV variants
/// already covered by RIFF) are accepted when they contain no byte sequence
/// that a sniffing browser would treat as markup.
#[must_use]
pub fn content_signature_matches(content_type: &str, prefix: &[u8]) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "image/jpeg" => prefix.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/png" => prefix.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/gif" => prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a"),
        "image/webp" => riff_form(prefix, b"WEBP"),
        "application/pdf" => prefix.starts_with(b"%PDF-"),
        "video/mp4" => mp4_brand(prefix),
        "video/webm" | "audio/webm" => prefix.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
        "audio/ogg" => prefix.starts_with(b"OggS"),
        "audio/wav" | "audio/x-wav" => riff_form(prefix, b"WAVE"),
        "audio/mpeg" => {
            prefix.starts_with(b"ID3")
                // A bare MPEG audio frame begins with eleven set sync bits.
                || (prefix.len() >= 2 && prefix[0] == 0xFF && (prefix[1] & 0xE0) == 0xE0)
        }
        // Textual types have no signature. What matters is that they do not
        // begin with something a sniffing browser would promote to markup.
        "text/plain" | "text/markdown" | "text/csv" | "application/json" => {
            !looks_like_markup(prefix)
        }
        // Anything else is not served inline, so there is nothing to corroborate.
        _ => true,
    }
}

/// Whether the leading bytes would tempt a sniffing browser to render markup.
pub(crate) fn looks_like_markup(prefix: &[u8]) -> bool {
    let start = prefix.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(prefix);
    let trimmed: Vec<u8> = start
        .iter()
        .copied()
        .skip_while(u8::is_ascii_whitespace)
        .take(16)
        .map(|byte| byte.to_ascii_lowercase())
        .collect();
    [
        b"<!doctype".as_slice(),
        b"<html".as_slice(),
        b"<head".as_slice(),
        b"<body".as_slice(),
        b"<script".as_slice(),
        b"<svg".as_slice(),
        b"<?xml".as_slice(),
    ]
    .iter()
    .any(|marker| trimmed.starts_with(marker))
}

pub(crate) fn riff_form(prefix: &[u8], form: &[u8; 4]) -> bool {
    prefix.len() >= 12 && prefix.starts_with(b"RIFF") && &prefix[8..12] == form.as_slice()
}

/// Whether the prefix carries an ISO base-media `ftyp` box with a video brand.
pub(crate) fn mp4_brand(prefix: &[u8]) -> bool {
    prefix.len() >= 12 && &prefix[4..8] == b"ftyp"
}
