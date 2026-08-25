import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ObjectDetails } from './object-details';
import { auditorPermissions, jsonResponse, renderWithProviders, session } from '@/test/render';
import type { ObjectSummary, SharingSettings } from '@/types/api';

const push = vi.fn();
const replace = vi.fn();
let searchParams = new URLSearchParams();

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push, replace }),
  usePathname: () => '/buckets/records/objects/reports/annual report.pdf',
  useSearchParams: () => searchParams,
}));

const object: ObjectSummary = {
  key: 'reports/annual report.pdf',
  size: 4_096,
  content_type: 'application/pdf',
  etag: 'etag-1',
  checksum: 'sha256:abc123',
  version_id: 'version-1',
  created_at: '2026-08-20T10:00:00Z',
  modified_at: '2026-08-21T10:00:00Z',
  custom_metadata: { department: 'finance' },
};

const sharingSettings: SharingSettings = {
  shares_enabled: true,
  embeds_enabled: true,
  maximum_lifetime_days: 365,
  require_expiration: false,
  require_share_password: false,
  maximum_access_count: 10_000,
  minimum_password_length: 8,
  preview_text_limit_bytes: 1024 * 1024,
  embeddable_content_types: ['image/png', 'video/mp4', 'audio/mpeg'],
};

let fetchMock: ReturnType<typeof vi.fn>;

/** Answers every request this screen makes, so a test only states its own case. */
function respond(overrides: (url: string) => Response | null = () => null) {
  return (url: string) => {
    const custom = overrides(String(url));
    if (custom) return Promise.resolve(custom);
    if (String(url).includes('/v1/sharing/settings')) {
      return Promise.resolve(jsonResponse(sharingSettings));
    }
    if (String(url).includes('/object-shares/') || String(url).includes('/object-embeds/')) {
      return Promise.resolve(jsonResponse([]));
    }
    return Promise.resolve(jsonResponse(object));
  };
}

beforeEach(() => {
  push.mockClear();
  replace.mockClear();
  searchParams = new URLSearchParams();
  fetchMock = vi.fn().mockImplementation(respond());
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('ObjectDetails', () => {
  it('renders protocol metadata and an encoded streaming download URL', async () => {
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);

    await userEvent.click(await screen.findByRole('tab', { name: 'Overview' }));
    expect(await screen.findByText('sha256:abc123')).toBeTruthy();
    expect(screen.getByText('4096 bytes')).toBeTruthy();
    const download = screen.getByRole('link', { name: 'Download' });
    expect(download.getAttribute('href')).toBe(
      '/api/oes/v1/buckets/records/object-content/reports/annual%20report.pdf',
    );
  });

  it('opens on Preview for an object OES can render', async () => {
    // A screen that opens on a checksum table when the object is a document
    // makes its reader do the work.
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);
    const preview = await screen.findByRole('tab', { name: 'Preview' });
    await waitFor(() => expect(preview.getAttribute('aria-selected')).toBe('true'));
    const frame = await screen.findByTitle(/Preview of annual report\.pdf/);
    expect(frame.getAttribute('src')).toBe(
      '/api/oes/v1/buckets/records/object-preview/reports/annual%20report.pdf',
    );
  });

  it('opens on Overview and offers no Preview tab for an unrenderable object', async () => {
    fetchMock.mockImplementation(
      respond((url) =>
        url.includes('/object/') || url.includes('/object?')
          ? jsonResponse({ ...object, content_type: 'application/octet-stream' })
          : null,
      ),
    );
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);

    const overview = await screen.findByRole('tab', { name: 'Overview' });
    await waitFor(() => expect(overview.getAttribute('aria-selected')).toBe('true'));
    expect(screen.queryByRole('tab', { name: 'Preview' })).toBeNull();
    expect(await screen.findByText(/cannot be shown in the browser safely/)).toBeTruthy();
  });

  it('refuses to preview stored active content and says why', async () => {
    fetchMock.mockImplementation(
      respond((url) =>
        url.includes('/object/') ? jsonResponse({ ...object, content_type: 'text/html' }) : null,
      ),
    );
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);

    await screen.findByRole('tab', { name: 'Overview' });
    // No viewer is mounted for a format that can carry script, at all.
    expect(screen.queryByRole('tab', { name: 'Preview' })).toBeNull();
    expect(screen.queryByTitle(/Preview of/)).toBeNull();
  });

  it('targets the exact version when one is pinned, never the current one', async () => {
    searchParams = new URLSearchParams({ version: 'version-0' });
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);

    await screen.findByText('Historical version');
    await waitFor(() => {
      const metadata = fetchMock.mock.calls
        .map(([url]) => String(url))
        .find((url) => url.includes('/object/reports/'));
      expect(metadata).toContain('version_id=version-0');
    });
    const frame = await screen.findByTitle(/Preview of annual report\.pdf/);
    expect(frame.getAttribute('src')).toContain('version_id=version-0');
    // Every route out of this screen names the pinned version, including the
    // one inside the preview card: a reader looking at history who downloads
    // the current bytes has been handed the wrong file.
    for (const link of screen.getAllByRole('link', { name: 'Download' })) {
      expect(link.getAttribute('href')).toContain('version_id=version-0');
    }
  });

  it('keeps custom metadata on its own tab and renders it as text', async () => {
    fetchMock.mockImplementation(
      respond((url) =>
        url.includes('/object/')
          ? jsonResponse({
              ...object,
              custom_metadata: { note: '<img src=x onerror=alert(1)>' },
            })
          : null,
      ),
    );
    const { container } = renderWithProviders(
      <ObjectDetails bucket="records" objectKey={object.key} />,
    );
    await screen.findByRole('tab', { name: 'Metadata' });

    await userEvent.click(screen.getByRole('tab', { name: 'Metadata' }));
    expect(await screen.findByText('note')).toBeTruthy();
    // Stored metadata is caller-supplied and is exactly what an attacker fills
    // with markup. It must arrive as characters, never as elements.
    expect(screen.getByText('<img src=x onerror=alert(1)>')).toBeTruthy();
    expect(container.querySelector('img')).toBeNull();
  });

  it('explains an object stored without custom metadata', async () => {
    fetchMock.mockImplementation(
      respond((url) =>
        url.includes('/object/') ? jsonResponse({ ...object, custom_metadata: {} }) : null,
      ),
    );
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);
    await screen.findByRole('tab', { name: 'Metadata' });

    await userEvent.click(screen.getByRole('tab', { name: 'Metadata' }));
    expect(await screen.findByText('No custom metadata')).toBeTruthy();
  });

  it('verifies the object without claiming it can repair it', async () => {
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);
    await screen.findByRole('tab', { name: 'Integrity' });

    await userEvent.click(screen.getByRole('tab', { name: 'Integrity' }));
    const verify = await screen.findByRole('button', { name: 'Verify object' });
    // Verification proves a mismatch; it cannot rebuild bytes, so no repair
    // control is offered at the level of a single object.
    expect(screen.queryByRole('button', { name: /repair/i })).toBeNull();

    await userEvent.click(verify);
    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([url]) =>
        String(url).includes('/verify/objects/records/'),
      );
      expect(call).toBeTruthy();
      expect(call?.[1]?.method).toBe('POST');
    });
    expect(await screen.findByText(/match the recorded checksum/)).toBeTruthy();
  });

  it('lists storage activity for this key only', async () => {
    fetchMock.mockImplementation(
      respond((url) =>
        url.includes('/v1/events')
          ? jsonResponse({
              events: [
                {
                  id: 'e1',
                  type: 'object.created',
                  time: '2026-08-21T10:00:00Z',
                  bucket: 'records',
                  object: object.key,
                  version_id: 'version-1',
                  size: 4_096,
                  metadata: {},
                },
              ],
              next_time: null,
              next_id: null,
            })
          : null,
      ),
    );
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);
    await screen.findByRole('tab', { name: 'Activity' });

    await userEvent.click(screen.getByRole('tab', { name: 'Activity' }));
    expect(await screen.findByText('object.created')).toBeTruthy();
    // The request is scoped to this key rather than fetching the whole feed.
    const call = fetchMock.mock.calls.find(([url]) => String(url).includes('/v1/events'));
    expect(String(call?.[0])).toContain('prefix=reports%2Fannual+report.pdf');
  });

  it('offers sharing to a role that may share, and hides it from one that may not', async () => {
    const { unmount } = renderWithProviders(
      <ObjectDetails bucket="records" objectKey={object.key} />,
    );
    expect(await screen.findByRole('button', { name: 'Share' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Embed' })).toBeTruthy();
    unmount();

    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />, {
      session: session(auditorPermissions),
    });
    await screen.findByRole('tab', { name: 'Overview' });
    expect(screen.queryByRole('button', { name: 'Share' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Embed' })).toBeNull();
  });

  it('drops the Sharing tab entirely when the deployment has disabled it', async () => {
    fetchMock.mockImplementation(
      respond((url) =>
        url.includes('/v1/sharing/settings')
          ? jsonResponse({
              ...sharingSettings,
              shares_enabled: false,
              embeds_enabled: false,
            })
          : null,
      ),
    );
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);
    await screen.findByRole('tab', { name: 'Overview' });
    await waitFor(() => expect(screen.queryByRole('tab', { name: 'Sharing' })).toBeNull());
  });

  it('requires confirmation, deletes the exact key, and returns to its bucket', async () => {
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);
    await screen.findByRole('tab', { name: 'Overview' });
    await userEvent.click(screen.getByRole('button', { name: 'Delete' }));
    const dialog = await screen.findByRole('dialog');
    expect(
      within(dialog).getByText(/Share and embed links pointing at this object stop working/),
    ).toBeTruthy();

    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Delete object' }));

    await waitFor(() => {
      expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'DELETE')).toBe(true);
      expect(push).toHaveBeenCalledWith('/buckets/records');
    });
  });
});
