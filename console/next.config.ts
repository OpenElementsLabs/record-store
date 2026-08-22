import type { NextConfig } from 'next';

/**
 * Security headers applied to every console response.
 *
 * The content security policy is nonce based: `middleware.ts` mints a nonce per
 * request so Next.js can tag its own bootstrap scripts without the policy
 * having to allow inline scripting in general.
 */
const securityHeaders = [
  { key: 'X-Content-Type-Options', value: 'nosniff' },
  { key: 'X-Frame-Options', value: 'DENY' },
  { key: 'Referrer-Policy', value: 'no-referrer' },
  { key: 'Cross-Origin-Opener-Policy', value: 'same-origin' },
  {
    key: 'Permissions-Policy',
    value: 'camera=(), microphone=(), geolocation=(), payment=()',
  },
];

/**
 * The policy for API responses, which are payloads rather than pages.
 *
 * `sandbox` is deliberately absent: object downloads are served from this path
 * with `Content-Disposition: attachment`, and sandboxing a navigation response
 * can suppress the download the operator asked for.
 */
const apiPolicy = "default-src 'none'; frame-ancestors 'none'; base-uri 'none'";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  poweredByHeader: false,
  // The console is deployed as a container image alongside the OES server.
  output: 'standalone',
  async headers() {
    return [
      { source: '/:path*', headers: securityHeaders },
      // API responses are data, never documents, so they are not routed through
      // the nonce middleware. They still carry a policy: if one is ever opened
      // directly, it may load nothing at all.
      { source: '/api/:path*', headers: [{ key: 'Content-Security-Policy', value: apiPolicy }] },
    ];
  },
};

export default nextConfig;
