import { managementApiUrl } from '@/lib/server/config';
import { readSessionToken } from '@/lib/server/session';

/**
 * Reports whether the management API can actually serve.
 *
 * This has its own route rather than going through the forwarding proxy because
 * the readiness probe is deliberately not part of the versioned management
 * surface: it sits at the server root so an orchestrator can poll it before any
 * credential exists. The proxy only forwards under `/api/`, so it cannot reach
 * it, and pointing it there would have meant reshaping the public API to suit
 * one console screen.
 *
 * A session is still required. The probe itself needs no credential, but the
 * console must not become an unauthenticated way to probe a private deployment.
 *
 * Every outcome is answered with 200 and a body describing what happened. The
 * screen has to render "not ready" and "unreachable" differently, and both are
 * information rather than failures of this route.
 */
export async function GET(): Promise<Response> {
  const token = await readSessionToken();
  if (!token) {
    return json(401, { state: 'unreachable', reason: 'Sign in to continue.' });
  }

  let upstream: Response;
  try {
    upstream = await fetch(`${managementApiUrl()}/ready`, {
      headers: { accept: 'application/json' },
      cache: 'no-store',
      signal: AbortSignal.timeout(10_000),
    });
  } catch {
    return json(200, {
      state: 'unreachable',
      reason: 'The Record Store management API did not answer.',
    });
  }

  if (upstream.ok) return json(200, { state: 'ready' });

  // The server answered and declined. Its own message and request identifier
  // are what an operator correlates with the server log, so they are carried
  // through rather than replaced with a generic string.
  const body = (await upstream.json().catch(() => null)) as {
    error?: { message?: string; request_id?: string };
  } | null;
  return json(200, {
    state: 'not-ready',
    reason:
      body?.error?.message ?? `The server reported it is not ready (HTTP ${upstream.status}).`,
    request_id: body?.error?.request_id ?? null,
  });
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json', 'cache-control': 'no-store' },
  });
}

export const dynamic = 'force-dynamic';
