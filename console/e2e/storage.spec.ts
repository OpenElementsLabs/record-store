import { expect, test, uniqueBucket } from './fixtures';

test.describe('storage workflows', () => {
  test('create a bucket, upload, download, and delete an object', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('e2e');

    await page.goto('/buckets');
    await page
      .getByRole('button', { name: /create bucket/i })
      .first()
      .click();
    await page.getByLabel('Bucket name').fill(bucket);
    await page.getByRole('button', { name: 'Create bucket' }).click();
    await expect(page.getByRole('link', { name: bucket })).toBeVisible();

    // The list already carries accounting, so the new bucket shows zero objects.
    const row = page.getByRole('row').filter({ has: page.getByRole('link', { name: bucket }) });
    await expect(row).toContainText('0');

    await page.getByRole('link', { name: bucket }).click();
    await expect(page.getByRole('heading', { name: bucket })).toBeVisible();
    await expect(page.getByText('This bucket is empty')).toBeVisible();

    const contents = 'hello from the OES console end-to-end test\n';
    await page.setInputFiles('input[type="file"]', {
      name: 'greeting.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from(contents),
    });

    await expect(page.getByText('Uploads')).toBeVisible();
    await expect(page.getByRole('link', { name: /greeting\.txt/ })).toBeVisible({
      timeout: 20_000,
    });

    // Downloading streams from OES through the console's own origin.
    const download = page.waitForEvent('download');
    await page.getByRole('button', { name: /actions for greeting\.txt/i }).click();
    await page.getByRole('menuitem', { name: /download/i }).click();
    const file = await download;
    expect(file.suggestedFilename()).toBe('greeting.txt');

    await page.getByRole('link', { name: /greeting\.txt/ }).click();
    await expect(page.getByRole('heading', { name: 'greeting.txt' })).toBeVisible();
    await expect(page.getByText('text/plain')).toBeVisible();
    await expect(page.getByText(/^sha256:/)).toBeVisible();
    // Internal storage details must never appear in the UI.
    await expect(page.getByText(/payload_format/i)).toHaveCount(0);

    await page.getByRole('button', { name: 'Delete' }).click();
    await page.getByRole('button', { name: 'Delete object' }).click();
    await expect(page.getByText('This bucket is empty')).toBeVisible();
  });

  test('uploads into a prefix and navigates by breadcrumb', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('prefix');

    await page.goto('/buckets');
    await page
      .getByRole('button', { name: /create bucket/i })
      .first()
      .click();
    await page.getByLabel('Bucket name').fill(bucket);
    await page.getByRole('button', { name: 'Create bucket' }).click();
    await page.getByRole('link', { name: bucket }).click();

    await page.setInputFiles('input[type="file"]', {
      name: 'note.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('top level'),
    });
    await expect(page.getByRole('link', { name: /note\.txt/ })).toBeVisible({ timeout: 20_000 });

    // Prefixes are logical: they exist only because objects sit under them.
    await page.goto(`/buckets/${bucket}?prefix=reports%2F`);
    await page.setInputFiles('input[type="file"]', {
      name: 'q1.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('quarterly'),
    });
    await expect(page.getByRole('link', { name: /q1\.txt/ })).toBeVisible({ timeout: 20_000 });

    await page.goto(`/buckets/${bucket}`);
    await expect(page.getByRole('button', { name: 'reports' })).toBeVisible();
    await page.getByRole('button', { name: 'reports' }).click();
    await expect(page.getByRole('link', { name: /q1\.txt/ })).toBeVisible();
    await expect(page).toHaveURL(/prefix=reports/);
  });

  test('a non-empty bucket cannot be deleted and says why', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('nonempty');

    await page.goto('/buckets');
    await page
      .getByRole('button', { name: /create bucket/i })
      .first()
      .click();
    await page.getByLabel('Bucket name').fill(bucket);
    await page.getByRole('button', { name: 'Create bucket' }).click();
    await page.getByRole('link', { name: bucket }).click();
    await page.setInputFiles('input[type="file"]', {
      name: 'blocker.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('present'),
    });
    await expect(page.getByRole('link', { name: /blocker\.txt/ })).toBeVisible({ timeout: 20_000 });

    await page.goto('/buckets');
    await page.getByRole('button', { name: new RegExp(`actions for ${bucket}`, 'i') }).click();
    await page.getByText('Delete bucket').click();
    await page.getByRole('button', { name: 'Delete bucket' }).click();

    // The backend's own refusal is shown rather than a generic failure.
    await expect(page.getByRole('dialog')).toContainText(/not empty/i);
  });

  test('version history appears once versioning is enabled', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('versions');

    await page.goto('/buckets');
    await page
      .getByRole('button', { name: /create bucket/i })
      .first()
      .click();
    await page.getByLabel('Bucket name').fill(bucket);
    await page.getByRole('button', { name: 'Create bucket' }).click();
    await page.getByRole('link', { name: bucket }).click();

    // The tab only exists after versioning is turned on.
    await expect(page.getByRole('tab', { name: 'Versions' })).toHaveCount(0);
    await page.getByRole('tab', { name: 'Settings' }).click();
    await page.getByRole('button', { name: 'Enable versioning' }).click();
    await expect(page.getByRole('tab', { name: 'Versions' })).toBeVisible();

    await page.getByRole('tab', { name: 'Objects' }).click();
    for (const body of ['first revision', 'second revision']) {
      await page.setInputFiles('input[type="file"]', {
        name: 'doc.txt',
        mimeType: 'text/plain',
        buffer: Buffer.from(body),
      });
      await expect(page.getByRole('link', { name: /doc\.txt/ })).toBeVisible({ timeout: 20_000 });
    }

    await page.getByRole('tab', { name: 'Versions' }).click();
    await expect(page.getByText('Current')).toBeVisible();
    await expect(page.getByRole('row').filter({ hasText: 'doc.txt' })).toHaveCount(2);
  });
});
