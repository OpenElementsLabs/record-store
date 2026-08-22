import { AUDITOR_TOKEN, MANAGEMENT_TOKEN, expect, signIn, test } from './fixtures';

test.describe('authentication', () => {
  test('an unauthenticated visit is sent to sign in', async ({ page }) => {
    await page.goto('/');
    await expect(page).toHaveURL(/\/login/);
    await expect(page.getByRole('button', { name: 'Sign in' })).toBeVisible();
  });

  test('an invalid token is rejected without creating a session', async ({ page }) => {
    await page.goto('/login');
    await page.getByLabel('Management token').fill('not-a-valid-management-token-value');
    await page.getByRole('button', { name: 'Sign in' }).click();

    await expect(page.getByRole('alert')).toContainText(/not accepted/i);
    await expect(page).toHaveURL(/\/login/);
    // No session cookie may exist after a refused sign-in.
    const cookies = await page.context().cookies();
    expect(cookies.find((cookie) => cookie.name === 'oes_session')).toBeUndefined();
  });

  test('signing in stores the session in an HTTP-only cookie', async ({ page }) => {
    await signIn(page);
    const cookies = await page.context().cookies();
    const sessionCookie = cookies.find((cookie) => cookie.name === 'oes_session');
    expect(sessionCookie).toBeDefined();
    // Script must never be able to read the credential.
    expect(sessionCookie?.httpOnly).toBe(true);
    expect(sessionCookie?.sameSite).toBe('Strict');

    const visible = await page.evaluate(() => document.cookie);
    expect(visible).not.toContain(MANAGEMENT_TOKEN);
    expect(await page.evaluate(() => window.localStorage.getItem('oes_session'))).toBeNull();
  });

  test('a protected page keeps working after a refresh', async ({ page }) => {
    await signIn(page);
    await page.goto('/buckets');
    await expect(page.getByRole('heading', { name: 'Buckets' })).toBeVisible();
    await page.reload();
    await expect(page.getByRole('heading', { name: 'Buckets' })).toBeVisible();
  });

  test('signing out clears the session and returns to sign in', async ({ page }) => {
    await signIn(page);
    await page.getByRole('button', { name: /system administrator/i }).click();
    await page.getByRole('menuitem', { name: /sign out/i }).click();
    await expect(page).toHaveURL(/\/login/);

    await page.goto('/buckets');
    await expect(page).toHaveURL(/\/login/);
  });

  test('an auditor sees read-only screens and no destructive controls', async ({ page }) => {
    await signIn(page, AUDITOR_TOKEN);
    await expect(page.getByRole('link', { name: 'Audit log' })).toBeVisible();
    // Screens an auditor cannot open are absent, not disabled.
    await expect(page.getByRole('link', { name: 'Service accounts' })).toHaveCount(0);
    await expect(page.getByRole('link', { name: 'Policies' })).toHaveCount(0);

    await page.goto('/buckets');
    await expect(page.getByRole('button', { name: /create bucket/i })).toHaveCount(0);
  });
});
