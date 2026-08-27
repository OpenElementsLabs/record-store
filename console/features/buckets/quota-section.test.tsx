import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { QuotaSection } from './quota-section';
import {
  auditorPermissions,
  errorBody,
  jsonResponse,
  renderWithProviders,
  session,
} from '@/test/render';
import type { Bucket, BucketQuota } from '@/types/api';

function bucket(quota: BucketQuota, overrides: Partial<Bucket> = {}): Bucket {
  return {
    id: 'b1',
    organization_id: 'org',
    name: 'uploads',
    created_at: '2026-08-01T10:00:00Z',
    versioning: 'disabled',
    quota,
    object_count: 120,
    logical_bytes: 4_000_000_000,
    version_count: 120,
    version_bytes: 4_000_000_000,
    multipart_bytes: 0,
    ...overrides,
  };
}

const unlimited: BucketQuota = { bytes: { mode: 'unlimited' }, objects: { mode: 'unlimited' } };

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn().mockResolvedValue(jsonResponse(bucket(unlimited)));
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

async function sentQuota(): Promise<BucketQuota> {
  const call = await waitFor(() => {
    const found = fetchMock.mock.calls.find(([, init]) => init?.method === 'PUT');
    expect(found).toBeTruthy();
    return found;
  });
  return (JSON.parse(String(call?.[1]?.body)) as { quota: BucketQuota }).quota;
}

describe('QuotaSection', () => {
  it('reports usage against a configured limit', () => {
    renderWithProviders(
      <QuotaSection
        record={bucket({
          bytes: { mode: 'limit', bytes: 8_000_000_000 },
          objects: { mode: 'limit', objects: 200 },
        })}
      />,
    );

    expect(screen.getByText(/4\.00 GB/)).toBeTruthy();
    expect(screen.getByText(/of 8\.00 GB/)).toBeTruthy();
    const bars = screen.getAllByRole('progressbar');
    expect(bars[0]?.getAttribute('aria-valuenow')).toBe('50');
    expect(bars[1]?.getAttribute('aria-valuenow')).toBe('60');
  });

  it('draws no progress bar when nothing is limited', () => {
    renderWithProviders(<QuotaSection record={bucket(unlimited)} />);

    // A bar would imply a threshold the deployment has not configured.
    expect(screen.queryByRole('progressbar')).toBeNull();
    expect(screen.getAllByText('No limit configured').length).toBe(2);
  });

  it('sends an unlimited quota for both limits', async () => {
    renderWithProviders(
      <QuotaSection
        record={bucket({
          bytes: { mode: 'limit', bytes: 8_000_000_000 },
          objects: { mode: 'limit', objects: 200 },
        })}
      />,
    );

    await userEvent.click(screen.getAllByRole('radio', { name: 'Unlimited' })[0]!);
    await userEvent.click(screen.getAllByRole('radio', { name: 'Unlimited' })[1]!);
    await userEvent.click(screen.getByRole('button', { name: 'Save quota' }));

    expect(await sentQuota()).toEqual({
      bytes: { mode: 'unlimited' },
      objects: { mode: 'unlimited' },
    });
  });

  it('converts a whole amount to an exact byte count', async () => {
    renderWithProviders(<QuotaSection record={bucket(unlimited)} />);

    await userEvent.click(screen.getAllByRole('radio', { name: 'Set a limit' })[0]!);
    await userEvent.type(screen.getByLabelText('Amount'), '5');
    await userEvent.selectOptions(screen.getByLabelText('Storage limit unit'), 'GB');
    await userEvent.click(screen.getByRole('button', { name: 'Save quota' }));

    const quota = await sentQuota();
    expect(quota.bytes).toEqual({ mode: 'limit', bytes: 5_000_000_000 });
  });

  it('round-trips a stored limit unchanged when it is not edited', async () => {
    // The stored value is not a whole number of GB, so a naive unit split would
    // send back a different byte count than Record Store has recorded.
    renderWithProviders(
      <QuotaSection
        record={bucket({
          bytes: { mode: 'limit', bytes: 1_500_000_000 },
          objects: { mode: 'unlimited' },
        })}
      />,
    );

    await userEvent.click(screen.getByRole('button', { name: 'Save quota' }));
    const quota = await sentQuota();
    expect(quota.bytes).toEqual({ mode: 'limit', bytes: 1_500_000_000 });
  });

  it('refuses a fractional amount before contacting the API', async () => {
    renderWithProviders(<QuotaSection record={bucket(unlimited)} />);

    await userEvent.click(screen.getAllByRole('radio', { name: 'Set a limit' })[0]!);
    await userEvent.type(screen.getByLabelText('Amount'), '1.5');
    await userEvent.click(screen.getByRole('button', { name: 'Save quota' }));

    expect(await screen.findByRole('alert')).toBeTruthy();
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'PUT')).toBe(false);
  });

  it('surfaces the backend refusal rather than a generic message', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(errorBody('QUOTA_BELOW_USAGE', 'Quota is below current usage', 'req-4'), 409),
    );
    renderWithProviders(<QuotaSection record={bucket(unlimited)} />);

    await userEvent.click(screen.getAllByRole('radio', { name: 'Set a limit' })[0]!);
    await userEvent.type(screen.getByLabelText('Amount'), '1');
    await userEvent.click(screen.getByRole('button', { name: 'Save quota' }));

    expect(await screen.findByText('Quota is below current usage')).toBeTruthy();
  });

  it('offers no editing to a role that cannot manage buckets', () => {
    renderWithProviders(<QuotaSection record={bucket(unlimited)} />, {
      session: session(auditorPermissions),
    });

    expect(screen.queryByRole('button', { name: 'Save quota' })).toBeNull();
    expect(screen.getByText(/does not permit changing quotas/)).toBeTruthy();
  });
});
