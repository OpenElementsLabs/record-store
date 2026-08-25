import { expect, test, uniqueBucket } from './fixtures';

test.describe('standalone deployment', () => {
  test('the console reports standalone mode and hides cluster concepts', async ({ signedIn }) => {
    const page = signedIn;
    await expect(page.getByRole('heading', { name: 'Overview' })).toBeVisible();

    // A standalone operator must not be shown cluster machinery.
    await expect(page.getByRole('link', { name: 'Cluster overview' })).toHaveCount(0);
    await expect(page.getByRole('link', { name: 'Nodes' })).toHaveCount(0);
    await expect(page.getByText(/quorum/i)).toHaveCount(0);

    await expect(page.getByText('Standalone', { exact: false }).first()).toBeVisible();
  });

  test('a cluster route explains itself instead of erroring', async ({ signedIn }) => {
    await signedIn.goto('/cluster');
    await expect(signedIn.getByText('Cluster features are not enabled')).toBeVisible();
  });

  test('the overview shows real storage figures', async ({ signedIn }) => {
    await expect(signedIn.getByText('Stored data')).toBeVisible();
    await expect(signedIn.getByText('Objects', { exact: true })).toBeVisible();
    await expect(signedIn.getByText('Disk capacity')).toBeVisible();
    // Figures come from the API, so none of them may be a placeholder dash.
    await expect(signedIn.getByText('Stored data').locator('..')).not.toContainText('—');
  });

  test('system health reports readiness and capacity', async ({ signedIn }) => {
    await signedIn.goto('/system');
    await expect(signedIn.getByRole('heading', { name: 'System health' })).toBeVisible();
    // 'Ready' now appears in both the summary strip and the subsystem list.
    await expect(signedIn.getByText('Ready').first()).toBeVisible();
    await expect(signedIn.getByText('Disk capacity')).toBeVisible();
  });

  test('an unknown route renders a not-found page', async ({ signedIn }) => {
    await signedIn.goto('/does-not-exist');
    await expect(signedIn.getByRole('heading', { name: 'Page not found' })).toBeVisible();
  });

  test('the layout adapts to a narrow viewport', async ({ signedIn }) => {
    await signedIn.setViewportSize({ width: 390, height: 844 });
    await signedIn.goto('/buckets');
    // The sidebar collapses behind a toggle rather than overflowing.
    const open = signedIn.getByRole('button', { name: 'Open navigation' });
    await expect(open).toBeVisible();
    await open.click();
    await expect(signedIn.getByRole('link', { name: 'Buckets' }).first()).toBeVisible();

    const overflow = await signedIn.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(1);
  });

  test('no secret material is written to the browser console', async ({ page }) => {
    const messages: string[] = [];
    page.on('console', (message) => messages.push(message.text()));

    await page.goto('/login');
    await page.getByLabel('Management token').fill('e2e-management-system-token-32-bytes-long');
    await page.getByRole('button', { name: 'Continue to console' }).click();
    await expect(page.getByRole('navigation', { name: 'Console sections' })).toBeVisible();

    const bucket = uniqueBucket('log-check');
    await page.goto('/buckets');
    await page
      .getByRole('button', { name: /create bucket/i })
      .first()
      .click();
    await page.getByLabel('Bucket name').fill(bucket);
    await page.getByRole('button', { name: 'Create bucket' }).click();
    await expect(page.getByRole('link', { name: bucket })).toBeVisible();

    const leaked = messages.filter(
      (text) => text.includes('e2e-management-system-token') || /authorization/i.test(text),
    );
    expect(leaked).toEqual([]);
  });

  test('the command palette navigates by keyboard', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/');

    // The shortcut is printed on the trigger, so both routes in must work.
    await page.getByRole('button', { name: /Search/ }).click();
    await expect(page.getByRole('dialog')).toBeVisible();
    await page.getByLabel('Search commands').fill('audit');
    await page.keyboard.press('Enter');

    await expect(page.getByRole('heading', { name: 'Audit log' })).toBeVisible();
  });

  test('integrity reports a clean scan and refuses to overstate it', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/integrity');

    await expect(page.getByRole('heading', { name: 'Integrity' })).toBeVisible();
    await expect(page.getByText('Scan findings')).toBeVisible();
    // Standalone must never imply redundancy it does not have.
    await expect(page.getByText(/may be recoverable from another replica/)).toHaveCount(0);
  });

  test('metrics derive a rate rather than inventing one', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/metrics');

    await expect(page.getByRole('heading', { name: 'Metrics' })).toBeVisible();
    // One counter reading is not a rate, so the first paint says so.
    await expect(page.getByText('Collecting…').first()).toBeVisible();
    await expect(page.getByText(/Requests served|Requests/).first()).toBeVisible();
  });

  test('health reports disabled cluster parts as not enabled, not failed', async ({ signedIn }) => {
    const page = signedIn;
    await page.goto('/system');

    await expect(page.getByText('Subsystems')).toBeVisible();
    await expect(page.getByText('Not enabled').first()).toBeVisible();
    await expect(page.getByText('No quorum')).toHaveCount(0);
  });

  test('a bucket quota can be set and is reported back', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('quota');

    await page.goto('/buckets');
    await page
      .getByRole('button', { name: /create bucket/i })
      .first()
      .click();
    await page.getByLabel('Bucket name').fill(bucket);
    await page.getByRole('button', { name: 'Create bucket' }).click();
    await page.getByRole('link', { name: bucket }).click();

    await page.getByRole('tab', { name: 'Quota' }).click();
    await page.getByRole('radio', { name: 'Set a limit' }).first().check();
    await page.getByLabel('Amount').fill('5');
    await page.getByLabel('Storage limit unit').selectOption('GB');
    await page.getByRole('button', { name: 'Save quota' }).click();

    await expect(page.getByText('of 5.00 GB')).toBeVisible();
  });
});
