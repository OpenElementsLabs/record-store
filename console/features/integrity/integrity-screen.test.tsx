import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { IntegrityScreen } from './integrity-screen';
import {
  auditorPermissions,
  jsonResponse,
  renderWithProviders,
  session,
  systemInfo,
} from '@/test/render';
import type { StorageInspection } from '@/types/api';

function inspection(overrides: Partial<StorageInspection> = {}): StorageInspection {
  return {
    metadata_payloads_scanned: 1_200,
    data_payloads_scanned: 1_200,
    metadata_without_data: 0,
    data_without_metadata: 0,
    unknown_data_entries: 0,
    recognized_temporary_entries: 0,
    unknown_temporary_entries: 0,
    truncated: false,
    missing_payload_samples: [],
    orphan_payload_samples: [],
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

/** Answers the inspection call, and buckets for the verify form. */
function respond(scan: StorageInspection) {
  fetchMock.mockImplementation((url: string) => {
    if (String(url).includes('/storage/inspect')) return Promise.resolve(jsonResponse(scan));
    if (String(url).includes('/buckets')) return Promise.resolve(jsonResponse([]));
    return Promise.resolve(jsonResponse({}));
  });
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('IntegrityScreen', () => {
  it('reports a clean scan without implying action is needed', async () => {
    respond(inspection());
    renderWithProviders(<IntegrityScreen />);

    expect(await screen.findByText('No inconsistencies found')).toBeTruthy();
    expect(screen.getByText(/1,200 objects scanned/)).toBeTruthy();
  });

  it('separates reclaimable space from data loss', async () => {
    respond(inspection({ data_without_metadata: 4, orphan_payload_samples: ['a1', 'b2'] }));
    renderWithProviders(<IntegrityScreen />);

    // Orphans are space, not lost objects, and the headline must say so.
    expect(await screen.findByText('Reclaimable storage found')).toBeTruthy();
    expect(screen.getByText(/no object depends on them/)).toBeTruthy();
  });

  it('tells a standalone operator that a checksum cannot rebuild lost bytes', async () => {
    respond(inspection({ metadata_without_data: 3, missing_payload_samples: ['c3'] }));
    renderWithProviders(<IntegrityScreen />, { info: systemInfo({ mode: 'standalone' }) });

    expect(await screen.findByText('Objects are missing their payloads')).toBeTruthy();
    expect(screen.getByText(/single copy of each object/)).toBeTruthy();
    expect(screen.getByText(/restoring from a backup outside Record Store/)).toBeTruthy();
    // The misleading opposite must not appear.
    expect(document.body.textContent ?? '').not.toMatch(/may be recoverable from another replica/);
  });

  it('points a clustered operator at redundancy instead', async () => {
    respond(inspection({ metadata_without_data: 3 }));
    renderWithProviders(<IntegrityScreen />, {
      info: systemInfo({
        mode: 'cluster',
        capabilities: { ...systemInfo().capabilities, cluster: true },
      }),
    });

    expect(await screen.findByText(/may be recoverable from another replica/)).toBeTruthy();
    expect(document.body.textContent ?? '').not.toMatch(/single copy of each object/);
  });

  it('says when a scan only sampled the catalog', async () => {
    respond(inspection({ truncated: true }));
    renderWithProviders(<IntegrityScreen />);

    // Presenting a truncated scan as a total would overstate the result.
    expect(await screen.findByText(/a sample rather than a total/)).toBeTruthy();
  });

  it('never offers reclaim before a preview reports something to remove', async () => {
    respond(inspection({ data_without_metadata: 2 }));
    renderWithProviders(<IntegrityScreen />);
    await screen.findByText('Reclaimable storage found');

    expect(screen.getByRole('button', { name: 'Reclaim' }).hasAttribute('disabled')).toBe(true);
  });

  it('requires a preview and a confirmation before deleting orphans', async () => {
    respond(inspection({ data_without_metadata: 2 }));
    renderWithProviders(<IntegrityScreen />);
    await screen.findByText('Reclaimable storage found');

    fetchMock.mockImplementation((url: string, init?: RequestInit) => {
      if (String(url).includes('/storage/repair')) {
        const body = JSON.parse(String(init?.body)) as { dry_run: boolean };
        return Promise.resolve(
          jsonResponse({
            inspection: inspection({ data_without_metadata: 2 }),
            removed_orphan_payloads: body.dry_run ? 0 : 2,
            dry_run: body.dry_run,
          }),
        );
      }
      return Promise.resolve(jsonResponse(inspection({ data_without_metadata: 2 })));
    });

    await userEvent.click(screen.getByRole('button', { name: 'Preview' }));
    expect(await screen.findByText(/2 orphaned payloads would be removed/)).toBeTruthy();
    // The preview must not have deleted anything.
    const previewCall = fetchMock.mock.calls.find(([url]) =>
      String(url).includes('/storage/repair'),
    );
    expect(JSON.parse(String(previewCall?.[1]?.body)).dry_run).toBe(true);

    await userEvent.click(screen.getByRole('button', { name: 'Reclaim' }));
    const dialog = await screen.findByRole('dialog');
    expect(dialog.textContent).toMatch(/Objects and versions are not affected/);

    await userEvent.click(await screen.findByRole('button', { name: 'Reclaim', hidden: false }));
    await waitFor(() => {
      const applied = fetchMock.mock.calls
        .filter(([url]) => String(url).includes('/storage/repair'))
        .map(([, init]) => JSON.parse(String(init?.body)).dry_run);
      expect(applied).toContain(false);
    });
  });

  it('offers no reclaim controls to a role that cannot manage storage', async () => {
    respond(inspection({ data_without_metadata: 5 }));
    renderWithProviders(<IntegrityScreen />, { session: session(auditorPermissions) });

    await screen.findByText('Reclaimable storage found');
    expect(screen.queryByRole('button', { name: 'Preview' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Reclaim' })).toBeNull();
  });
});
