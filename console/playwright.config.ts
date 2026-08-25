import { defineConfig, devices } from '@playwright/test';

const S3_PORT = testPort('OES_E2E_S3_PORT', 47_600);
const API_PORT = testPort('OES_E2E_API_PORT', 47_601);
const CONSOLE_PORT = testPort('OES_E2E_CONSOLE_PORT', 47_602);
const RPC_PORT = testPort('OES_E2E_RPC_PORT', 47_603);
const HARNESS_PORT = testPort('OES_E2E_HARNESS_PORT', 47_604);
const CONSOLE_URL = `http://127.0.0.1:${CONSOLE_PORT}`;
const MANAGEMENT_URL = `http://127.0.0.1:${API_PORT}`;
const MANAGEMENT_TOKEN = 'e2e-management-system-token-32-bytes-long';

// Test workers and global setup inherit these authoritative endpoints.
process.env.OES_E2E_MANAGEMENT_URL = MANAGEMENT_URL;
process.env.OES_E2E_CONSOLE_URL = CONSOLE_URL;
process.env.OES_E2E_EXPECTED_MODE = 'standalone';
process.env.OES_E2E_TOKEN = MANAGEMENT_TOKEN;
// Embeds are published on the storage endpoint, so the specs need to know it.
process.env.OES_E2E_S3_PORT = String(S3_PORT);

export default defineConfig({
  testDir: './e2e',
  testIgnore: ['cluster.spec.ts'],
  globalSetup: './e2e/global-setup.ts',
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',
  timeout: 60_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: CONSOLE_URL,
    trace: 'retain-on-failure',
    ...devices['Desktop Chrome'],
  },
  webServer: [
    {
      command: 'node e2e/start-oes.mjs',
      url: `http://127.0.0.1:${HARNESS_PORT}/ready`,
      reuseExistingServer: false,
      timeout: 600_000,
      stdout: 'pipe',
      stderr: 'pipe',
      env: {
        OES_E2E_S3_PORT: String(S3_PORT),
        OES_E2E_API_PORT: String(API_PORT),
        OES_E2E_CONSOLE_PORT: String(CONSOLE_PORT),
        OES_E2E_RPC_PORT: String(RPC_PORT),
        OES_E2E_HARNESS_PORT: String(HARNESS_PORT),
        OES_E2E_TOKEN: MANAGEMENT_TOKEN,
      },
    },
    {
      command: 'node e2e/start-console.mjs',
      url: CONSOLE_URL,
      reuseExistingServer: false,
      timeout: 120_000,
      env: {
        PORT: String(CONSOLE_PORT),
        OES_API_URL: MANAGEMENT_URL,
        OES_CONSOLE_SECURE_COOKIES: 'false',
        NODE_ENV: 'production',
      },
    },
  ],
  metadata: { managementToken: MANAGEMENT_TOKEN, mode: 'standalone' },
});

function testPort(name: string, fallback: number): number {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < 1 || value > 65_535) {
    throw new Error(`${name} must be a valid TCP port`);
  }
  return value;
}
