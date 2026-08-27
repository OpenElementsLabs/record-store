import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { readSessionToken } from './session';
import { forwardToManagementApi, verifyCredential } from './proxy';

vi.mock('./session', () => ({ readSessionToken: vi.fn() }));

const sessionToken = vi.mocked(readSessionToken);

function request(
  method: string,
  headers: Record<string, string> = {},
  body?: ReadableStream<Uint8Array>,
): Request {
  return new Request('http://console.test/api/record-store/v1/objects?version=4', {
    method,
    headers: { host: 'console.test', ...headers },
    ...(body ? { body, duplex: 'half' } : {}),
  } as RequestInit);
}

describe('management API proxy boundary', () => {
  beforeEach(() => {
    process.env.RECORD_STORE_API_URL = 'http://management.test:7601/';
    sessionToken.mockResolvedValue('server-session-token');
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    delete process.env.RECORD_STORE_API_URL;
  });

  it('requires the HTTP-only session before contacting the upstream API', async () => {
    sessionToken.mockResolvedValue(null);
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);

    const response = await forwardToManagementApi(request('GET'), ['v1', 'buckets']);

    expect(response.status).toBe(401);
    await expect(response.json()).resolves.toMatchObject({ error: { code: 'UNAUTHORIZED' } });
    expect(fetch).not.toHaveBeenCalled();
  });

  it('attaches only the server session token and preserves the encoded path and query', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response('{}'));
    vi.stubGlobal('fetch', fetch);

    await forwardToManagementApi(
      request('GET', {
        authorization: 'Bearer browser-controlled-token',
        cookie: 'private=browser-cookie',
        accept: 'application/json',
        'x-request-id': 'request-7',
        'x-untrusted': 'drop-me',
      }),
      ['v1', 'objects', 'report 2026.pdf'],
    );

    expect(fetch).toHaveBeenCalledOnce();
    const [url, init] = fetch.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('http://management.test:7601/api/v1/objects/report%202026.pdf?version=4');
    const headers = new Headers(init.headers);
    expect(headers.get('authorization')).toBe('Bearer server-session-token');
    expect(headers.get('cookie')).toBeNull();
    expect(headers.get('x-untrusted')).toBeNull();
    expect(headers.get('accept')).toBe('application/json');
    expect(headers.get('x-request-id')).toBe('request-7');
  });

  it.each(['GET', 'HEAD'])('allows origin-free %s requests', async (method) => {
    const fetch = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    vi.stubGlobal('fetch', fetch);

    expect((await forwardToManagementApi(request(method), ['v1', 'buckets'])).status).toBe(204);
  });

  it.each(['POST', 'PUT', 'PATCH', 'DELETE'])(
    'accepts same-origin %s requests and forwards the method',
    async (method) => {
      const fetch = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
      vi.stubGlobal('fetch', fetch);

      const response = await forwardToManagementApi(
        request(method, { origin: 'http://console.test' }),
        ['v1', 'buckets'],
      );

      expect(response.status).toBe(204);
      expect(fetch.mock.calls[0]?.[1]).toMatchObject({ method });
    },
  );

  it('rejects missing, malformed, cross-scheme, and foreign mutation origins', async () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    const origins = [undefined, 'not a URL', 'https://console.test', 'http://foreign.test'];

    for (const origin of origins) {
      const response = await forwardToManagementApi(request('POST', origin ? { origin } : {}), [
        'v1',
        'buckets',
      ]);
      expect(response.status, String(origin)).toBe(403);
    }
    expect(fetch).not.toHaveBeenCalled();
  });

  it('uses the external forwarded HTTPS origin, including its port', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    vi.stubGlobal('fetch', fetch);
    const headers = {
      origin: 'https://objects.example.test:8443',
      'x-forwarded-host': 'objects.example.test:8443',
      'x-forwarded-proto': 'https',
    };

    expect(
      (await forwardToManagementApi(request('DELETE', headers), ['v1', 'buckets', 'old'])).status,
    ).toBe(204);

    const wrongPort = await forwardToManagementApi(
      request('DELETE', { ...headers, origin: 'https://objects.example.test' }),
      ['v1', 'buckets', 'old'],
    );
    expect(wrongPort.status).toBe(403);
  });

  it('streams request and response bodies without buffering them', async () => {
    const upload = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new Uint8Array([1, 2, 3]));
        controller.close();
      },
    });
    const download = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new Uint8Array([4, 5, 6]));
        controller.close();
      },
    });
    const incoming = request(
      'PUT',
      { origin: 'http://console.test', 'content-type': 'application/octet-stream' },
      upload,
    );
    const fetch = vi.fn().mockResolvedValue(
      new Response(download, {
        headers: {
          'content-type': 'application/octet-stream',
          etag: 'object-etag',
          'set-cookie': 'must-not-leak=true',
          'x-private-upstream': 'drop-me',
        },
      }),
    );
    vi.stubGlobal('fetch', fetch);

    const response = await forwardToManagementApi(incoming, ['v1', 'objects', 'large.bin']);
    const init = fetch.mock.calls[0]?.[1] as RequestInit;

    expect(init.body).toBe(incoming.body);
    expect(response.body).toBe(download);
    expect(response.headers.get('etag')).toBe('object-etag');
    expect(response.headers.get('set-cookie')).toBeNull();
    expect(response.headers.get('x-private-upstream')).toBeNull();
    expect(response.headers.get('cache-control')).toBe('no-store');
    await expect(response.arrayBuffer()).resolves.toEqual(new Uint8Array([4, 5, 6]).buffer);
  });

  it.each([401, 403, 409, 500])('preserves upstream HTTP status %s', async (status) => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        new Response(JSON.stringify({ error: { code: 'UPSTREAM' } }), {
          status,
          headers: { 'content-type': 'application/json' },
        }),
      ),
    );

    const response = await forwardToManagementApi(request('GET'), ['v1', 'auth', 'session']);
    expect(response.status).toBe(status);
  });

  it('maps an unreachable upstream to a bounded public error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('private connection details')));

    const response = await forwardToManagementApi(request('GET'), ['v1', 'buckets']);
    expect(response.status).toBe(503);
    const body = await response.text();
    expect(body).toContain('MANAGEMENT_API_UNREACHABLE');
    expect(body).not.toContain('private connection details');
  });
});

describe('credential verification', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('accepts a valid upstream session without returning the credential', async () => {
    const fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ role: 'auditor' }), {
        headers: { 'content-type': 'application/json' },
      }),
    );
    vi.stubGlobal('fetch', fetch);

    await expect(verifyCredential('private-token')).resolves.toEqual({
      ok: true,
      body: { role: 'auditor' },
    });
    expect(fetch.mock.calls[0]?.[1]).toMatchObject({
      headers: { authorization: 'Bearer private-token', accept: 'application/json' },
    });
  });

  it.each([401, 403])('rejects an invalid or expired upstream session (%s)', async (status) => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(null, { status })));
    await expect(verifyCredential('expired-token')).resolves.toEqual({ ok: false, status });
  });

  it('reports an unavailable verifier without leaking the exception', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('connect ECONNREFUSED')));
    await expect(verifyCredential('token')).resolves.toEqual({ ok: false, status: 503 });
  });
});
