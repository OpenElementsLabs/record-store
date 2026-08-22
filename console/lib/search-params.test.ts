import { describe, expect, it } from 'vitest';

import {
  mergeSearch,
  readEnum,
  readInt,
  readOptionalString,
  readString,
  readTimestamp,
} from './search-params';

function params(query: string): URLSearchParams {
  return new URLSearchParams(query);
}

describe('readString', () => {
  it('trims values and falls back when absent', () => {
    expect(readString(params('prefix=%20docs%2F%20'), 'prefix')).toBe('docs/');
    expect(readString(params(''), 'prefix', 'fallback')).toBe('fallback');
  });

  it('rejects oversized values instead of forwarding them', () => {
    const long = 'a'.repeat(2000);
    expect(readString(params(`prefix=${long}`), 'prefix', 'safe')).toBe('safe');
  });
});

describe('readOptionalString', () => {
  it('returns null for absent or empty values', () => {
    expect(readOptionalString(params(''), 'q')).toBeNull();
    expect(readOptionalString(params('q='), 'q')).toBeNull();
    expect(readOptionalString(params('q=hello'), 'q')).toBe('hello');
  });
});

describe('readInt', () => {
  it('clamps into range and ignores nonsense', () => {
    expect(readInt(params('limit=50'), 'limit', 25, 1, 100)).toBe(50);
    expect(readInt(params('limit=9999'), 'limit', 25, 1, 100)).toBe(100);
    expect(readInt(params('limit=0'), 'limit', 25, 1, 100)).toBe(1);
    expect(readInt(params('limit=abc'), 'limit', 25, 1, 100)).toBe(25);
    expect(readInt(params(''), 'limit', 25, 1, 100)).toBe(25);
  });
});

describe('readEnum', () => {
  const allowed = ['success', 'denied', 'failure'] as const;

  it('accepts known values and rejects anything else', () => {
    expect(readEnum(params('result=denied'), 'result', allowed)).toBe('denied');
    expect(readEnum(params('result=nonsense'), 'result', allowed)).toBeNull();
    expect(readEnum(params(''), 'result', allowed)).toBeNull();
  });
});

describe('readTimestamp', () => {
  it('normalises valid timestamps and drops invalid ones', () => {
    expect(readTimestamp(params('since=2026-08-22T10:00:00Z'), 'since')).toBe(
      '2026-08-22T10:00:00.000Z',
    );
    expect(readTimestamp(params('since=yesterday'), 'since')).toBeNull();
    expect(readTimestamp(params(''), 'since')).toBeNull();
  });
});

describe('mergeSearch', () => {
  it('keeps existing values and applies updates', () => {
    const result = mergeSearch(params('prefix=docs%2F&limit=50'), { limit: 100 });
    const parsed = new URLSearchParams(result.slice(1));
    expect(parsed.get('prefix')).toBe('docs/');
    expect(parsed.get('limit')).toBe('100');
  });

  it('removes cleared values rather than leaving them empty', () => {
    const result = mergeSearch(params('prefix=docs%2F&cursor=abc'), { cursor: null });
    expect(result).not.toContain('cursor');
    expect(result).toContain('prefix=docs');
  });

  it('returns an empty string when nothing remains', () => {
    expect(mergeSearch(params('cursor=abc'), { cursor: null })).toBe('');
  });
});
