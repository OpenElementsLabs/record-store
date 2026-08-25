import { forwardCapabilityRequest } from '@/lib/server/capability-proxy';
import { isCapabilityToken } from '@/lib/capability-token';

/**
 * Verifies a share password.
 *
 * The password crosses this boundary once, on its way to an Argon2 comparison,
 * and is never written to a log, a cookie, or a response on the way back. What
 * comes back is a short-lived ticket the viewer holds instead.
 */
type Context = { params: Promise<{ token: string }> };

export async function POST(request: Request, context: Context): Promise<Response> {
  const { token } = await context.params;
  if (!isCapabilityToken(token)) {
    return new Response(null, { status: 404, headers: { 'cache-control': 'no-store' } });
  }
  return forwardCapabilityRequest(request, `/s/${token}/unlock`);
}

export const dynamic = 'force-dynamic';
