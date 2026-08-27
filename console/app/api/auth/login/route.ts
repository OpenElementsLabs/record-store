import { cookies } from 'next/headers';
import { z } from 'zod';

import { SESSION_COOKIE } from '@/lib/server/config';
import { verifyCredential } from '@/lib/server/proxy';
import { sessionCookieOptions } from '@/lib/server/session';

/** Eight hours: long enough for a working session, short enough to expire. */
const SESSION_SECONDS = 8 * 60 * 60;

const schema = z.object({ token: z.string().min(1).max(4096) });

/**
 * Exchanges a management credential for a session cookie.
 *
 * The credential is validated against the management API before anything is
 * stored, so an invalid value never creates a session. The response never echoes
 * the credential back.
 */
export async function POST(request: Request): Promise<Response> {
  let payload: unknown;
  try {
    payload = await request.json();
  } catch {
    return json(400, { code: 'INVALID_REQUEST', message: 'A management token is required.' });
  }
  const parsed = schema.safeParse(payload);
  if (!parsed.success) {
    return json(400, { code: 'INVALID_REQUEST', message: 'A management token is required.' });
  }

  const verified = await verifyCredential(parsed.data.token);
  if (!verified.ok) {
    if (verified.status === 503) {
      return json(503, {
        code: 'MANAGEMENT_API_UNREACHABLE',
        message: 'The Record Store management API is unreachable.',
      });
    }
    return json(401, {
      code: 'INVALID_CREDENTIALS',
      message: 'That management token was not accepted.',
    });
  }

  const store = await cookies();
  store.set(SESSION_COOKIE, parsed.data.token, sessionCookieOptions(SESSION_SECONDS));
  return new Response(JSON.stringify({ session: verified.body }), {
    status: 200,
    headers: { 'content-type': 'application/json', 'cache-control': 'no-store' },
  });
}

function json(status: number, error: { code: string; message: string }): Response {
  return new Response(JSON.stringify({ error: { ...error, request_id: '' } }), {
    status,
    headers: { 'content-type': 'application/json', 'cache-control': 'no-store' },
  });
}

export const dynamic = 'force-dynamic';
