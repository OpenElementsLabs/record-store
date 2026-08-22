import { NextResponse, type NextRequest } from 'next/server';

/**
 * Applies a per-request content security policy.
 *
 * A fresh nonce lets the framework tag its own bootstrap scripts, so the policy
 * never has to allow inline scripting in general. Styles remain inline-capable
 * because the framework injects critical CSS, which is a far smaller risk than
 * permitting arbitrary script.
 */
export function middleware(request: NextRequest): NextResponse {
  const nonce = Buffer.from(crypto.randomUUID()).toString('base64');
  const directives = [
    "default-src 'self'",
    `script-src 'self' 'nonce-${nonce}' 'strict-dynamic'`,
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: blob:",
    "font-src 'self'",
    // The console talks only to its own origin; the management API is reached
    // through this server, never directly from the browser.
    "connect-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'self'",
    "frame-ancestors 'none'",
    'upgrade-insecure-requests',
  ];
  const policy = directives.join('; ');

  const headers = new Headers(request.headers);
  headers.set('x-nonce', nonce);
  headers.set('content-security-policy', policy);

  const response = NextResponse.next({ request: { headers } });
  response.headers.set('content-security-policy', policy);
  return response;
}

export const config = {
  matcher: [
    // Documents only. Static assets need no policy header, and `/api` routes
    // must not pass through here at all: this middleware re-issues the request,
    // which caps how large a body may be, and an object upload streaming
    // through `/api/oes` would be silently truncated at that cap. API responses
    // are data rather than documents, so they get their own policy from
    // `next.config.ts` instead of a nonce from here.
    '/((?!api/|_next/static|_next/image|favicon.ico).*)',
  ],
};
