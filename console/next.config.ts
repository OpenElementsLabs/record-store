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

const nextConfig: NextConfig = {
  reactStrictMode: true,
  poweredByHeader: false,
  // The console is deployed as a container image alongside the OES server.
  output: 'standalone',
  async headers() {
    return [{ source: '/:path*', headers: securityHeaders }];
  },
};

export default nextConfig;
