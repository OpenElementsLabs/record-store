import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { forwardCapabilityRequest, readShareDescriptor } from './capability-proxy';

function request(
  method: string,
  headers: Record<string, string> = {},
  url = 'http://console.test/s/AbCdEf/content',
): Request {
  return new Request(url, { method, headers: { host: 'console.test', ...headers } });
}

describe('public capability boundary', () => {
  beforeEach(() => {
    process.env.OES_API_URL = 'http://management.test:7601/';
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    delete process.env.OES_API_URL;
  });

  it('never attaches a credential of any kind', async () => {
    // A share token authorizes itself. Adding an ambient credential here would
    // turn a narrow read capability into whatever the console's session can do.
    const fetch = vi.fn().mockResolvedValue(new Response('bytes'));
    vi.stubGlobal('fetch', fetch);

    await forwardCapabilityRequest(
      request('GET', {
        authorization: 'Bearer browser-supplied',
        cookie: 'oes_session=a-management-session',
      }),
      '/s/AbCdEf/content',
    );

    const sent = fetch.mock.calls[0]?.[1] as RequestInit;
    const headers = new Headers(sent.headers);
    expect(headers.get('authorization')).toBeNull();
    expect(headers.get('cookie')).toBeNull();
  });

  it('forwards the headers that make range requests and CORS work', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response('bytes'));
    vi.stubGlobal('fetch', fetch);

    await forwardCapabilityRequest(
      request('GET', {
        range: 'bytes=100-199',
        'if-none-match': '"etag-1"',
        origin: 'https://example.com',
        'x-oes-share-ticket': 'ticket-value',
        'x-untrusted': 'drop-me',
      }),
      '/s/AbCdEf/content',
    );

    const headers = new Headers((fetch.mock.calls[0]?.[1] as RequestInit).headers);
    expect(headers.get('range')).toBe('bytes=100-199');
    expect(headers.get('if-none-match')).toBe('"etag-1"');
    expect(headers.get('origin')).toBe('https://example.com');
    expect(headers.get('x-oes-share-ticket')).toBe('ticket-value');
    expect(headers.get('x-untrusted')).toBeNull();
  });

  it('replaces the forwarded client address rather than trusting the caller', async () => {
    // The backend partitions its rate limiters by this value, so a client that
    // could choose its own would be choosing its own allowance.
    const fetch = vi.fn().mockResolvedValue(new Response('bytes'));
    vi.stubGlobal('fetch', fetch);

    await forwardCapabilityRequest(request('GET', {}), '/s/AbCdEf/content');
    expect(
      new Headers((fetch.mock.calls[0]?.[1] as RequestInit).headers).get('x-forwarded-for'),
    ).toBeNull();

    fetch.mockClear();
    await forwardCapabilityRequest(
      request('GET', { 'x-forwarded-for': '203.0.113.7, 10.0.0.1' }),
      '/s/AbCdEf/content',
    );
    expect(
      new Headers((fetch.mock.calls[0]?.[1] as RequestInit).headers).get('x-forwarded-for'),
    ).toBe('203.0.113.7');
  });

  it('carries a partial response back intact, headers included', async () => {
    // A `206` whose `Content-Range` was dropped on the way back is a truncated
    // object as far as the browser is concerned.
    const upstream = new Response('slice', {
      status: 206,
      headers: {
        'content-type': 'video/mp4',
        'content-range': 'bytes 100-199/4096',
        'content-length': '100',
        'accept-ranges': 'bytes',
        etag: '"etag-1"',
        'cache-control': 'public, max-age=60',
        'access-control-allow-origin': 'https://example.com',
        vary: 'Origin',
        'content-security-policy': 'sandbox',
        'x-content-type-options': 'nosniff',
      },
    });
    const fetch = vi.fn().mockResolvedValue(upstream);
    vi.stubGlobal('fetch', fetch);

    const response = await forwardCapabilityRequest(request('GET'), '/s/AbCdEf/content');

    expect(response.status).toBe(206);
    expect(response.headers.get('content-range')).toBe('bytes 100-199/4096');
    expect(response.headers.get('accept-ranges')).toBe('bytes');
    expect(response.headers.get('access-control-allow-origin')).toBe('https://example.com');
    expect(response.headers.get('vary')).toBe('Origin');
    expect(response.headers.get('content-security-policy')).toBe('sandbox');
    expect(response.headers.get('cache-control')).toBe('public, max-age=60');
  });

  it('drops upstream headers that were never meant to reach the public', async () => {
    const fetch = vi.fn().mockResolvedValue(
      new Response('bytes', {
        headers: { 'set-cookie': 'oes_session=leak', 'x-internal-node': 'node-3' },
      }),
    );
    vi.stubGlobal('fetch', fetch);

    const response = await forwardCapabilityRequest(request('GET'), '/s/AbCdEf/content');
    expect(response.headers.get('set-cookie')).toBeNull();
    expect(response.headers.get('x-internal-node')).toBeNull();
  });

  it('answers a transport failure without describing the internals', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('ECONNREFUSED 10.0.0.4:7601')));

    const response = await forwardCapabilityRequest(request('GET'), '/s/AbCdEf/content');
    expect(response.status).toBe(503);
    const body = await response.text();
    expect(body).not.toContain('10.0.0.4');
    expect(body).not.toContain('ECONNREFUSED');
  });

  it('sends an unlock ticket when reading a descriptor and omits it otherwise', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response('{}'));
    vi.stubGlobal('fetch', fetch);

    await readShareDescriptor('AbCdEf', 'ticket-value');
    expect(
      new Headers((fetch.mock.calls[0]?.[1] as RequestInit).headers).get('x-oes-share-ticket'),
    ).toBe('ticket-value');

    fetch.mockClear();
    await readShareDescriptor('AbCdEf', null);
    expect(
      new Headers((fetch.mock.calls[0]?.[1] as RequestInit).headers).get('x-oes-share-ticket'),
    ).toBeNull();
  });
});
