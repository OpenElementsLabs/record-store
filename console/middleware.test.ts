import { describe, expect, it } from 'vitest';

import { config, middleware } from './middleware';

/** Compiles the matcher the way the framework does, so the test asserts routing. */
function matches(path: string): boolean {
  return config.matcher.some((pattern) => new RegExp(`^${pattern}$`).test(path));
}

function policyFor(path: string): string {
  const request = new Request(`http://console.test${path}`);
  const response = middleware(request as never);
  return response.headers.get('content-security-policy') ?? '';
}

describe('middleware', () => {
  it('applies a nonce policy to document routes', () => {
    expect(matches('/')).toBe(true);
    expect(matches('/buckets')).toBe(true);
    expect(matches('/buckets/uploads/objects/report.pdf')).toBe(true);

    const policy = policyFor('/buckets');
    expect(policy).toMatch(/script-src 'self' 'nonce-[A-Za-z0-9+/=]+' 'strict-dynamic'/);
    // The browser reaches OES only through this server's own origin.
    expect(policy).toContain("connect-src 'self'");
    expect(policy).toContain("frame-ancestors 'none'");
  });

  it('mints a distinct nonce per request', () => {
    expect(policyFor('/buckets')).not.toBe(policyFor('/buckets'));
  });

  it('leaves API routes out of the middleware entirely', () => {
    // This middleware re-issues the request, which caps how large a body may
    // be. An object upload streaming through `/api/oes` would be truncated at
    // that cap — silently, because the truncated request still succeeds.
    expect(matches('/api/oes/v1/buckets/uploads/object/archive/backup.tar')).toBe(false);
    expect(matches('/api/auth/login')).toBe(false);
    expect(matches('/api/oes/v1/system/info')).toBe(false);
  });

  it('skips assets that need no policy', () => {
    expect(matches('/_next/static/chunks/main.js')).toBe(false);
    expect(matches('/favicon.ico')).toBe(false);
  });
});
