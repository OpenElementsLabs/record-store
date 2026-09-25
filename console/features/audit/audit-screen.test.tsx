import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { AuditScreen } from './audit-screen';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { AuditEvent } from '@/types/api';

const push = vi.fn();
let searchParams = new URLSearchParams();

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push, replace: vi.fn() }),
  usePathname: () => '/audit',
  useSearchParams: () => searchParams,
}));

function event(overrides: Partial<AuditEvent> = {}): AuditEvent {
  return {
    event_id: 'evt-1',
    timestamp: '2026-08-23T10:00:00Z',
    request_id: 'req-abcdef',
    principal: 'ingest',
    credential_id: 'cred-1',
    source_ip: '10.0.0.9',
    operation: 'PutObject',
    resource: 'uploads/report.pdf',
    result: 'success',
    metadata: {},
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  push.mockClear();
  searchParams = new URLSearchParams();
  fetchMock = vi
    .fn()
    .mockImplementation(() =>
      Promise.resolve(jsonResponse({ events: [event()], next_time: null, next_id: null })),
    );
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('AuditScreen filters', () => {
  it('sends source IP and request ID to the backend', async () => {
    searchParams = new URLSearchParams('source_ip=10.0.0.9&request_id=req-abcdef');
    renderWithProviders(<AuditScreen />);

    await screen.findByText('PutObject');
    const url = String(fetchMock.mock.calls[0]?.[0]);
    // Filtering happens on the server, over a bounded scan, not in the browser.
    expect(url).toContain('source_ip=10.0.0.9');
    expect(url).toContain('request_id=req-abcdef');
  });

  it('submits the new filters and drops the stale cursor', async () => {
    searchParams = new URLSearchParams('after_time=2026-08-01T00%3A00%3A00Z&after_id=evt-0');
    renderWithProviders(<AuditScreen />);
    await screen.findByText('PutObject');

    await userEvent.type(screen.getByLabelText('Source IP'), '10.0.0.9');
    await userEvent.click(screen.getByRole('button', { name: /apply|filter|search/i }));

    const target = String(push.mock.calls[0]?.[0]);
    expect(target).toContain('source_ip=10.0.0.9');
    // A changed filter invalidates the page position it was taken from.
    expect(target).not.toContain('after_time');
  });

  it('traces every event sharing a request ID from the detail drawer', async () => {
    renderWithProviders(<AuditScreen />);
    await userEvent.click(await screen.findByText('PutObject'));

    const dialog = await screen.findByRole('dialog');
    await userEvent.click(
      within(dialog).getByRole('button', { name: 'Show every event for this request' }),
    );

    expect(String(push.mock.calls[0]?.[0])).toContain('request_id=req-abcdef');
  });

  it('traces every event from an address', async () => {
    renderWithProviders(<AuditScreen />);
    await userEvent.click(await screen.findByText('PutObject'));

    const dialog = await screen.findByRole('dialog');
    await userEvent.click(
      within(dialog).getByRole('button', { name: 'Show every event from this address' }),
    );

    expect(String(push.mock.calls[0]?.[0])).toContain('source_ip=10.0.0.9');
  });

  it('offers no trace action for an event that recorded neither', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse({
          events: [event({ request_id: null, source_ip: null })],
          next_time: null,
          next_id: null,
        }),
      ),
    );
    renderWithProviders(<AuditScreen />);
    await userEvent.click(await screen.findByText('PutObject'));

    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).queryByRole('button', { name: /Show every event/ })).toBeNull();
  });
});

describe('AuditScreen honesty about what the server said', () => {
  it('shows an announced change as attempted rather than as a failure', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse({
          events: [event({ result: 'attempted' })],
          next_time: null,
          next_id: null,
          scan_truncated: false,
        }),
      ),
    );
    renderWithProviders(<AuditScreen />);

    await screen.findByText('PutObject');
    // An announcement is the record written *before* a change. Rendering it as
    // a failure would be the console asserting an outcome the server never
    // recorded — and an operation whose outcome is unknown is precisely the one
    // an operator needs to notice.
    expect(screen.getByText('Attempted')).toBeTruthy();
    expect(screen.queryByText('Failure')).toBeNull();
  });

  it('says so when the server stopped scanning before the end of the range', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse({
          events: [],
          next_time: '2026-08-23T09:00:00Z',
          next_id: 'evt-9',
          scan_truncated: true,
        }),
      ),
    );
    renderWithProviders(<AuditScreen />);

    // "No matches" and "the server gave up looking" are different answers, and
    // reading the second as the first hides activity.
    expect(await screen.findByText('No matches in this window')).toBeTruthy();
    expect(screen.getByRole('button', { name: /Next page/ })).not.toHaveProperty('disabled', true);
  });

  it('reports an exhausted range as genuinely empty', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse({ events: [], next_time: null, next_id: null, scan_truncated: false }),
      ),
    );
    renderWithProviders(<AuditScreen />);

    expect(await screen.findByText('No audit events')).toBeTruthy();
    expect(screen.getByRole('button', { name: /Next page/ })).toHaveProperty('disabled', true);
  });
});
