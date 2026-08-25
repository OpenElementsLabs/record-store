import { isCapabilityToken } from '@/lib/capability-token';
import { forwardCapabilityRequest } from '@/lib/server/capability-proxy';

/**
 * The share's descriptor, as JSON.
 *
 * The page at `/s/<token>` renders HTML and is what a recipient opens. This is
 * the same information for the page's own script, which needs it again after a
 * password is entered: the file name is deliberately withheld until the password
 * has been verified, so it has to be re-read from the server with the resulting
 * ticket rather than being held back in the browser.
 */
type Context = { params: Promise<{ token: string }> };

export async function GET(request: Request, context: Context): Promise<Response> {
  const { token } = await context.params;
  if (!isCapabilityToken(token)) {
    return new Response(null, { status: 404, headers: { 'cache-control': 'no-store' } });
  }
  return forwardCapabilityRequest(request, `/s/${token}`);
}

export const dynamic = 'force-dynamic';
