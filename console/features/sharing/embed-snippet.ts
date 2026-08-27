/**
 * Embed snippets, generated from the object's validated media type.
 *
 * A snippet is only produced for a shape Record Store actually serves: an `<img>` for an
 * image, a media element for audio and video, and nothing at all for a type that
 * has no safe element. Offering markup that will not work — or worse, markup
 * that invites a browser to interpret stored bytes as a document — would be a
 * generator that produces bugs rather than convenience.
 */

import { previewKind } from '@/lib/preview-kind';

export type EmbedSnippet = {
  readonly language: 'html';
  readonly code: string;
};

/**
 * Builds the markup for one embed URL.
 *
 * `alt` is left empty rather than filled with the file name: the page author
 * knows what the image means in their context and Record Store does not, and a wrong
 * alternative text is worse for a screen-reader user than an empty one they can
 * replace. The comment in the snippet says so.
 */
export function embedSnippet(url: string, contentType: string): EmbedSnippet | null {
  const kind = previewKind(contentType);
  switch (kind) {
    case 'image':
      return {
        language: 'html',
        code: `<!-- Describe the image for screen readers in alt. -->\n<img\n  src="${url}"\n  alt=""\n/>`,
      };
    case 'video':
      return {
        language: 'html',
        code: `<video\n  controls\n  preload="metadata"\n  src="${url}">\n</video>`,
      };
    case 'audio':
      return {
        language: 'html',
        code: `<audio\n  controls\n  preload="metadata"\n  src="${url}">\n</audio>`,
      };
    default:
      return null;
  }
}

/** Whether a direct element embed exists for this media type at all. */
export function hasEmbedSnippet(contentType: string): boolean {
  return embedSnippet('https://example.invalid', contentType) !== null;
}
