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
    // The browser reaches Record Store only through this server's own origin.
    expect(policy).toContain("connect-src 'self'");
    expect(policy).toContain("frame-ancestors 'none'");
  });

  it('mints a distinct nonce per request', () => {
    expect(policyFor('/buckets')).not.toBe(policyFor('/buckets'));
  });

  it('gives the public share page its own stricter policy', () => {
    // The share page is opened by strangers and shows untrusted stored content,
    // so it is written separately rather than inheriting the console's policy.
    expect(matches('/s/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abc')).toBe(true);

    const policy = policyFor('/s/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abc');
    expect(policy).toMatch(/script-src 'self' 'nonce-[A-Za-z0-9+/=]+' 'strict-dynamic'/);
    // A share page submits its password with `fetch`, so no navigation-style
    // form submission is needed anywhere on it.
    expect(policy).toContain("form-action 'none'");
    // A page that can be framed can be clickjacked into revealing what it shows.
    expect(policy).toContain("frame-ancestors 'none'");
    expect(policy).toContain("connect-src 'self'");
    expect(policy).not.toContain('unsafe-eval');
  });

  it('marks a share page as neither indexable nor a referrer source', () => {
    const request = new Request('http://console.test/s/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abc');
    const response = middleware(request as never);
    expect(response.headers.get('x-robots-tag')).toContain('noindex');
    expect(response.headers.get('referrer-policy')).toBe('no-referrer');
  });

  it('keeps share byte routes out of the middleware entirely', () => {
    // These re-issue the request through a body cap, which would silently
    // truncate a streaming object. The share *page* is a document and still
    // goes through; the routes that carry bytes must not.
    expect(matches('/s/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abc/content')).toBe(false);
    expect(matches('/s/AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abc/unlock')).toBe(false);
  });

  it('leaves API routes out of the middleware entirely', () => {
    // This middleware re-issues the request, which caps how large a body may
    // be. An object upload streaming through `/api/record-store` would be truncated at
    // that cap — silently, because the truncated request still succeeds.
    expect(matches('/api/record-store/v1/buckets/uploads/object/archive/backup.tar')).toBe(false);
    expect(matches('/api/auth/login')).toBe(false);
    expect(matches('/api/record-store/v1/system/info')).toBe(false);
  });

  it('skips assets that need no policy', () => {
    expect(matches('/_next/static/chunks/main.js')).toBe(false);
    expect(matches('/favicon.ico')).toBe(false);
  });
});
