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

    const contents = 'hello from the Record Store console end-to-end test\n';
    await page.setInputFiles('input[type="file"]', {
      name: 'greeting.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from(contents),
    });

    // The queue panel's heading, not any prose that happens to say "uploads".
    await expect(page.getByRole('heading', { name: 'Uploads' })).toBeVisible();
    await expect(page.getByRole('link', { name: /greeting\.txt/ })).toBeVisible({
      timeout: 20_000,
    });

    // Downloading streams from Record Store through the console's own origin.
    const download = page.waitForEvent('download');
    await page.getByRole('button', { name: /actions for greeting\.txt/i }).click();
    await page.getByRole('menuitem', { name: /download/i }).click();
    const file = await download;
    expect(file.suggestedFilename()).toBe('greeting.txt');

    await page.getByRole('link', { name: /greeting\.txt/ }).click();
    await expect(page.getByRole('heading', { name: 'greeting.txt' })).toBeVisible();
    // A previewable object opens on its preview, so the file's own content is
    // what a reader sees first.
    await expect(page.getByText(contents.trim())).toBeVisible();
    await expect(page.getByText('text/plain').first()).toBeVisible();

    // Identifiers live on Overview.
    await page.getByRole('tab', { name: 'Overview' }).click();
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
    // Wait for the browser to finish loading *this* prefix before uploading.
    // Without it the file can be attached while the previous listing is still on
    // screen, and the upload races the view it is meant to land in.
    await expect(page.getByText('Nothing under this prefix')).toBeVisible({ timeout: 20_000 });
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

    // Versioning is its own tab; the history below it appears once versioning
    // is actually on, so an empty history is not shown as if it were a feature
    // that had failed.
    await page.getByRole('tab', { name: 'Versioning' }).click();
    await page.getByRole('button', { name: 'Enable versioning' }).click();
    // Matched exactly: a toast confirming the change and the explanatory copy
    // below it both contain the word, and either may be on screen at this point.
    await expect(page.getByText('Enabled', { exact: true }).first()).toBeVisible();

    await page.getByRole('tab', { name: 'Objects' }).click();
    // Each revision is a distinct size, and the wait is on that size rather than
    // on the object's name. Waiting for the name would pass instantly on the
    // second pass — the row is already there from the first upload — and the
    // version history would then be read before the second write had landed.
    for (const [size, body] of [
      [12, 'a'.repeat(12)],
      [34, 'b'.repeat(34)],
    ] as const) {
      await page.setInputFiles('input[type="file"]', {
        name: 'doc.txt',
        mimeType: 'text/plain',
        buffer: Buffer.from(body),
      });
      await expect(
        page
          .getByRole('row')
          .filter({ hasText: 'doc.txt' })
          .filter({ hasText: `${size} B` }),
      ).toBeVisible({ timeout: 20_000 });
    }

    await page.getByRole('tab', { name: 'Versioning' }).click();
    // The badge on the newest version, not the "Current state" label beside it.
    await expect(page.getByText('Current', { exact: true }).first()).toBeVisible();
    await expect(page.getByRole('row').filter({ hasText: 'doc.txt' })).toHaveCount(2);
  });
});

test.describe('finding objects across folders', () => {
  /**
   * The real question this answers: an object two folders deep, whose location
   * the reader does not remember. Folder navigation cannot answer it, and the
   * storage layer offers no substring matching — so the find is a bucket-wide
   * scan by the start of the key, against the real backend.
   */
  test('finds a nested object the folder view would hide, and states its scope', async ({
    signedIn,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('find');

    await page.goto('/buckets');
    await page
      .getByRole('button', { name: /create bucket/i })
      .first()
      .click();
    await page.getByLabel('Bucket name').fill(bucket);
    await page.getByRole('button', { name: 'Create bucket' }).click();
    await page.getByRole('link', { name: bucket }).click();

    await page.goto(`/buckets/${bucket}?prefix=reports%2F2026%2F`);
    await expect(page.getByText('Nothing under this prefix')).toBeVisible({ timeout: 20_000 });
    await page.setInputFiles('input[type="file"]', {
      name: 'annual.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('buried two folders down'),
    });
    await expect(page.getByRole('link', { name: /annual\.txt/ })).toBeVisible({ timeout: 20_000 });

    // From the bucket root the object is invisible: only the folder shows.
    await page.goto(`/buckets/${bucket}`);
    await expect(page.getByRole('button', { name: 'reports' })).toBeVisible();
    await expect(page.getByRole('link', { name: /annual\.txt/ })).toHaveCount(0);

    await page.getByLabel('Find keys beginning with').fill('reports/');
    await page.getByRole('button', { name: 'Find' }).click();

    // The whole key, because results span folders.
    await expect(page.getByRole('link', { name: 'reports/2026/annual.txt' })).toBeVisible({
      timeout: 20_000,
    });
    await expect(page.getByText(/Keys in/)).toBeVisible();
    await expect(page).toHaveURL(/find=reports/);

    // Prefix-only matching is stated, and demonstrably true: the bare file name
    // does not match a key that begins with its folder.
    await page.getByLabel('Find keys beginning with').fill('annual.txt');
    await page.getByRole('button', { name: 'Find' }).click();
    await expect(page.getByText(/No keys in .* begin with/)).toBeVisible({ timeout: 20_000 });
    // The empty state's own explanation, not the persistent scope note above it.
    await expect(page.getByText(/Matching is on the beginning of the whole key/)).toBeVisible();

    await page.getByRole('button', { name: 'Clear' }).click();
    await expect(page.getByRole('button', { name: 'reports' })).toBeVisible();
  });
});

test.describe('uploading over an existing object', () => {
  /**
   * The console knows the bucket's versioning state, so it can say what an
   * overwrite costs before it happens rather than telling the reader to go and
   * check. With versioning off, the previous bytes are genuinely unrecoverable.
   */
  test('warns before replacing a key and states the consequence for this bucket', async ({
    signedIn,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('overwrite');

    await page.goto('/buckets');
    await page
      .getByRole('button', { name: /create bucket/i })
      .first()
      .click();
    await page.getByLabel('Bucket name').fill(bucket);
    await page.getByRole('button', { name: 'Create bucket' }).click();
    await page.getByRole('link', { name: bucket }).click();

    // A new bucket does not version, and the screen says so rather than
    // deferring the question.
    await expect(page.getByText(/Versioning is off/)).toBeVisible();

    await page.setInputFiles('input[type="file"]', {
      name: 'contract.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('first'),
    });
    await expect(page.getByRole('link', { name: /contract\.txt/ })).toBeVisible({
      timeout: 20_000,
    });

    await page.setInputFiles('input[type="file"]', {
      name: 'contract.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('second'),
    });

    await expect(page.getByText('contract.txt already exists here')).toBeVisible();
    await expect(page.getByText(/previous bytes cannot be recovered/)).toBeVisible();

    // Dismissing must not upload: the file stays unsent.
    await page.getByRole('button', { name: /cancel/i }).click();
    await expect(page.getByText('contract.txt already exists here')).toHaveCount(0);

    await page.setInputFiles('input[type="file"]', {
      name: 'contract.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('second'),
    });
    await page.getByRole('button', { name: 'Upload anyway' }).click();
    await expect(page.getByRole('heading', { name: 'Uploads' })).toBeVisible();
    await expect(page.getByText('Stored successfully.')).toBeVisible({ timeout: 20_000 });
  });
});
