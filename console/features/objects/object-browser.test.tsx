import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ObjectBrowser } from './object-browser';
import {
  auditorPermissions,
  errorBody,
  jsonResponse,
  renderWithProviders,
  session,
} from '@/test/render';
import type { ObjectListPage } from '@/types/api';

const push = vi.fn();
let searchParams = new URLSearchParams();

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push, replace: vi.fn() }),
  usePathname: () => '/buckets/uploads',
  useSearchParams: () => searchParams,
}));

function page(overrides: Partial<ObjectListPage> = {}): ObjectListPage {
  return {
    objects: [
      {
        key: 'documents/report.pdf',
        size: 2_048,
        content_type: 'application/pdf',
        etag: 'abc',
        checksum: 'sha256:deadbeef',
        version_id: 'v1',
        created_at: '2026-08-01T10:00:00Z',
        modified_at: '2026-08-02T10:00:00Z',
        custom_metadata: {},
      },
    ],
    prefixes: ['documents/'],
    is_truncated: false,
    next_continuation_token: null,
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  push.mockClear();
  searchParams = new URLSearchParams();
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('ObjectBrowser', () => {
  it('requests a delimited listing so keys group into logical folders', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);

    await screen.findByText('report.pdf');
    const url = String(fetchMock.mock.calls[0]?.[0]);
    expect(url).toContain('/api/record-store/v1/buckets/uploads/objects');
    expect(url).toContain('delimiter=%2F');
    expect(url).toContain('limit=50');
  });

  it('shows prefixes and objects as distinct kinds of entry', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);

    // A prefix navigates; it is not a downloadable object.
    expect(await screen.findByRole('button', { name: 'documents' })).toBeTruthy();
    expect(screen.getByRole('link', { name: /report\.pdf/ })).toBeTruthy();
  });

  it('navigates into a prefix and drops the stale cursor', async () => {
    searchParams = new URLSearchParams('cursor=oldcursor');
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);

    await userEvent.click(await screen.findByRole('button', { name: 'documents' }));
    expect(push).toHaveBeenCalledTimes(1);
    const target = String(push.mock.calls[0]?.[0]);
    expect(target).toContain('prefix=documents%2F');
    // The old cursor belongs to the previous listing and must not survive.
    expect(target).not.toContain('cursor');
  });

  it('pages forward only when the API returned a cursor', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page({ next_continuation_token: 'next-1' })));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');

    const next = screen.getByRole('button', { name: /next page/i });
    expect(next.hasAttribute('disabled')).toBe(false);
    await userEvent.click(next);
    expect(String(push.mock.calls[0]?.[0])).toContain('cursor=next-1');
  });

  it('sends the cursor from the URL, so a page survives a reload', async () => {
    searchParams = new URLSearchParams('cursor=page-2&limit=100');
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');

    const url = String(fetchMock.mock.calls[0]?.[0]);
    expect(url).toContain('continuation_token=page-2');
    expect(url).toContain('limit=100');
  });

  it('drops the cursor when the page size changes', async () => {
    searchParams = new URLSearchParams('cursor=page-2');
    fetchMock.mockResolvedValue(jsonResponse(page({ next_continuation_token: 'next-1' })));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');

    await userEvent.selectOptions(screen.getByLabelText(/rows per page/i), '100');

    // A cursor names a position within one page size, so it cannot be carried
    // over to a differently sized listing.
    const target = String(push.mock.calls[0]?.[0]);
    expect(target).toContain('limit=100');
    expect(target).not.toContain('cursor');
  });

  it('renders one server page without implying it is the whole bucket', async () => {
    const objects = Array.from({ length: 50 }, (_, index) => ({
      ...page().objects[0]!,
      key: `documents/file-${String(index).padStart(3, '0')}.bin`,
    }));
    fetchMock.mockResolvedValue(
      jsonResponse(
        page({ objects, prefixes: [], is_truncated: true, next_continuation_token: 'next-1' }),
      ),
    );
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('file-000.bin');

    // Exactly the fetched page is shown, and the row count is never presented
    // as a total: a bucket can hold millions of keys the console has not seen.
    const [, ...rows] = within(screen.getByRole('table')).getAllByRole('row');
    expect(rows).toHaveLength(50);
    expect(screen.queryByText(/50 objects|of 50|1-50/)).toBeNull();
    expect(screen.getByRole('button', { name: /next page/i }).hasAttribute('disabled')).toBe(false);
  });

  it('disables paging when the listing is complete', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');
    expect(screen.getByRole('button', { name: /next page/i }).hasAttribute('disabled')).toBe(true);
  });

  it('builds a download link straight to Record Store rather than proxying in the page', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');

    await userEvent.click(screen.getByRole('button', { name: /actions for report\.pdf/i }));
    const download = await screen.findByRole('menuitem', { name: /download/i });
    const href = download.querySelector('a')?.getAttribute('href') ?? download.getAttribute('href');
    expect(href).toBe('/api/record-store/v1/buckets/uploads/object-content/documents/report.pdf');
  });

  it('encodes each key segment so a key cannot alter the request path', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(
        page({
          objects: [{ ...page().objects[0]!, key: 'odd names/a b?c#d.txt' }],
          prefixes: [],
        }),
      ),
    );
    renderWithProviders(<ObjectBrowser bucket="uploads" />);

    const link = await screen.findByRole('link', { name: /a b\?c#d\.txt/ });
    const href = link.getAttribute('href') ?? '';
    expect(href).toContain('odd%20names/a%20b%3Fc%23d.txt');
    // The logical hierarchy survives; only the segments are escaped.
    expect(href.split('/objects/')[1]?.split('/').length).toBe(2);
  });

  it('hides upload and delete from a role that cannot change objects', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />, {
      session: session(auditorPermissions),
    });
    await screen.findByText('report.pdf');

    expect(screen.queryByText('Upload files')).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: /actions for report\.pdf/i }));
    expect(await screen.findByRole('menuitem', { name: /download/i })).toBeTruthy();
    expect(screen.queryByRole('menuitem', { name: /delete object/i })).toBeNull();
  });

  it('explains an empty prefix rather than showing a blank table', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page({ objects: [], prefixes: [] })));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    expect(await screen.findByText('This bucket is empty')).toBeTruthy();
  });

  it('requires confirmation before deleting an object', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');

    await userEvent.click(screen.getByRole('button', { name: /actions for report\.pdf/i }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /delete object/i }));

    const dialog = await screen.findByRole('dialog');
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'DELETE')).toBe(false);

    fetchMock.mockResolvedValueOnce(new Response(null, { status: 204 }));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Delete object' }));

    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'DELETE');
      expect(String(call?.[0])).toContain('/object/documents/report.pdf');
    });
  });

  it('copies an object server side rather than through the browser', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');

    await userEvent.click(screen.getByRole('button', { name: /actions for report\.pdf/i }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /copy to/i }));

    const dialog = await screen.findByRole('dialog');
    // The suggested key keeps the extension so the copy keeps its file type.
    expect((within(dialog).getByLabelText('Destination key') as HTMLInputElement).value).toBe(
      'documents/report-copy.pdf',
    );

    await userEvent.click(within(dialog).getByRole('button', { name: 'Copy object' }));
    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([url]) => String(url).includes('/object-copy/'));
      expect(call).toBeTruthy();
      expect(String(call?.[0])).toContain('/buckets/uploads/object-copy/documents/report-copy.pdf');
      expect(JSON.parse(String(call?.[1]?.body))).toEqual({
        source_bucket: 'uploads',
        source_key: 'documents/report.pdf',
      });
    });
  });

  it('refuses to copy an object over itself', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');

    await userEvent.click(screen.getByRole('button', { name: /actions for report\.pdf/i }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /copy to/i }));
    const dialog = await screen.findByRole('dialog');

    const key = within(dialog).getByLabelText('Destination key');
    await userEvent.clear(key);
    await userEvent.type(key, 'documents/report.pdf');

    expect(within(dialog).getByRole('alert').textContent).toMatch(/cannot be copied over itself/);
    expect(
      within(dialog).getByRole('button', { name: 'Copy object' }).hasAttribute('disabled'),
    ).toBe(true);
  });

  it('deletes a selection one key at a time and reports partial failure', async () => {
    const objects = ['a.txt', 'b.txt', 'c.txt'].map((key) => ({
      ...page().objects[0]!,
      key: `documents/${key}`,
    }));
    fetchMock.mockImplementation((url: string, init?: RequestInit) => {
      if (init?.method === 'DELETE') {
        return String(url).includes('b.txt')
          ? Promise.resolve(jsonResponse(errorBody('OBJECT_LOCKED', 'Object is locked', 'r1'), 409))
          : Promise.resolve(new Response(null, { status: 204 }));
      }
      return Promise.resolve(jsonResponse(page({ objects, prefixes: [] })));
    });

    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('a.txt');

    await userEvent.click(screen.getByRole('checkbox', { name: /select all objects/i }));
    expect(screen.getByText('3 selected')).toBeTruthy();

    await userEvent.click(screen.getByRole('button', { name: /delete selected/i }));

    // One request per key, because the API deletes a single key at a time.
    await waitFor(() => {
      const deletes = fetchMock.mock.calls.filter(([, init]) => init?.method === 'DELETE');
      expect(deletes).toHaveLength(3);
    });
    // The failure is named rather than the batch being reported as successful.
    const report = (await screen.findByText(/1 could not be deleted/)).closest('div');
    // Scoped to the report: b.txt also appears in the table behind it.
    expect(report?.textContent).toContain('b.txt');
    expect(report?.textContent).toContain('Object is locked');
    // The successful two are not listed as failures.
    expect(report?.textContent).not.toContain('a.txt');
  });

  it('drops a selection when the listing location changes', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />);
    await screen.findByText('report.pdf');

    await userEvent.click(screen.getByRole('checkbox', { name: /select report\.pdf/i }));
    expect(screen.getByText('1 selected')).toBeTruthy();

    // A selection made on one page must not survive into another.
    await userEvent.click(screen.getByRole('button', { name: 'documents' }));
    expect(screen.queryByText('1 selected')).toBeNull();
  });

  it('offers no selection or copy to a role that cannot change objects', async () => {
    fetchMock.mockResolvedValue(jsonResponse(page()));
    renderWithProviders(<ObjectBrowser bucket="uploads" />, {
      session: session(auditorPermissions),
    });
    await screen.findByText('report.pdf');

    expect(screen.queryByRole('checkbox')).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: /actions for report\.pdf/i }));
    expect(screen.queryByRole('menuitem', { name: /copy to/i })).toBeNull();
  });
});
