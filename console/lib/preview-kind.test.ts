import { describe, expect, it } from 'vitest';

import { previewKind } from './preview-kind';

describe('previewKind', () => {
  it('classifies safe formats from content type', () => {
    expect(previewKind('image/png')).toBe('image');
    expect(previewKind('application/pdf')).toBe('pdf');
    expect(previewKind('application/json')).toBe('json');
  });

  it('does not allow active content inline', () => {
    expect(previewKind('text/html')).toBe('unsafe-inline');
    expect(previewKind('image/svg+xml')).toBe('unsafe-inline');
    expect(previewKind('application/octet-stream')).toBe('unsupported');
    expect(previewKind(null)).toBe('unsupported');
  });
});
