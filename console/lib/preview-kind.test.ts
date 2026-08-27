import { describe, expect, it } from 'vitest';

import { isElementEmbeddable, isPreviewable, previewKind, previewKindLabel } from './preview-kind';

describe('previewKind', () => {
  it('classifies the formats Record Store is prepared to render', () => {
    expect(previewKind('image/png')).toBe('image');
    expect(previewKind('image/jpeg')).toBe('image');
    expect(previewKind('video/mp4')).toBe('video');
    expect(previewKind('audio/mpeg')).toBe('audio');
    expect(previewKind('application/pdf')).toBe('pdf');
    expect(previewKind('text/plain')).toBe('text');
    expect(previewKind('application/json')).toBe('json');
  });

  it('ignores parameters and case, which are presentation details of one type', () => {
    expect(previewKind('IMAGE/PNG')).toBe('image');
    expect(previewKind('text/plain; charset=UTF-8')).toBe('text');
    expect(previewKind('  application/json  ')).toBe('json');
  });

  it('names active content as refused rather than merely unknown', () => {
    // The distinction matters to the reader: "we will not show this" is a
    // different message from "we do not know what this is".
    for (const contentType of [
      'text/html',
      'application/xhtml+xml',
      'image/svg+xml',
      'application/xml',
      'text/xml',
      'text/javascript',
      'application/javascript',
      'application/ecmascript',
      'application/xslt+xml',
    ]) {
      expect(previewKind(contentType), contentType).toBe('unsafe_inline');
    }
  });

  it('refuses anything it was not told about rather than guessing', () => {
    expect(previewKind('application/octet-stream')).toBe('unsupported');
    expect(previewKind('application/x-newly-invented')).toBe('unsupported');
    expect(previewKind(null)).toBe('unsupported');
    expect(previewKind(undefined)).toBe('unsupported');
    expect(previewKind('')).toBe('unsupported');
  });

  it('never treats a refused classification as previewable or embeddable', () => {
    expect(isPreviewable('unsafe_inline')).toBe(false);
    expect(isPreviewable('unsupported')).toBe(false);
    expect(isElementEmbeddable('unsafe_inline')).toBe(false);
    expect(isElementEmbeddable('pdf')).toBe(false);
    expect(isElementEmbeddable('text')).toBe(false);
    expect(isElementEmbeddable('image')).toBe(true);
  });

  it('gives every classification a human label', () => {
    for (const kind of [
      'image',
      'video',
      'audio',
      'pdf',
      'text',
      'json',
      'unsafe_inline',
      'unsupported',
    ] as const) {
      expect(previewKindLabel(kind).length).toBeGreaterThan(0);
    }
  });
});
