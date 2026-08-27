import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SharingTab } from './sharing-tab';
import { auditorPermissions, jsonResponse, renderWithProviders, session } from '@/test/render';
import type { EmbedLink, ShareLink, SharingSettings } from '@/types/api';

vi.mock('sonner', () => ({
  toast: Object.assign(vi.fn(), {
    success: vi.fn(),
    error: vi.fn(),
    message: vi.fn(),
  }),
}));

const settings: SharingSettings = {
  shares_enabled: true,
  embeds_enabled: true,
  maximum_lifetime_days: 365,
  require_expiration: false,
  require_share_password: false,
  maximum_access_count: 10_000,
  minimum_password_length: 8,
  preview_text_limit_bytes: 1024 * 1024,
  embeddable_content_types: ['image/png'],
};

const activeShare: ShareLink = {
  id: 'share-1',
  label: 'Board review',
  bucket: 'records',
  key: 'reports/annual.pdf',
  version_mode: 'current',
  version_id: null,
  permission: 'view_and_download',
  status: 'active',
  password_protected: true,
  created_by: 'management:system-administrator',
  created_at: '2026-08-01T10:00:00Z',
  expires_at: '2026-08-31T10:00:00Z',
  revoked_at: null,
  last_accessed_at: '2026-08-10T10:00:00Z',
  access_count: 12,
  maximum_access_count: null,
};

const revokedShare: ShareLink = {
  ...activeShare,
  id: 'share-2',
  label: 'Old link',
  status: 'revoked',
  revoked_at: '2026-08-11T10:00:00Z',
  password_protected: false,
};

const activeEmbed: EmbedLink = {
  id: 'embed-1',
  label: 'Company website',
  bucket: 'records',
  key: 'reports/annual.pdf',
  version_mode: 'pinned',
  version_id: 'version-9',
  status: 'active',
  content_type: 'image/png',
  disposition: 'inline',
  allowed_origins: ['https://example.com'],
  created_by: 'management:system-administrator',
  created_at: '2026-08-01T10:00:00Z',
  updated_at: null,
  expires_at: null,
  revoked_at: null,
  last_accessed_at: null,
  access_count: 0,
};

let fetchMock: ReturnType<typeof vi.fn>;

function respond(
  options: {
    readonly shares?: readonly ShareLink[];
    readonly embeds?: readonly EmbedLink[];
    readonly override?: (url: string, init?: RequestInit) => Response | null;
  } = {},
) {
  return (url: string, init?: RequestInit) => {
    const custom = options.override?.(String(url), init);
    if (custom) return Promise.resolve(custom);
    if (String(url).includes('/v1/sharing/settings'))
      return Promise.resolve(jsonResponse(settings));
    if (String(url).includes('/object-shares/')) {
      return Promise.resolve(jsonResponse(options.shares ?? []));
    }
    if (String(url).includes('/object-embeds/')) {
      return Promise.resolve(jsonResponse(options.embeds ?? []));
    }
    return Promise.resolve(jsonResponse({}));
  };
}

beforeEach(() => {
  fetchMock = vi.fn().mockImplementation(respond());
  vi.stubGlobal('fetch', fetchMock);
  vi.stubGlobal('navigator', {
    ...globalThis.navigator,
    clipboard: { writeText: vi.fn().mockResolvedValue(undefined) },
  });
});

afterEach(() => vi.unstubAllGlobals());

function renderTab(overrides: Parameters<typeof respond>[0] = {}, options = {}) {
  fetchMock.mockImplementation(respond(overrides));
  return renderWithProviders(
    <SharingTab bucket="records" objectKey="reports/annual.pdf" contentType="application/pdf" />,
    options,
  );
}

describe('SharingTab', () => {
  it('separates links for people from links for applications', async () => {
    renderTab({ shares: [activeShare], embeds: [activeEmbed] });

    // Merging the two would make "revoke everything the marketing site uses"
    // a reading exercise.
    expect(await screen.findByText('Share links')).toBeTruthy();
    expect(await screen.findByText('Embeds')).toBeTruthy();
    expect(await screen.findByText('Board review')).toBeTruthy();
    expect(await screen.findByText('Company website')).toBeTruthy();
    expect(screen.getByText(/For people/)).toBeTruthy();
    expect(screen.getByText(/For websites and applications/)).toBeTruthy();
  });

  it('states every status with a word, not only a colour', async () => {
    renderTab({ shares: [activeShare, revokedShare], embeds: [activeEmbed] });

    // Two links are active, and each says so in words as well as in colour.
    expect(await screen.findAllByText('Active')).toHaveLength(2);
    expect(await screen.findByText('Revoked')).toBeTruthy();
    expect(await screen.findByText('Password protected')).toBeTruthy();
    expect((await screen.findAllByText('Current version')).length).toBeGreaterThan(0);
    expect(await screen.findByText('Pinned version')).toBeTruthy();
    expect(await screen.findByText('1 allowed origin')).toBeTruthy();
  });

  it('warns plainly when an embed restricts nothing', async () => {
    renderTab({ embeds: [{ ...activeEmbed, allowed_origins: [] }] });
    expect(await screen.findByText('Any origin')).toBeTruthy();
  });

  it('lists capabilities without ever fetching a token', async () => {
    renderTab({ shares: [activeShare], embeds: [activeEmbed] });
    await screen.findByText('Board review');

    // Reading a list must not put a live capability anywhere. The URL is
    // fetched only when someone copies it.
    expect(fetchMock.mock.calls.some(([url]) => String(url).endsWith('/url'))).toBe(false);
  });

  it('fetches a share URL only when it is copied', async () => {
    renderTab({
      shares: [activeShare],
      override: (url) =>
        url.includes('/v1/shares/share-1/url')
          ? jsonResponse({ url: 'https://record-store.example.com/s/AbCdEf', available: true })
          : null,
    });
    await screen.findByText('Board review');

    await userEvent.click(screen.getByRole('button', { name: 'Copy' }));

    await waitFor(() => {
      expect(
        fetchMock.mock.calls.some(([url]) => String(url).includes('/shares/share-1/url')),
      ).toBe(true);
    });
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith(
      'https://record-store.example.com/s/AbCdEf',
    );
  });

  it('says so when a link can no longer be displayed instead of showing nothing', async () => {
    const { toast } = await import('sonner');
    renderTab({
      shares: [activeShare],
      override: (url) =>
        url.includes('/v1/shares/share-1/url')
          ? jsonResponse({ url: null, available: false })
          : null,
    });
    await screen.findByText('Board review');

    await userEvent.click(screen.getByRole('button', { name: 'Copy' }));
    await waitFor(() => {
      expect(vi.mocked(toast.error)).toHaveBeenCalledWith(
        expect.stringContaining('can no longer be displayed'),
      );
    });
  });

  it('is honest that revocation cannot recall bytes already downloaded', async () => {
    renderTab({ shares: [activeShare] });
    await screen.findByText('Board review');

    await userEvent.click(screen.getByRole('button', { name: /Actions for Board review/ }));
    await userEvent.click(await screen.findByRole('menuitem', { name: 'Revoke link' }));

    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/keeps their copy/)).toBeTruthy();

    fetchMock.mockImplementationOnce(() => Promise.resolve(jsonResponse(revokedShare)));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Revoke link' }));
    await waitFor(() => {
      expect(
        fetchMock.mock.calls.some(
          ([url, init]) =>
            String(url).includes('/shares/share-1/revoke') &&
            (init as RequestInit | undefined)?.method === 'POST',
        ),
      ).toBe(true);
    });
  });

  it('offers deletion only once a link is already inert', async () => {
    renderTab({ shares: [activeShare, revokedShare] });
    await screen.findByText('Old link');

    await userEvent.click(screen.getByRole('button', { name: /Actions for Board review/ }));
    expect(await screen.findByRole('menuitem', { name: 'Revoke link' })).toBeTruthy();
    expect(screen.queryByRole('menuitem', { name: 'Delete record' })).toBeNull();
    await userEvent.keyboard('{Escape}');

    await userEvent.click(screen.getByRole('button', { name: /Actions for Old link/ }));
    expect(await screen.findByRole('menuitem', { name: 'Delete record' })).toBeTruthy();
    expect(screen.queryByRole('menuitem', { name: 'Revoke link' })).toBeNull();
  });

  it('offers no creation or withdrawal to a role that may not share', async () => {
    renderTab({ shares: [activeShare] }, { session: session(auditorPermissions) });
    await screen.findByText('Board review');

    expect(screen.queryByRole('button', { name: /Create share link/ })).toBeNull();
    expect(screen.queryByRole('button', { name: /Create embed/ })).toBeNull();
    expect(screen.queryByRole('button', { name: /Actions for Board review/ })).toBeNull();
  });

  it('explains an object with nothing shared yet', async () => {
    renderTab();
    expect(await screen.findByText('No share links')).toBeTruthy();
    expect(await screen.findByText('No embeds')).toBeTruthy();
  });
});
