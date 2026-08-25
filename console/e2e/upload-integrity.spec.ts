/**
 * Regression coverage for a truncation bug around the 10 MiB boundary.
 *
 * A 40 MiB console upload once returned 201 Created after storing only the
 * first 10 MiB: `middleware.ts` was re-issuing the request through
 * `NextResponse.next({ request })`, which caps how large a body may be. The
 * fix excludes `/api/` from that middleware (see middleware.ts), but the bug
 * only ever showed up as a successful response with the wrong byte count, so
 * the only real protection is measuring stored bytes and checksums directly.
 *
 * Payloads are generated in memory rather than committed as fixtures.
 */
import { createHash, randomBytes } from 'node:crypto';
import { readFile } from 'node:fs/promises';

import { expect, MANAGEMENT_TOKEN, test, uniqueBucket } from './fixtures';

const MIB = 1024 * 1024;
const BOUNDARY = 10 * MIB;
const SIZES = [BOUNDARY - 1, BOUNDARY, BOUNDARY + 1, 40 * MIB];

const MANAGEMENT_URL = process.env.OES_E2E_MANAGEMENT_URL ?? 'http://127.0.0.1:47601';

function payload(size: number): { buffer: Buffer; checksum: string } {
  const buffer = randomBytes(size);
  return { buffer, checksum: `sha256:${createHash('sha256').update(buffer).digest('hex')}` };
}

/** Bucket creation via the real backend, so a UI click isn't repeated per size. */
async function createBucketDirect(bucket: string): Promise<void> {
  const response = await fetch(`${MANAGEMENT_URL}/api/v1/buckets`, {
    method: 'POST',
    headers: { authorization: `Bearer ${MANAGEMENT_TOKEN}`, 'content-type': 'application/json' },
    body: JSON.stringify({ name: bucket }),
  });
  if (!response.ok) throw new Error(`failed to create bucket ${bucket}: HTTP ${response.status}`);
}

/** Reads back what the backend actually persisted, independent of the console. */
async function fetchObjectDirect(
  bucket: string,
  key: string,
): Promise<{ size: number; checksum: string }> {
  const response = await fetch(
    `${MANAGEMENT_URL}/api/v1/buckets/${encodeURIComponent(bucket)}/object/${encodeURIComponent(key)}`,
    { headers: { authorization: `Bearer ${MANAGEMENT_TOKEN}` } },
  );
  if (!response.ok) throw new Error(`failed to read object ${key}: HTTP ${response.status}`);
  return response.json();
}

test.describe('upload integrity across the 10 MiB boundary', () => {
  /**
   * The only block in the suite that needs its own deadline.
   *
   * Every other spec clicks through a page. The largest case here moves 40 MiB
   * up through the console, 40 MiB back down, and hashes both — so the default
   * 60-second test timeout was not merely tight, it was unreachable: the
   * assertion below asked to wait 60 seconds for the upload alone, which the
   * test deadline could never grant. On a slow runner that surfaced as "test
   * timeout" rather than as anything about the bytes.
   *
   * Only the deadline moves. Every integrity assertion — the stored byte count,
   * the backend's checksum, and the checksum of what the console downloads —
   * stays exactly as strict, because those are what this file exists to protect.
   */
  test.describe.configure({ timeout: 300_000 });

  for (const size of SIZES) {
    test(`console upload stores the complete ${size}-byte object`, async ({ signedIn }) => {
      const page = signedIn;
      const bucket = uniqueBucket('integrity');
      await createBucketDirect(bucket);
      const { buffer, checksum } = payload(size);

      await page.goto(`/buckets/${bucket}`);
      await page.setInputFiles('input[type="file"]', {
        name: 'payload.bin',
        mimeType: 'application/octet-stream',
        buffer,
      });
      // Comfortably inside the block's deadline, so a stalled upload is
      // reported as a stalled upload rather than as an expired test.
      await expect(page.getByRole('link', { name: /payload\.bin/ })).toBeVisible({
        timeout: 120_000,
      });

      // Ground truth: what the backend actually stored, not what the console claims.
      const stored = await fetchObjectDirect(bucket, 'payload.bin');
      expect(stored.size).toBe(size);
      expect(stored.checksum).toBe(checksum);

      // The console's own download path must return the same complete object.
      const download = page.waitForEvent('download');
      await page.getByRole('button', { name: /actions for payload\.bin/i }).click();
      await page.getByRole('menuitem', { name: /download/i }).click();
      const file = await download;
      const downloadedPath = await file.path();
      expect(downloadedPath).not.toBeNull();
      const downloaded = await readFile(downloadedPath as string);
      expect(downloaded.length).toBe(size);
      expect(`sha256:${createHash('sha256').update(downloaded).digest('hex')}`).toBe(checksum);
    });
  }

  // Control: the direct backend path was never affected by the console bug, so
  // this isolates future regressions to the proxy/frontend rather than OES itself.
  test('direct backend upload stores the complete object (control)', async () => {
    const bucket = uniqueBucket('integrity-direct');
    await createBucketDirect(bucket);
    const size = 40 * MIB;
    const { buffer, checksum } = payload(size);

    const upload = await fetch(
      `${MANAGEMENT_URL}/api/v1/buckets/${encodeURIComponent(bucket)}/object/${encodeURIComponent('payload.bin')}`,
      {
        method: 'PUT',
        headers: { authorization: `Bearer ${MANAGEMENT_TOKEN}` },
        body: buffer,
      },
    );
    expect(upload.status).toBe(201);

    const stored = await fetchObjectDirect(bucket, 'payload.bin');
    expect(stored.size).toBe(size);
    expect(stored.checksum).toBe(checksum);
  });
});
