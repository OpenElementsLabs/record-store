export type PreviewKind =
  'image' | 'video' | 'audio' | 'pdf' | 'text' | 'json' | 'unsupported' | 'unsafe-inline';

/** Classifies trusted server metadata, never a filename extension. */
export function previewKind(contentType: string | null): PreviewKind {
  switch (contentType?.toLowerCase()) {
    case 'image/jpeg':
    case 'image/png':
    case 'image/webp':
    case 'image/gif':
      return 'image';
    case 'video/mp4':
    case 'video/webm':
      return 'video';
    case 'audio/mpeg':
    case 'audio/ogg':
    case 'audio/wav':
    case 'audio/webm':
      return 'audio';
    case 'application/pdf':
      return 'pdf';
    case 'text/plain':
    case 'text/markdown':
      return 'text';
    case 'application/json':
      return 'json';
    case 'text/html':
    case 'image/svg+xml':
    case 'application/xml':
    case 'text/javascript':
    case 'application/javascript':
      return 'unsafe-inline';
    default:
      return 'unsupported';
  }
}
