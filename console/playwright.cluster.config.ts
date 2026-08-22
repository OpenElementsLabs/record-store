import { defineConfig, devices } from '@playwright/test';

const CONSOLE_PORT = testPort('OES_CLUSTER_E2E_CONSOLE_PORT', 18_602);
const API_PORT = testPort('OES_CLUSTER_E2E_NODE_1_API_PORT', 18_601);
const HARNESS_PORT = testPort('OES_CLUSTER_E2E_HARNESS_PORT', 18_604);
const CONSOLE_URL = `http://127.0.0.1:${CONSOLE_PORT}`;
const MANAGEMENT_URL = `http://127.0.0.1:${API_PORT}`;
const MANAGEMENT_TOKEN = 'e2e-management-system-token-32-bytes-long';

process.env.OES_E2E_MANAGEMENT_URL = MANAGEMENT_URL;
process.env.OES_E2E_CONSOLE_URL = CONSOLE_URL;
process.env.OES_E2E_EXPECTED_MODE = 'cluster';
process.env.OES_E2E_TOKEN = MANAGEMENT_TOKEN;

export default defineConfig({
  testDir: './e2e',
  testMatch: ['cluster.spec.ts'],
  globalSetup: './e2e/global-setup.ts',
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 1 : 0,
  reporter: 'list',
  timeout: 60_000,
  expect: { timeout: 15_000 },
  use: {
    baseURL: CONSOLE_URL,
    trace: 'retain-on-failure',
    ...devices['Desktop Chrome'],
  },
  webServer: [
    {
      command: 'node e2e/start-cluster.mjs',
      url: `http://127.0.0.1:${HARNESS_PORT}/ready`,
      reuseExistingServer: false,
      timeout: 600_000,
      stdout: 'pipe',
      stderr: 'pipe',
      env: { OES_E2E_TOKEN: MANAGEMENT_TOKEN },
    },
    {
      command: `npm run start:e2e -- --port ${CONSOLE_PORT}`,
      url: CONSOLE_URL,
      reuseExistingServer: false,
      timeout: 120_000,
      env: {
        OES_API_URL: MANAGEMENT_URL,
        OES_CONSOLE_SECURE_COOKIES: 'false',
        NODE_ENV: 'production',
      },
    },
  ],
  metadata: { managementToken: MANAGEMENT_TOKEN, mode: 'cluster' },
});

function testPort(name: string, fallback: number): number {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < 1 || value > 65_535) {
    throw new Error(`${name} must be a valid TCP port`);
  }
  return value;
}
