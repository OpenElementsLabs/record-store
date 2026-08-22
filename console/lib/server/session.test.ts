import { afterEach, describe, expect, it } from 'vitest';

import { sessionCookieOptions } from './session';

describe('session cookie policy', () => {
  afterEach(() => {
    delete process.env.OES_CONSOLE_SECURE_COOKIES;
  });

  it('is HTTP-only, strict same-site, path-scoped, and time-bounded', () => {
    process.env.OES_CONSOLE_SECURE_COOKIES = 'true';
    expect(sessionCookieOptions(8 * 60 * 60)).toEqual({
      httpOnly: true,
      sameSite: 'strict',
      secure: true,
      path: '/',
      maxAge: 28_800,
    });
  });

  it('supports explicit insecure loopback development and immediate clearing', () => {
    process.env.OES_CONSOLE_SECURE_COOKIES = 'false';
    expect(sessionCookieOptions(0)).toMatchObject({ secure: false, maxAge: 0 });
  });
});
