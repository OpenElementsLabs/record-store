import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { GET } from './route';
import { readSessionToken } from '@/lib/server/session';

vi.mock('@/lib/server/session', () => ({ readSessionToken: vi.fn() }));
vi.mock('@/lib/server/config', () => ({ managementApiUrl: () => 'http://record-store.test' }));

const sessionToken = vi.mocked(readSessionToken);
let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  sessionToken.mockResolvedValue('session-token');
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

async function body(response: Response) {
  return (await response.json()) as Record<string, unknown>;
}

describe('readiness route', () => {
  /**
   * The probe sits at the management server's root, outside the versioned
   * surface the forwarding proxy serves. Asking the proxy for it would 404, and
   * a 404 read as "not ready" would report a healthy deployment as broken —
   * which is exactly what happened before this route existed.
   */
  it('asks the management server root, not the versioned API surface', async () => {
    fetchMock.mockResolvedValue(new Response('{}', { status: 200 }));

    await GET();

    expect(String(fetchMock.mock.calls[0]?.[0])).toBe('http://record-store.test/ready');
  });

  it('reports a server that answered and declined, with its reason and identifier', async () => {
    fetchMock.mockResolvedValue(
      new Response(
        JSON.stringify({
          error: { message: 'storage is read-only', request_id: 'req-7' },
        }),
        { status: 503 },
      ),
    );

    const result = await body(await GET());

    // Answered with 200 on purpose: "not ready" is information the screen
    // renders, not a failure of this route.
    expect(result).toMatchObject({
      state: 'not-ready',
      reason: 'storage is read-only',
      request_id: 'req-7',
    });
  });

  /**
   * A server that cannot be reached and a server that declines are different
   * incidents — one is the network, one is the disk — so they must not collapse
   * into a single state.
   */
  it('separates an unreachable server from one that declined', async () => {
    fetchMock.mockRejectedValue(new TypeError('connect ECONNREFUSED'));

    const result = await body(await GET());

    expect(result.state).toBe('unreachable');
    expect(result.reason).toMatch(/did not answer/);
  });

  it('refuses without a session, so the console is not an open probe of a private deployment', async () => {
    sessionToken.mockResolvedValue(null);

    const response = await GET();

    expect(response.status).toBe(401);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('never lets a readiness answer be cached', async () => {
    fetchMock.mockResolvedValue(new Response('{}', { status: 200 }));

    const response = await GET();

    expect(response.headers.get('cache-control')).toBe('no-store');
  });
});
