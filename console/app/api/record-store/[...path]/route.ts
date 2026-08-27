import { forwardToManagementApi } from '@/lib/server/proxy';

/**
 * Forwards browser requests to the Record Store management API.
 *
 * Keeping this on the console's own origin means the session credential can live
 * in an HTTP-only cookie, and the management API needs no cross-origin
 * configuration.
 */
type Context = { params: Promise<{ path: string[] }> };

async function handle(request: Request, context: Context): Promise<Response> {
  const { path } = await context.params;
  return forwardToManagementApi(request, path ?? []);
}

export const GET = handle;
export const HEAD = handle;
export const POST = handle;
export const PUT = handle;
export const PATCH = handle;
export const DELETE = handle;

// Object transfers stream, so this route must not be statically optimised.
export const dynamic = 'force-dynamic';
