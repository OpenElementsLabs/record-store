import { describe, expect, it } from 'vitest';

import { ApiError, apiErrorFromResponse, asApiError, networkError } from './error';

function response(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json', ...headers },
  });
}

describe('apiErrorFromResponse', () => {
  it('reads the documented error envelope', async () => {
    const error = await apiErrorFromResponse(
      response(404, {
        error: { code: 'BUCKET_NOT_FOUND', message: 'Bucket was not found', request_id: 'req-1' },
      }),
    );
    expect(error).toBeInstanceOf(ApiError);
    expect(error.code).toBe('BUCKET_NOT_FOUND');
    expect(error.message).toBe('Bucket was not found');
    expect(error.requestId).toBe('req-1');
    expect(error.kind).toBe('not-found');
    expect(error.isNotFound).toBe(true);
  });

  it('classifies statuses so the UI can react differently', async () => {
    const cases: readonly [number, string][] = [
      [401, 'unauthorized'],
      [403, 'forbidden'],
      [404, 'not-found'],
      [409, 'conflict'],
      [400, 'invalid'],
      [503, 'unavailable'],
      [500, 'server'],
    ];
    for (const [status, kind] of cases) {
      const error = await apiErrorFromResponse(
        response(status, { error: { code: 'X', message: 'm', request_id: 'r' } }),
      );
      expect(error.kind, `status ${status}`).toBe(kind);
    }
  });

  it('still produces a usable error when a proxy answers with a non-envelope body', async () => {
    const error = await apiErrorFromResponse(
      new Response('<html>gateway</html>', {
        status: 502,
        headers: { 'x-request-id': 'proxy-1' },
      }),
    );
    expect(error.code).toBe('HTTP_502');
    expect(error.requestId).toBe('proxy-1');
    expect(error.kind).toBe('server');
    expect(error.message).toContain('502');
  });

  it('provides an actionable message for a lost session', async () => {
    const error = await apiErrorFromResponse(new Response(null, { status: 401 }));
    expect(error.isUnauthorized).toBe(true);
    expect(error.message).toContain('Sign in');
  });

  it('never exposes internals for an ignored malformed body', async () => {
    const error = await apiErrorFromResponse(
      new Response('not json', { status: 500, headers: { 'content-type': 'application/json' } }),
    );
    expect(error.message).not.toContain('not json');
  });
});

describe('networkError', () => {
  it('is distinguishable from an HTTP failure', () => {
    const error = networkError();
    expect(error.kind).toBe('network');
    expect(error.status).toBe(0);
    expect(asApiError(error)).toBe(error);
    expect(asApiError(new Error('other'))).toBeNull();
  });
});
