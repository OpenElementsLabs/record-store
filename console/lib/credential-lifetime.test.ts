import { describe, expect, it } from 'vitest';

import { TEMPORARY_LIFETIMES, secondsRemaining } from './credential-lifetime';

const now = new Date('2026-08-23T12:00:00Z');

describe('secondsRemaining', () => {
  it('returns null for a credential that does not expire', () => {
    expect(secondsRemaining(null, now)).toBeNull();
  });

  it('counts down to the expiry', () => {
    expect(secondsRemaining('2026-08-23T13:00:00Z', now)).toBe(3_600);
  });

  it('clamps an elapsed credential to zero rather than counting backwards', () => {
    expect(secondsRemaining('2026-08-23T11:00:00Z', now)).toBe(0);
  });

  it('treats an unparseable timestamp as no expiry rather than as expired', () => {
    // Reporting "expired" for a value we failed to read would be worse than
    // saying nothing about it.
    expect(secondsRemaining('not a date', now)).toBeNull();
  });
});

describe('TEMPORARY_LIFETIMES', () => {
  it('stays inside the range the backend accepts', () => {
    for (const option of TEMPORARY_LIFETIMES) {
      expect(option.seconds).toBeGreaterThanOrEqual(60);
      expect(option.seconds).toBeLessThanOrEqual(86_400);
    }
  });
});
