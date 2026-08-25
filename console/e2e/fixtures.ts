import { expect, type Page, test as base } from '@playwright/test';

export const MANAGEMENT_TOKEN = 'e2e-management-system-token-32-bytes-long';
export const AUDITOR_TOKEN = 'e2e-management-auditor-token-32-bytes-long';

/** Signs in through the real login form and waits for the shell. */
export async function signIn(page: Page, token = MANAGEMENT_TOKEN): Promise<void> {
  await page.goto('/login');
  await page.getByLabel('Management token').fill(token);
  await page.getByRole('button', { name: 'Continue to console' }).click();
  await expect(page.getByRole('navigation', { name: 'Console sections' })).toBeVisible();
}

/** A unique bucket name so parallel or repeated runs never collide. */
export function uniqueBucket(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}${Math.floor(Math.random() * 1000)}`;
}

export const test = base.extend<{ signedIn: Page }>({
  signedIn: async ({ page }, use) => {
    await signIn(page);
    await use(page);
  },
});

export { expect };
