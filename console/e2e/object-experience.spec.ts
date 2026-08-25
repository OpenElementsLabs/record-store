/**
 * The object experience, end to end in a real browser.
 *
 * These exercise the things that only a browser can prove: that a preview
 * actually renders, that a share link opens for someone with no session at all,
 * that revoking one takes effect on the visitor's next request, and that an
 * embed URL works from an `<img>` on a page the console does not control.
 *
 * Fixtures are generated here rather than committed, so the bytes a test asserts
 * on are the bytes it created.
 */
import { createHash } from 'node:crypto';

import { expect, MANAGEMENT_TOKEN, test, uniqueBucket } from './fixtures';

const MANAGEMENT_URL = process.env.OES_E2E_MANAGEMENT_URL ?? 'http://127.0.0.1:47601';
/** Where object bytes are published, and therefore where embeds resolve. */
const STORAGE_URL = `http://127.0.0.1:${process.env.OES_E2E_S3_PORT ?? '47600'}`;

/** A one-pixel PNG, which a browser will genuinely decode. */
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
  'base64',
);

/** A minimal PDF a browser viewer will accept. */
const PDF = Buffer.from(
  `%PDF-1.4
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>endobj
trailer<</Root 1 0 R>>
%%EOF
`,
  'utf8',
);

async function management(path: string, init: RequestInit = {}): Promise<Response> {
  return fetch(`${MANAGEMENT_URL}${path}`, {
    ...init,
    headers: {
      authorization: `Bearer ${MANAGEMENT_TOKEN}`,
      ...(init.headers as Record<string, string> | undefined),
    },
  });
}

async function createBucket(bucket: string): Promise<void> {
  const response = await management('/api/v1/buckets', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ name: bucket }),
  });
  if (!response.ok) throw new Error(`failed to create bucket ${bucket}: HTTP ${response.status}`);
}

async function upload(
  bucket: string,
  key: string,
  contentType: string,
  body: Buffer | string,
): Promise<void> {
  const response = await management(
    `/api/v1/buckets/${encodeURIComponent(bucket)}/object/${key.split('/').map(encodeURIComponent).join('/')}`,
    { method: 'PUT', headers: { 'content-type': contentType }, body: body as BodyInit },
  );
  if (!response.ok) throw new Error(`failed to upload ${key}: HTTP ${response.status}`);
}

function objectPath(bucket: string, key: string): string {
  return `/buckets/${encodeURIComponent(bucket)}/objects/${key
    .split('/')
    .map(encodeURIComponent)
    .join('/')}`;
}

test.describe('object preview', () => {
  test('an image opens on its preview with working zoom, and downloads separately', async ({
    signedIn,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('preview-image');
    await createBucket(bucket);
    await upload(bucket, 'brand/logo.png', 'image/png', PNG);

    await page.goto(objectPath(bucket, 'brand/logo.png'));
    // Preview leads for an object OES can render.
    await expect(page.getByRole('tab', { name: 'Preview' })).toHaveAttribute(
      'aria-selected',
      'true',
    );

    const image = page.getByRole('img', { name: 'logo.png' });
    await expect(image).toBeVisible();
    // The browser really decoded it, rather than showing a broken element.
    await expect
      .poll(() => image.evaluate((element: HTMLImageElement) => element.naturalWidth))
      .toBeGreaterThan(0);

    await expect(page.getByText('100%')).toBeVisible();
    await page.getByRole('button', { name: 'Zoom in' }).click();
    await expect(page.getByText('150%')).toBeVisible();
    await page.getByRole('button', { name: 'Reset zoom' }).click();
    await expect(page.getByText('100%')).toBeVisible();

    // Download stays an attachment even though the same object previews inline.
    const download = page.waitForEvent('download');
    await page.getByRole('link', { name: 'Download' }).first().click();
    expect((await download).suggestedFilename()).toBe('logo.png');
  });

  test('text is shown as characters and truncation is announced', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('preview-text');
    await createBucket(bucket);
    await upload(bucket, 'notes.txt', 'text/plain', 'a line of plain text\n');
    // Larger than the default preview slice, so the truncation notice appears.
    await upload(bucket, 'big.txt', 'text/plain', 'x'.repeat(2 * 1024 * 1024));

    await page.goto(objectPath(bucket, 'notes.txt'));
    await expect(page.getByText('a line of plain text')).toBeVisible();
    await expect(page.getByRole('status')).toHaveCount(0);

    await page.goto(objectPath(bucket, 'big.txt'));
    await expect(page.getByRole('status')).toContainText('Showing the first');
    await expect(page.getByRole('status')).toContainText('Download the file');
  });

  test('valid JSON is formatted and invalid JSON degrades to text', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('preview-json');
    await createBucket(bucket);
    await upload(bucket, 'good.json', 'application/json', '{"replicas":3,"region":"eu"}');
    await upload(bucket, 'broken.json', 'application/json', '{"replicas":');

    await page.goto(objectPath(bucket, 'good.json'));
    await expect(page.getByText('"replicas": 3')).toBeVisible();

    await page.goto(objectPath(bucket, 'broken.json'));
    // A file that was never valid JSON says nothing about OES's storage.
    await expect(page.getByText('not valid JSON')).toBeVisible();
    await expect(page.getByText(/corrupt/i)).toHaveCount(0);
  });

  test('a PDF is framed from a sandboxed same-origin bytes route', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('preview-pdf');
    await createBucket(bucket);
    await upload(bucket, 'report.pdf', 'application/pdf', PDF);

    await page.goto(objectPath(bucket, 'report.pdf'));
    const frame = page.locator('iframe[title="Preview of report.pdf"]');
    await expect(frame).toBeVisible();

    // The response the frame loads is what carries the isolation, so its
    // headers are asserted rather than the element's attributes.
    const source = await frame.getAttribute('src');
    expect(source).toBeTruthy();
    const bytes = await page.request.get(new URL(source!, page.url()).toString());
    expect(bytes.status()).toBe(200);
    expect(bytes.headers()['content-type']).toBe('application/pdf');
    expect(bytes.headers()['content-disposition']).toBe('inline');
    expect(bytes.headers()['content-security-policy']).toContain('sandbox');
    expect(bytes.headers()['content-security-policy']).not.toContain('allow-scripts');
    expect(bytes.headers()['x-content-type-options']).toBe('nosniff');
    expect(bytes.headers()['x-frame-options']).toBe('SAMEORIGIN');
  });

  test('stored active content is never rendered and never executes', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('preview-hostile');
    await createBucket(bucket);
    await upload(
      bucket,
      'page.html',
      'text/html',
      '<script>window.__oesEscaped = true</script><h1>owned</h1>',
    );
    await upload(
      bucket,
      'drawing.svg',
      'image/svg+xml',
      '<svg xmlns="http://www.w3.org/2000/svg" onload="window.__oesEscaped = true"></svg>',
    );

    for (const key of ['page.html', 'drawing.svg']) {
      await page.goto(objectPath(bucket, key));
      await expect(page.getByRole('tab', { name: 'Preview' })).toHaveCount(0);
      await expect(page.getByText(/cannot be shown in the browser safely/)).toBeVisible();
      // Nothing from the object ran, and nothing from it was rendered.
      expect(await page.evaluate(() => '__oesEscaped' in window)).toBe(false);
      await expect(page.getByRole('heading', { name: 'owned' })).toHaveCount(0);
    }
  });

  test('a historical version previews its own bytes, and a delete marker previews nothing', async ({
    signedIn,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('preview-versions');
    await createBucket(bucket);
    await management(`/api/v1/buckets/${bucket}/versioning`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ versioning: 'enabled' }),
    });
    await upload(bucket, 'contract.txt', 'text/plain', 'first draft');
    await upload(bucket, 'contract.txt', 'text/plain', 'second draft');

    await page.goto(objectPath(bucket, 'contract.txt'));
    await expect(page.getByText('second draft')).toBeVisible();

    await page.getByRole('tab', { name: 'Versions' }).click();
    const rows = page.getByRole('row').filter({ hasText: 'contract.txt' });
    await expect(rows).toHaveCount(2);

    // The older row is the one without the "Current" badge.
    const older = rows.filter({ hasNot: page.getByText('Current', { exact: true }) }).first();
    await older.getByRole('button', { name: /Actions for version/ }).click();
    await page.getByRole('menuitem', { name: 'Preview this version' }).click();

    await expect(page.getByText('Historical version').first()).toBeVisible();
    await expect(page.getByText('first draft')).toBeVisible();
    await expect(page.getByText('second draft')).toHaveCount(0);

    // A delete marker has no bytes, so it is offered no preview at all.
    await management(`/api/v1/buckets/${bucket}/object/contract.txt`, { method: 'DELETE' });
    await page.goto(`${objectPath(bucket, 'contract.txt')}?tab=versions`);
    const marker = page.getByRole('row').filter({ hasText: 'Delete marker' });
    await expect(marker).toHaveCount(1);
    await marker.getByRole('button', { name: /Actions for version/ }).click();
    await expect(page.getByRole('menuitem', { name: 'Preview this version' })).toHaveCount(0);
  });

  test('an unpreviewable object gets a polished refusal, not a broken viewer', async ({
    signedIn,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('preview-binary');
    await createBucket(bucket);
    await upload(bucket, 'archive.bin', 'application/octet-stream', Buffer.from([0, 1, 2, 3]));

    await page.goto(objectPath(bucket, 'archive.bin'));
    await expect(page.getByRole('tab', { name: 'Overview' })).toHaveAttribute(
      'aria-selected',
      'true',
    );
    await expect(page.getByRole('tab', { name: 'Preview' })).toHaveCount(0);
    await expect(page.getByText(/cannot be shown in the browser safely/)).toBeVisible();
    await expect(page.getByRole('link', { name: 'Download' }).first()).toBeVisible();
  });
});

test.describe('share links', () => {
  // Copying a link writes to the clipboard, which a headless browser refuses
  // without an explicit grant. The console degrades gracefully when it is
  // denied, but the behaviour under test here is the copy succeeding.
  test.use({ permissions: ['clipboard-read', 'clipboard-write'] });

  test('a share is created, opened without a session, and stops when revoked', async ({
    signedIn,
    browser,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('share-flow');
    await createBucket(bucket);
    await upload(bucket, 'reports/summary.txt', 'text/plain', 'quarterly summary');

    await page.goto(`${objectPath(bucket, 'reports/summary.txt')}?tab=sharing`);
    await page.getByRole('button', { name: 'Create share link' }).click();

    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('Board review');
    await dialog.getByRole('button', { name: 'Create link' }).click();

    // The URL is shown once here, and copied from the dialog.
    const link = dialog.locator('p.font-mono');
    await expect(link).toBeVisible();
    const shareUrl = (await link.textContent())?.trim() ?? '';
    expect(shareUrl).toMatch(/\/s\/[A-Za-z0-9_-]{43}$/);
    // The URL itself reveals nothing about where the object lives.
    expect(shareUrl).not.toContain(bucket);
    expect(shareUrl).not.toContain('summary');
    await dialog.getByRole('button', { name: 'Done' }).click();

    await expect(page.getByText('Board review')).toBeVisible();
    await expect(page.getByText('Active', { exact: true })).toBeVisible();

    // A visitor with no session at all opens the link.
    const visitor = await browser.newContext();
    const visitorPage = await visitor.newPage();
    await visitorPage.goto(shareUrl);
    await expect(visitorPage.getByRole('heading', { name: 'summary.txt' })).toBeVisible();
    await expect(visitorPage.getByText('quarterly summary')).toBeVisible();
    await expect(visitorPage.getByText('Shared securely through OES')).toBeVisible();
    // No administrative surface reaches a recipient.
    await expect(visitorPage.getByRole('navigation', { name: 'Console sections' })).toHaveCount(0);
    await expect(visitorPage.getByText(bucket)).toHaveCount(0);

    const download = visitorPage.waitForEvent('download');
    await visitorPage.getByRole('link', { name: 'Download' }).click();
    expect((await download).suggestedFilename()).toBe('summary.txt');

    // Copying an existing link fetches it on demand and puts it on the clipboard.
    await page.reload();
    await page.getByRole('tab', { name: 'Sharing' }).click();
    await page.getByRole('button', { name: 'Copy' }).first().click();
    await expect(page.getByText('Copied the share link')).toBeVisible();

    await page.getByRole('button', { name: /Actions for Board review/ }).click();
    await page.getByRole('menuitem', { name: 'Revoke link' }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Revoke link' }).click();
    await expect(page.getByText('Revoked', { exact: true })).toBeVisible();

    // Revocation is authoritative on the visitor's very next request.
    await visitorPage.reload();
    await expect(visitorPage.getByText('This link is not available')).toBeVisible();
    await expect(visitorPage.getByText('quarterly summary')).toHaveCount(0);
    await visitor.close();
  });

  test('a password-protected share reveals nothing until it is unlocked', async ({
    signedIn,
    browser,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('share-password');
    await createBucket(bucket);
    await upload(bucket, 'salaries.txt', 'text/plain', 'confidential payroll');

    await page.goto(`${objectPath(bucket, 'salaries.txt')}?tab=sharing`);
    await page.getByRole('button', { name: 'Create share link' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('Payroll');
    await dialog.getByText('Require a password').click();
    await dialog.getByLabel('Password', { exact: true }).fill('correct horse battery');
    await dialog.getByRole('button', { name: 'Create link' }).click();
    const shareUrl = ((await dialog.locator('p.font-mono').textContent()) ?? '').trim();
    await dialog.getByRole('button', { name: 'Done' }).click();

    const visitor = await browser.newContext();
    const visitorPage = await visitor.newPage();
    await visitorPage.goto(shareUrl);
    await expect(visitorPage.getByRole('heading', { name: /password protected/i })).toBeVisible();
    // Not even the file name is disclosed before the password is verified.
    await expect(visitorPage.getByText('salaries.txt')).toHaveCount(0);
    await expect(visitorPage.getByText('confidential payroll')).toHaveCount(0);

    await visitorPage.getByLabel('Password', { exact: true }).fill('wrong password');
    await visitorPage.getByRole('button', { name: 'Unlock' }).click();
    // Scoped to the form's own error: the framework's route announcer also
    // carries `role="alert"`.
    await expect(visitorPage.locator('#share-password-error')).toContainText('not correct');

    await visitorPage.getByLabel('Password', { exact: true }).fill('correct horse battery');
    await visitorPage.getByRole('button', { name: 'Unlock' }).click();
    await expect(visitorPage.getByRole('heading', { name: 'salaries.txt' })).toBeVisible();
    await expect(visitorPage.getByText('confidential payroll')).toBeVisible();
    await visitor.close();
  });

  test('a view-only share offers no download', async ({ signedIn, browser }) => {
    const page = signedIn;
    const bucket = uniqueBucket('share-viewonly');
    await createBucket(bucket);
    await upload(bucket, 'preview-me.txt', 'text/plain', 'read but do not keep');

    await page.goto(`${objectPath(bucket, 'preview-me.txt')}?tab=sharing`);
    await page.getByRole('button', { name: 'Create share link' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('Read only');
    await dialog.getByText('View only', { exact: true }).click();
    await dialog.getByRole('button', { name: 'Create link' }).click();
    const shareUrl = ((await dialog.locator('p.font-mono').textContent()) ?? '').trim();
    await dialog.getByRole('button', { name: 'Done' }).click();

    const visitor = await browser.newContext();
    const visitorPage = await visitor.newPage();
    await visitorPage.goto(shareUrl);
    await expect(visitorPage.getByText('read but do not keep')).toBeVisible();
    await expect(visitorPage.getByRole('link', { name: 'Download' })).toHaveCount(0);
    await visitor.close();
  });
});

test.describe('embeds', () => {
  test('an embed renders on a page OES does not control, and stops when revoked', async ({
    signedIn,
    browser,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('embed-flow');
    await createBucket(bucket);
    await upload(bucket, 'brand/logo.png', 'image/png', PNG);

    await page.goto(`${objectPath(bucket, 'brand/logo.png')}?tab=sharing`);
    await page.getByRole('button', { name: 'Create embed' }).click();

    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('Company website');
    await dialog.getByRole('button', { name: 'Create embed' }).click();

    const embedUrl = ((await dialog.locator('p.font-mono').textContent()) ?? '').trim();
    expect(embedUrl).toMatch(/\/e\/[A-Za-z0-9_-]{43}$/);
    // An embed is published on the storage endpoint, not on the console: a site
    // loading an asset must never have to reach the management plane.
    expect(embedUrl.startsWith(`${STORAGE_URL}/e/`)).toBe(true);
    expect(embedUrl).not.toContain(new URL(page.url()).host);
    // A snippet is generated for an image, and it is markup a page can paste.
    const snippet = ((await dialog.locator('pre').textContent()) ?? '').trim();
    expect(snippet).toContain('<img');
    expect(snippet).toContain(embedUrl);
    await dialog.getByRole('button', { name: 'Done' }).click();
    await expect(page.getByText('Company website')).toBeVisible();

    // A genuinely third-party page: its own markup, its own origin, and a
    // cross-origin image load straight from storage.
    const visitor = await browser.newContext();
    const visitorPage = await visitor.newPage();
    await visitorPage.setContent(`<html><body>${snippet}</body></html>`);
    const embedded = visitorPage.locator('img');
    await expect
      .poll(() => embedded.evaluate((element: HTMLImageElement) => element.naturalWidth))
      .toBeGreaterThan(0);

    // The bytes are what OES stored, with the right type and no sniffing.
    const direct = await visitorPage.request.get(embedUrl);
    expect(direct.status()).toBe(200);
    expect(direct.headers()['content-type']).toBe('image/png');
    expect(direct.headers()['x-content-type-options']).toBe('nosniff');
    expect(
      createHash('sha256')
        .update(await direct.body())
        .digest('hex'),
    ).toBe(createHash('sha256').update(PNG).digest('hex'));

    await page.getByRole('button', { name: /Actions for Company website/ }).click();
    await page.getByRole('menuitem', { name: 'Revoke embed' }).click();
    await page.getByRole('dialog').getByRole('button', { name: 'Revoke embed' }).click();
    await expect(page.getByText('Revoked', { exact: true })).toBeVisible();

    const afterRevocation = await visitorPage.request.get(embedUrl);
    expect(afterRevocation.status()).toBe(404);
    await visitor.close();
  });

  test('an origin-restricted embed serves the listed site and refuses others', async ({
    signedIn,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('embed-origins');
    await createBucket(bucket);
    await upload(bucket, 'restricted.png', 'image/png', PNG);

    await page.goto(`${objectPath(bucket, 'restricted.png')}?tab=sharing`);
    await page.getByRole('button', { name: 'Create embed' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('Partner site');
    await dialog.getByLabel('Origin to allow').fill('https://example.com');
    await dialog.getByRole('button', { name: 'Add' }).click();
    await expect(dialog.getByText('https://example.com')).toBeVisible();
    await dialog.getByRole('button', { name: 'Create embed' }).click();
    const embedUrl = ((await dialog.locator('p.font-mono').textContent()) ?? '').trim();
    await dialog.getByRole('button', { name: 'Done' }).click();

    await expect(page.getByText('1 allowed origin')).toBeVisible();

    const allowed = await page.request.get(embedUrl, {
      headers: { origin: 'https://example.com' },
    });
    expect(allowed.status()).toBe(200);
    // The grant names the stored origin rather than reflecting the request.
    expect(allowed.headers()['access-control-allow-origin']).toBe('https://example.com');
    expect(allowed.headers()['vary']).toContain('Origin');

    const denied = await page.request.get(embedUrl, { headers: { origin: 'https://evil.test' } });
    expect(denied.status()).toBe(403);
    expect(denied.headers()['access-control-allow-origin']).toBeUndefined();
  });

  test('an object that cannot render inline gets a download embed and no snippet', async ({
    signedIn,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('embed-refused');
    await createBucket(bucket);
    await upload(bucket, 'page.html', 'text/html', '<h1>not embeddable</h1>');

    await page.goto(`${objectPath(bucket, 'page.html')}?tab=sharing`);
    await page.getByRole('button', { name: 'Create embed' }).click();
    const dialog = page.getByRole('dialog');
    // The refusal is explained before the operator fills anything in.
    await expect(dialog.getByText(/cannot be rendered inline safely/)).toBeVisible();
    await dialog.getByLabel('Name').fill('Download only');
    await dialog.getByRole('button', { name: 'Create embed' }).click();

    const embedUrl = ((await dialog.locator('p.font-mono').textContent()) ?? '').trim();
    // No markup is offered for a format that has no safe element.
    await expect(dialog.locator('pre')).toHaveCount(0);
    await dialog.getByRole('button', { name: 'Done' }).click();

    const served = await page.request.get(embedUrl);
    expect(served.status()).toBe(200);
    expect(served.headers()['content-type']).toBe('application/octet-stream');
    expect(served.headers()['content-disposition']).toContain('attachment');
  });
});

test.describe('capability delivery', () => {
  test('a shared video seeks through real range requests', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('share-ranges');
    await createBucket(bucket);
    // A real MP4 signature so the declared type is corroborated by the bytes.
    const body = Buffer.concat([
      Buffer.from('00000018667479706d70343200000000mp42isom', 'hex'),
      Buffer.alloc(64 * 1024, 7),
    ]);
    await upload(bucket, 'clip.mp4', 'video/mp4', body);

    await page.goto(`${objectPath(bucket, 'clip.mp4')}?tab=sharing`);
    await page.getByRole('button', { name: 'Create share link' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('Screening');
    await dialog.getByRole('button', { name: 'Create link' }).click();
    const shareUrl = ((await dialog.locator('p.font-mono').textContent()) ?? '').trim();
    await dialog.getByRole('button', { name: 'Done' }).click();

    const contentUrl = `${shareUrl}/content`;
    const full = await page.request.get(contentUrl);
    expect(full.status()).toBe(200);
    expect(full.headers()['accept-ranges']).toBe('bytes');
    const complete = await full.body();

    const partial = await page.request.get(contentUrl, {
      headers: { range: 'bytes=1024-2047' },
    });
    // Without a genuine 206 and a Content-Range, a media element cannot seek.
    expect(partial.status()).toBe(206);
    expect(partial.headers()['content-range']).toBe(`bytes 1024-2047/${complete.length}`);
    expect(partial.headers()['content-length']).toBe('1024');
    const slice = await partial.body();
    expect(slice.length).toBe(1024);
    expect(slice.equals(complete.subarray(1024, 2048))).toBe(true);
  });

  test('the console preview path also carries ranges through to the browser', async ({
    signedIn,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('preview-ranges');
    await createBucket(bucket);
    const body = Buffer.concat([
      Buffer.from('00000018667479706d70343200000000mp42isom', 'hex'),
      Buffer.alloc(32 * 1024, 3),
    ]);
    await upload(bucket, 'clip.mp4', 'video/mp4', body);

    const previewUrl = `/api/oes/v1/buckets/${encodeURIComponent(bucket)}/object-preview/clip.mp4`;
    await page.goto(objectPath(bucket, 'clip.mp4'));
    const partial = await page.request.get(previewUrl, { headers: { range: 'bytes=0-99' } });
    expect(partial.status()).toBe(206);
    expect(partial.headers()['content-range']).toContain('bytes 0-99/');
    expect((await partial.body()).length).toBe(100);
  });

  test('a public share page is not indexable and carries its own strict policy', async ({
    signedIn,
    browser,
  }) => {
    const page = signedIn;
    const bucket = uniqueBucket('share-headers');
    await createBucket(bucket);
    await upload(bucket, 'note.txt', 'text/plain', 'hello');

    await page.goto(`${objectPath(bucket, 'note.txt')}?tab=sharing`);
    await page.getByRole('button', { name: 'Create share link' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('Headers');
    await dialog.getByRole('button', { name: 'Create link' }).click();
    const shareUrl = ((await dialog.locator('p.font-mono').textContent()) ?? '').trim();
    await dialog.getByRole('button', { name: 'Done' }).click();

    const visitor = await browser.newContext();
    const visitorPage = await visitor.newPage();
    const response = await visitorPage.goto(shareUrl);
    const headers = response?.headers() ?? {};
    expect(headers['x-robots-tag']).toContain('noindex');
    expect(headers['referrer-policy']).toBe('no-referrer');
    expect(headers['content-security-policy']).toContain("frame-ancestors 'none'");
    expect(headers['content-security-policy']).toContain("form-action 'none'");
    expect(headers['content-security-policy']).not.toContain('unsafe-eval');

    // A revocable link must not be cached anywhere along the way.
    const bytes = await visitorPage.request.get(`${shareUrl}/content`);
    expect(bytes.headers()['cache-control']).toContain('no-store');
    await visitor.close();
  });

  test('sharing management is usable on a narrow screen', async ({ signedIn }) => {
    const page = signedIn;
    const bucket = uniqueBucket('share-mobile');
    await createBucket(bucket);
    await upload(bucket, 'note.txt', 'text/plain', 'hello');

    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`${objectPath(bucket, 'note.txt')}?tab=sharing`);
    await expect(page.getByRole('button', { name: 'Create share link' })).toBeVisible();

    await page.getByRole('button', { name: 'Create share link' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel('Name').fill('Mobile');
    await dialog.getByRole('button', { name: 'Create link' }).click();
    await expect(dialog.getByRole('button', { name: 'Done' })).toBeVisible();
    await dialog.getByRole('button', { name: 'Done' }).click();

    // The page must not gain a horizontal scrollbar on a phone.
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(1);
  });
});
