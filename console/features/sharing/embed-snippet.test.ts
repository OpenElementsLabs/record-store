import { describe, expect, it } from 'vitest';

import { embedSnippet, hasEmbedSnippet } from './embed-snippet';

const URL = 'https://record-store.example.com/e/AbCdEf';

describe('embedSnippet', () => {
  it('produces the element that actually renders each media type', () => {
    expect(embedSnippet(URL, 'image/png')?.code).toContain('<img');
    expect(embedSnippet(URL, 'video/mp4')?.code).toContain('<video');
    expect(embedSnippet(URL, 'audio/mpeg')?.code).toContain('<audio');
  });

  it('puts the URL in the snippet and nothing else that identifies the object', () => {
    const snippet = embedSnippet(URL, 'image/png');
    expect(snippet?.code).toContain(URL);
    expect(snippet?.code).not.toContain('bucket');
  });

  it('leaves alt text empty for the page author to write', () => {
    // A wrong alternative text is worse for a screen-reader user than an empty
    // one they can replace, and Record Store does not know what the image means in
    // somebody else's page.
    const snippet = embedSnippet(URL, 'image/png');
    expect(snippet?.code).toContain('alt=""');
    expect(snippet?.code).toContain('alt');
  });

  it('generates nothing for a format with no safe element', () => {
    // Producing markup for these would either not work or would invite a
    // browser to interpret stored bytes as a document.
    for (const contentType of [
      'text/html',
      'image/svg+xml',
      'application/pdf',
      'application/json',
      'text/plain',
      'application/octet-stream',
    ]) {
      expect(embedSnippet(URL, contentType), contentType).toBeNull();
      expect(hasEmbedSnippet(contentType), contentType).toBe(false);
    }
  });

  it('never emits a script tag or an inline handler', () => {
    for (const contentType of ['image/png', 'video/mp4', 'audio/mpeg']) {
      const code = embedSnippet(URL, contentType)?.code ?? '';
      expect(code).not.toMatch(/<script/i);
      expect(code).not.toMatch(/\son[a-z]+=/i);
    }
  });
});
