import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { BucketsScreen } from './buckets-screen';
import {
  auditorPermissions,
  errorBody,
  jsonResponse,
  renderWithProviders,
  session,
} from '@/test/render';
import type { Bucket } from '@/types/api';

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
  usePathname: () => '/buckets',
  useSearchParams: () => new URLSearchParams(),
}));

function bucket(overrides: Partial<Bucket> = {}): Bucket {
  return {
    id: 'b1',
    organization_id: 'org',
    name: 'uploads',
    created_at: '2026-08-01T10:00:00Z',
    versioning: 'disabled',
    quota: { bytes: { mode: 'unlimited' }, objects: { mode: 'unlimited' } },
    object_count: 12,
    logical_bytes: 2_500_000,
    version_count: 12,
    version_bytes: 2_500_000,
    multipart_bytes: 0,
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('BucketsScreen', () => {
  it('renders bucket accounting from a single request', async () => {
    fetchMock.mockResolvedValue(jsonResponse([bucket(), bucket({ id: 'b2', name: 'reports' })]));
    renderWithProviders(<BucketsScreen />);

    expect(await screen.findByText('uploads')).toBeTruthy();
    expect(screen.getByText('reports')).toBeTruthy();
    // Accounting comes with the list, so no follow-up request per bucket.
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(screen.getAllByText('2.50 MB').length).toBeGreaterThan(0);
  });

  it('shows an actionable empty state with a create action', async () => {
    fetchMock.mockResolvedValue(jsonResponse([]));
    renderWithProviders(<BucketsScreen />);

    expect(await screen.findByText('No buckets yet')).toBeTruthy();
    expect(screen.getAllByRole('button', { name: /create bucket/i }).length).toBeGreaterThan(0);
  });

  it('surfaces the API error code and request id rather than a generic message', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(errorBody('INTERNAL_ERROR', 'An internal error occurred', 'req-99'), 500),
    );
    renderWithProviders(<BucketsScreen />);

    expect(await screen.findByText('An internal error occurred')).toBeTruthy();
    await userEvent.click(screen.getByText('Details'));
    expect(screen.getByText('INTERNAL_ERROR')).toBeTruthy();
    expect(screen.getByText('req-99')).toBeTruthy();
  });

  it('reports an unreachable API distinctly from a rejected request', async () => {
    fetchMock.mockRejectedValue(new TypeError('Failed to fetch'));
    renderWithProviders(<BucketsScreen />);

    // The heading names the condition; the body repeats it with the product
    // name, so both nodes legitimately match a loose query.
    expect(await screen.findByText('The management API is unreachable')).toBeTruthy();
    expect(screen.getByText('The OES management API is unreachable.')).toBeTruthy();
  });

  it('creates a bucket and refreshes the list', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse([]))
      .mockResolvedValueOnce(jsonResponse(bucket({ name: 'new-bucket' }), 201))
      .mockResolvedValue(jsonResponse([bucket({ name: 'new-bucket' })]));

    renderWithProviders(<BucketsScreen />);
    await screen.findByText('No buckets yet');

    const openButtons = screen.getAllByRole('button', { name: /create bucket/i });
    await userEvent.click(openButtons[0]!);

    const dialog = await screen.findByRole('dialog');
    await userEvent.type(within(dialog).getByLabelText('Bucket name'), 'new-bucket');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create bucket' }));

    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'POST');
      expect(call).toBeTruthy();
      expect(JSON.parse(String(call?.[1]?.body))).toEqual({ name: 'new-bucket' });
    });
  });

  it('rejects an invalid bucket name before contacting the API', async () => {
    fetchMock.mockResolvedValue(jsonResponse([]));
    renderWithProviders(<BucketsScreen />);
    await screen.findByText('No buckets yet');

    await userEvent.click(screen.getAllByRole('button', { name: /create bucket/i })[0]!);
    const dialog = await screen.findByRole('dialog');
    await userEvent.type(within(dialog).getByLabelText('Bucket name'), 'AB');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create bucket' }));

    expect(await within(dialog).findByRole('alert')).toBeTruthy();
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'POST')).toBe(false);
  });

  it('requires confirmation before deleting and shows the backend refusal', async () => {
    fetchMock
      .mockResolvedValueOnce(jsonResponse([bucket()]))
      .mockResolvedValueOnce(
        jsonResponse(errorBody('BUCKET_NOT_EMPTY', 'Bucket is not empty', 'req-7'), 409),
      );

    renderWithProviders(<BucketsScreen />);
    await screen.findByText('uploads');

    await userEvent.click(screen.getByRole('button', { name: /actions for uploads/i }));
    await userEvent.click(await screen.findByText('Delete bucket'));

    const dialog = await screen.findByRole('dialog');
    // Nothing is deleted until the operator confirms.
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'DELETE')).toBe(false);

    await userEvent.click(within(dialog).getByRole('button', { name: 'Delete bucket' }));
    expect(await within(dialog).findByText('Bucket is not empty')).toBeTruthy();
  });

  it('offers no destructive actions to a role that cannot perform them', async () => {
    fetchMock.mockResolvedValue(jsonResponse([bucket()]));
    renderWithProviders(<BucketsScreen />, { session: session(auditorPermissions) });

    await screen.findByText('uploads');
    expect(screen.queryByRole('button', { name: /create bucket/i })).toBeNull();

    await userEvent.click(screen.getByRole('button', { name: /actions for uploads/i }));
    expect(await screen.findByText('Browse objects')).toBeTruthy();
    expect(screen.queryByText('Delete bucket')).toBeNull();
  });
});
