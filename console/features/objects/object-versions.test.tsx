import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ObjectVersions } from './object-versions';
import { auditorPermissions, jsonResponse, renderWithProviders, session } from '@/test/render';

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
  usePathname: () => '/buckets/uploads',
  useSearchParams: () => new URLSearchParams(),
}));

function entry(overrides: Record<string, unknown> = {}) {
  return {
    key: 'reports/q1.pdf',
    version_id: 'ver-11112222',
    size: 2_048,
    checksum: 'sha256:abc',
    etag: 'etag',
    created_at: '2026-08-20T10:00:00Z',
    is_latest: false,
    is_delete_marker: false,
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

function respond(entries: Record<string, unknown>[]) {
  fetchMock.mockImplementation((url: string, init?: RequestInit) => {
    if (init?.method === 'POST') {
      return Promise.resolve(jsonResponse({ key: 'reports/q1.pdf', version_id: 'ver-new' }, 201));
    }
    return Promise.resolve(
      jsonResponse({ versions: entries, next_key_marker: null, next_version_id_marker: null }),
    );
  });
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('ObjectVersions restore', () => {
  it('explains that restoring adds a version rather than rewriting history', async () => {
    respond([entry({ is_latest: true, version_id: 'ver-current' }), entry()]);
    renderWithProviders(<ObjectVersions bucket="uploads" />);
    await screen.findAllByText(/q1\.pdf/);

    await userEvent.click(screen.getAllByRole('button', { name: /actions/i })[1]!);
    await userEvent.click(await screen.findByRole('menuitem', { name: /restore as current/i }));

    const dialog = await screen.findByRole('dialog');
    expect(dialog.textContent).toMatch(/version history is not rewritten/i);
    expect(dialog.textContent).toMatch(/new current version is created/i);
  });

  it('restores nothing until the operator confirms', async () => {
    respond([entry({ is_latest: true, version_id: 'ver-current' }), entry()]);
    renderWithProviders(<ObjectVersions bucket="uploads" />);
    await screen.findAllByText(/q1\.pdf/);

    await userEvent.click(screen.getAllByRole('button', { name: /actions/i })[1]!);
    await userEvent.click(await screen.findByRole('menuitem', { name: /restore as current/i }));
    const dialog = await screen.findByRole('dialog');
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'POST')).toBe(false);

    await userEvent.click(within(dialog).getByRole('button', { name: 'Restore as current' }));
    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'POST');
      expect(String(call?.[0])).toContain('/v1/restore/uploads/');
    });
  });

  it('offers no restore for the current version or a delete marker', async () => {
    respond([
      entry({ is_latest: true, version_id: 'ver-current' }),
      entry({ version_id: 'ver-marker', is_delete_marker: true }),
    ]);
    renderWithProviders(<ObjectVersions bucket="uploads" />);
    await screen.findAllByText(/q1\.pdf/);

    // Restoring the current version is a no-op, and a delete marker has no
    // bytes to restore.
    for (const index of [0, 1]) {
      await userEvent.click(screen.getAllByRole('button', { name: /actions/i })[index]!);
      expect(screen.queryByRole('menuitem', { name: /restore as current/i })).toBeNull();
      await userEvent.keyboard('{Escape}');
    }
  });

  it('offers no restore to a role that cannot change objects', async () => {
    respond([entry({ is_latest: true, version_id: 'ver-current' }), entry()]);
    renderWithProviders(<ObjectVersions bucket="uploads" />, {
      session: session(auditorPermissions),
    });
    await screen.findAllByText(/q1\.pdf/);

    await userEvent.click(screen.getAllByRole('button', { name: /actions/i })[1]!);
    expect(screen.queryByRole('menuitem', { name: /restore as current/i })).toBeNull();
  });
});
