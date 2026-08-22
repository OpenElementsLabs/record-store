import { expect, test } from './fixtures';

test.describe('access management', () => {
  test('create a service account and reveal its secret once', async ({ signedIn }) => {
    const page = signedIn;
    const name = `e2e-agent-${Date.now().toString(36)}`;

    await page.goto('/service-accounts');
    await page
      .getByRole('button', { name: /create account/i })
      .first()
      .click();
    await page.getByLabel('Name').fill(name);
    await page.getByRole('button', { name: 'Create account' }).click();

    const dialog = page.getByRole('dialog');
    await expect(dialog).toContainText(/will not be shown again/i);

    // The secret starts masked and is revealed only on request.
    const secret = dialog.getByTestId('secret-value');
    await expect(secret).toHaveText(/^•+$/);
    await dialog.getByRole('button', { name: /reveal/i }).click();
    await expect(secret).not.toHaveText(/^•+$/);

    await dialog.getByRole('checkbox').check();
    await dialog.getByRole('button', { name: 'Done' }).click();
    await expect(page.getByText(name)).toBeVisible();
  });

  test('rotating a credential leaves the previous one active', async ({ signedIn }) => {
    const page = signedIn;
    const name = `e2e-rotate-${Date.now().toString(36)}`;

    await page.goto('/service-accounts');
    await page
      .getByRole('button', { name: /create account/i })
      .first()
      .click();
    await page.getByLabel('Name').fill(name);
    await page.getByRole('button', { name: 'Create account' }).click();
    await page.getByRole('dialog').getByRole('checkbox').check();
    await page.getByRole('dialog').getByRole('button', { name: 'Done' }).click();

    const row = page.getByRole('row').filter({ hasText: name });
    await expect(row).toContainText('1 active');

    await page.getByRole('button', { name: new RegExp(`actions for ${name}`, 'i') }).click();
    await page.getByRole('menuitem', { name: /rotate credential/i }).click();

    const dialog = page.getByRole('dialog');
    await expect(dialog).toContainText(/previous credential is still active/i);
    await dialog.getByRole('checkbox').check();
    await dialog.getByRole('button', { name: 'Done' }).click();

    // Both credentials remain usable so applications can roll forward safely.
    await expect(row).toContainText('2 active');
  });

  test('a policy warns when it grants broad access', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/policies');
    await page
      .getByRole('button', { name: /create policy/i })
      .first()
      .click();

    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill(`e2e-policy-${Date.now().toString(36)}`);
    await dialog.getByLabel('Resource pattern').fill('*');
    await dialog.getByRole('button', { name: 'Add' }).click();

    await expect(dialog).toContainText(/allows access across every matching resource/i);
    await dialog.getByRole('button', { name: 'Create policy' }).click();
    await expect(page.getByText('Broad access').first()).toBeVisible();
  });

  test('audit events record console activity with a request id', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/audit');
    await expect(page.getByRole('heading', { name: 'Audit log' })).toBeVisible();

    const firstRow = page.getByRole('row').nth(1);
    await expect(firstRow).toBeVisible({ timeout: 15_000 });
    await firstRow.click();

    const dialog = page.getByRole('dialog');
    await expect(dialog).toContainText('Event ID');
    await expect(dialog).toContainText('Request ID');
    // A record must never carry credential material.
    await expect(dialog).not.toContainText(/secret/i);
    await expect(dialog).not.toContainText(/bearer/i);
  });

  test('storage events are a separate feed from the audit trail', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/events');
    await expect(page.getByRole('heading', { name: 'Events' })).toBeVisible();
    await expect(page.getByText(/storage events recorded/i)).toBeVisible();
  });
});
