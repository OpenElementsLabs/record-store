import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';

import { expect, MANAGEMENT_TOKEN, test, uniqueBucket } from './fixtures';

const endpoint = process.env.RECORD_STORE_E2E_MANAGEMENT_URL ?? 'http://127.0.0.1:47601';
async function api(path: string, init: RequestInit = {}) {
  const response = await fetch(`${endpoint}/api/v1${path}`, {
    ...init,
    headers: { authorization: `Bearer ${MANAGEMENT_TOKEN}`, ...init.headers },
  });
  expect(response.ok, `${init.method ?? 'GET'} ${path}: ${response.status}`).toBeTruthy();
  return response;
}

async function fixture() {
  const bucket = uniqueBucket('product');
  await api('/buckets', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ name: bucket }),
  });
  await api(`/buckets/${bucket}/versioning`, {
    method: 'PUT',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ versioning: 'enabled' }),
  });
  return bucket;
}

test('history pages beyond 100 versions and keeps neighbouring keys out', async ({
  signedIn: page,
}) => {
  const bucket = await fixture();
  for (let i = 0; i < 101; i += 1)
    await api(`/buckets/${bucket}/object/record.txt`, { method: 'PUT', body: `version ${i}` });
  await api(`/buckets/${bucket}/object/record.txt.backup`, { method: 'PUT', body: 'neighbour' });
  await page.goto(`/buckets/${bucket}/objects/record.txt?tab=versions`);
  await expect(page.getByRole('row')).toHaveCount(101);
  await page.getByRole('button', { name: 'Next page' }).click();
  await expect(page).toHaveURL(/vkey=/);
  await expect(page.getByRole('row')).toHaveCount(2);
  await expect(page.getByRole('cell', { name: 'record.txt.backup', exact: true })).toHaveCount(0);
  await page.getByRole('button', { name: 'First page' }).click();
  await expect(page.getByRole('row')).toHaveCount(101);
});

test('historical proof downloads bind to matching bytes on a narrow keyboard-accessible view', async ({
  signedIn: page,
}, testInfo) => {
  const bucket = await fixture();
  const original = 'historical bytes';
  const first = await (
    await api(`/buckets/${bucket}/object/record.txt`, { method: 'PUT', body: original })
  ).json();
  await api(`/buckets/${bucket}/object/record.txt`, { method: 'PUT', body: 'new bytes' });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(
    `/buckets/${bucket}/objects/record.txt?version=${first.version_id}&tab=integrity`,
  );
  await expect(page.getByRole('button', { name: 'Verify object', exact: true })).toHaveCount(0);
  const proofButton = page.getByRole('button', { name: 'Download proof bundle' });
  await proofButton.focus();
  await expect(proofButton).toBeFocused();
  const proofDownload = page.waitForEvent('download');
  await page.keyboard.press('Enter');
  const file = await proofDownload;
  const bundle = JSON.parse(await readFile((await file.path())!, 'utf8'));
  expect(bundle.object.version_id).toBe(first.version_id);
  expect(bundle.payload.sha256).toBe(createHash('sha256').update(original).digest('hex'));
  expect(bundle.history.status).toBe('unavailable');
  const bytesDownload = page.waitForEvent('download');
  await page.getByRole('link', { name: 'Download matching version' }).click();
  const bytes = await bytesDownload;
  expect(await readFile((await bytes.path())!, 'utf8')).toBe(original);
  await expect(page.getByText(/Establish signer identity/)).toBeVisible();
  await page.screenshot({
    path: testInfo.outputPath('historical-proof-mobile.png'),
    fullPage: true,
  });
});
