import { describe, expect, it } from 'vitest';

import { exactUnit, splitBytes, toBytes } from './byte-size';

describe('byte-size', () => {
  it('picks the largest unit that divides a value exactly', () => {
    expect(exactUnit(1000)).toBe('kB');
    expect(exactUnit(5 * 1000 ** 3)).toBe('GB');
    // 1500 is 1.5 kB, which is not an exact kB, so it stays in bytes.
    expect(exactUnit(1500)).toBe('B');
    // 1.5 GB has no exact GB representation, so it stays in MB.
    expect(exactUnit(1_500_000_000)).toBe('MB');
    expect(exactUnit(1)).toBe('B');
    expect(exactUnit(0)).toBe('B');
  });

  it('round-trips a stored limit without drift', () => {
    for (const bytes of [1, 999, 1000, 1_500_000_000, 5 * 1000 ** 3, 42 * 1000 ** 4]) {
      const { value, unit } = splitBytes(bytes);
      expect(toBytes(String(value), unit)).toBe(bytes);
    }
  });

  it('multiplies whole amounts exactly', () => {
    expect(toBytes('5', 'GB')).toBe(5_000_000_000);
    expect(toBytes(' 10 ', 'TB')).toBe(10_000_000_000_000);
    expect(toBytes('0', 'GB')).toBe(0);
  });

  it('rejects input that could not be stored exactly', () => {
    // A fractional amount would have to round to reach a byte count.
    expect(toBytes('1.5', 'GB')).toBeNull();
    expect(toBytes('-1', 'GB')).toBeNull();
    expect(toBytes('', 'GB')).toBeNull();
    expect(toBytes('abc', 'GB')).toBeNull();
    expect(toBytes('99999999', 'PB')).toBeNull();
  });
});
