import { defineConfig, devices } from '@playwright/test';

/**
 * End-to-end configuration.
 *
 * Both the OES server and the console are started for the run, and the console
 * is pointed at the real management API. The primary environment is standalone:
 * cluster mode is not required to exercise the console.
 */
const CONSOLE_URL = 'http://127.0.0.1:7602';
const MANAGEMENT_TOKEN = 'e2e-management-system-token-32-bytes-long';

export default defineConfig({
  testDir: './e2e',
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
      url: 'http://127.0.0.1:7601/ready',
      reuseExistingServer: !process.env.CI,
      // A cold Rust build dominates the first run.
      timeout: 600_000,
      stdout: 'pipe',
      stderr: 'pipe',
    },
    {
      command: 'npm run start',
      url: CONSOLE_URL,
      reuseExistingServer: !process.env.CI,
      timeout: 120_000,
      env: {
        OES_API_URL: 'http://127.0.0.1:7601',
        // The test server runs over plain HTTP on loopback.
        OES_CONSOLE_SECURE_COOKIES: 'false',
        NODE_ENV: 'production',
      },
    },
  ],
  metadata: { managementToken: MANAGEMENT_TOKEN },
});
