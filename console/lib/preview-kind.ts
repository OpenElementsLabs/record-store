import type { PreviewKindName } from '@/types/api';

/**
 * How an object's bytes may be presented in a browser.
 *
 * This mirrors the classification the management API applies, and mirrors it
 * deliberately rather than deriving it locally: the browser copy decides which
 * viewer to mount, and the server copy decides whether any bytes are served
 * inline at all. The server is the one that matters — a console that got this
 * wrong would show a broken viewer, whereas a server that got it wrong would
 * hand a browser somebody else's markup to execute.
 */
export type PreviewKind = PreviewKindName;

const CLASSIFICATION: Readonly<Record<string, PreviewKind>> = {
  'image/jpeg': 'image',
  'image/png': 'image',
  'image/webp': 'image',
  'image/gif': 'image',
  'video/mp4': 'video',
  'video/webm': 'video',
  'audio/mpeg': 'audio',
  'audio/ogg': 'audio',
  'audio/wav': 'audio',
  'audio/x-wav': 'audio',
  'audio/webm': 'audio',
  'application/pdf': 'pdf',
  'text/plain': 'text',
  'text/markdown': 'text',
  'text/csv': 'text',
  'application/json': 'json',
  // Named rather than left to the fallback so the refusal is a decision on the
  // record. Every one of these can carry script or fetch external resources.
  'text/html': 'unsafe_inline',
  'application/xhtml+xml': 'unsafe_inline',
  'image/svg+xml': 'unsafe_inline',
  'application/xml': 'unsafe_inline',
  'text/xml': 'unsafe_inline',
  'text/javascript': 'unsafe_inline',
  'application/javascript': 'unsafe_inline',
  'application/ecmascript': 'unsafe_inline',
  'application/x-shockwave-flash': 'unsafe_inline',
  'application/xslt+xml': 'unsafe_inline',
};

/**
 * Classifies trusted server metadata, never a filename extension.
 *
 * Parameters such as `; charset=utf-8` are presentation details of the same
 * media type and are ignored. Anything unlisted is refused rather than guessed.
 */
export function previewKind(contentType: string | null | undefined): PreviewKind {
  if (!contentType) return 'unsupported';
  const essence = contentType.split(';')[0]?.trim().toLowerCase() ?? '';
  return CLASSIFICATION[essence] ?? 'unsupported';
}

/** Whether OES will render this classification inline. */
export function isPreviewable(kind: PreviewKind): boolean {
  return (
    kind === 'image' ||
    kind === 'video' ||
    kind === 'audio' ||
    kind === 'pdf' ||
    kind === 'text' ||
    kind === 'json'
  );
}

/** Whether a plain `<img>`, `<video>`, or `<audio>` embed is meaningful. */
export function isElementEmbeddable(kind: PreviewKind): boolean {
  return kind === 'image' || kind === 'video' || kind === 'audio';
}

/** A short human label for a classification, for empty and refusal states. */
export function previewKindLabel(kind: PreviewKind): string {
  switch (kind) {
    case 'image':
      return 'Image';
    case 'video':
      return 'Video';
    case 'audio':
      return 'Audio';
    case 'pdf':
      return 'PDF document';
    case 'text':
      return 'Text';
    case 'json':
      return 'JSON';
    case 'unsafe_inline':
      return 'Active content';
    case 'unsupported':
      return 'Unsupported';
  }
}
