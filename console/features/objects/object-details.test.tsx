import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ObjectDetails } from './object-details';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { ObjectSummary } from '@/types/api';

const push = vi.fn();

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push, replace: vi.fn() }),
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

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  push.mockClear();
  fetchMock = vi.fn().mockResolvedValue(jsonResponse(object));
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('ObjectDetails', () => {
  it('renders protocol metadata and an encoded streaming download URL', async () => {
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);

    expect(await screen.findByText('sha256:abc123')).toBeTruthy();
    expect(screen.getByText('department')).toBeTruthy();
    expect(screen.getByText('finance')).toBeTruthy();
    expect(screen.getByText('4096 bytes')).toBeTruthy();
    const download = screen.getByRole('link', { name: 'Download' });
    expect(download.getAttribute('href')).toBe(
      '/api/oes/v1/buckets/records/object-content/reports/annual%20report.pdf',
    );
  });

  it('requires confirmation, deletes the exact key, and returns to its bucket', async () => {
    renderWithProviders(<ObjectDetails bucket="records" objectKey={object.key} />);
    await screen.findByText('sha256:abc123');
    await userEvent.click(screen.getByRole('button', { name: 'Delete' }));
    const dialog = await screen.findByRole('dialog');
    expect(fetchMock).toHaveBeenCalledTimes(1);

    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Delete object' }));

    await waitFor(() => {
      expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'DELETE')).toBe(true);
      expect(push).toHaveBeenCalledWith('/buckets/records');
    });
  });
});
