import { describe, expect, it } from 'vitest';

import {
  formatBytes,
  formatBytesOf,
  formatCount,
  formatDate,
  formatDateTime,
  formatDuration,
  formatPercent,
  formatRatio,
  formatRelativeTime,
  keyBasename,
  keySegments,
  shortenIdentifier,
} from './format';

describe('formatBytes', () => {
  it('renders small values as plain bytes', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1)).toBe('1 B');
    expect(formatBytes(999)).toBe('999 B');
  });

  it('uses decimal units consistently', () => {
    expect(formatBytes(1_000)).toBe('1.00 kB');
    expect(formatBytes(1_500_000)).toBe('1.50 MB');
    expect(formatBytes(4_000_000_000_000)).toBe('4.00 TB');
    expect(formatBytes(1_820_000_000_000)).toBe('1.82 TB');
  });

  it('reduces precision as the value grows so columns stay narrow', () => {
    expect(formatBytes(12_000)).toBe('12.0 kB');
    expect(formatBytes(120_000)).toBe('120 kB');
  });

  it('honours an explicit precision', () => {
    expect(formatBytes(1_234_567, { precision: 3 })).toBe('1.235 MB');
  });

  it('reports unusable input rather than guessing', () => {
    expect(formatBytes(Number.NaN)).toBe('—');
    expect(formatBytes(-1)).toBe('—');
  });

  it('renders a used-of-total pair', () => {
    expect(formatBytesOf(1_820_000_000_000, 4_000_000_000_000)).toBe('1.82 TB of 4.00 TB');
    expect(formatBytesOf(500, 0)).toBe('500 B');
  });
});

describe('numbers', () => {
  it('groups counts', () => {
    expect(formatCount(1234)).toBe(new Intl.NumberFormat().format(1234));
    expect(formatCount(Number.POSITIVE_INFINITY)).toBe('—');
  });

  it('renders percentages and guards division by zero', () => {
    expect(formatPercent(42.4)).toBe('42%');
    expect(formatRatio(1, 4)).toBe('25%');
    expect(formatRatio(1, 0)).toBe('—');
  });
});

describe('timestamps', () => {
  it('formats absolute values without manual string parsing', () => {
    const iso = '2026-08-22T10:30:00Z';
    expect(formatDateTime(iso)).not.toBe('—');
    expect(formatDate(iso)).not.toBe('—');
  });

  it('degrades gracefully for missing or malformed values', () => {
    expect(formatDateTime(null)).toBe('—');
    expect(formatDateTime(undefined)).toBe('—');
    expect(formatDateTime('not a date')).toBe('—');
    expect(formatRelativeTime('nope')).toBe('—');
  });

  it('formats relative values against a fixed reference', () => {
    const now = new Date('2026-08-22T12:00:00Z');
    expect(formatRelativeTime('2026-08-22T11:57:00Z', now)).toContain('3');
    // `numeric: 'auto'` deliberately renders the nearest day as a word.
    expect(formatRelativeTime('2026-08-21T12:00:00Z', now)).toBe('yesterday');
    expect(formatRelativeTime('2026-08-19T12:00:00Z', now)).toContain('3');
  });

  it('formats durations in operator-friendly units', () => {
    expect(formatDuration(30)).toBe('30s');
    expect(formatDuration(90)).toBe('1m');
    expect(formatDuration(3 * 3600 + 25 * 60)).toBe('3h 25m');
    expect(formatDuration(50 * 3600)).toBe('2d 2h');
    expect(formatDuration(-5)).toBe('—');
  });
});

describe('object keys', () => {
  it('treats prefixes as logical segments, not directories', () => {
    expect(keySegments('documents/finance/')).toEqual(['documents', 'finance']);
    expect(keySegments('')).toEqual([]);
    expect(keySegments('///')).toEqual([]);
  });

  it('derives a display name from a key', () => {
    expect(keyBasename('documents/finance/report.pdf')).toBe('report.pdf');
    expect(keyBasename('report.pdf')).toBe('report.pdf');
  });

  it('shortens long identifiers from both ends', () => {
    const id = '0123456789abcdef0123456789abcdef';
    expect(shortenIdentifier(id)).toBe('01234567…89abcdef');
    expect(shortenIdentifier('short')).toBe('short');
  });
});
