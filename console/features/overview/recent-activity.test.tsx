import { screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { RecentActivity } from './recent-activity';
import { jsonResponse, renderWithProviders, systemInfo } from '@/test/render';

afterEach(() => vi.unstubAllGlobals());

describe('RecentActivity', () => {
  it('renders real event identity, resource, and byte count', async () => {
    const fetch = vi.fn().mockResolvedValue(
      jsonResponse({
        events: [
          {
            id: 'event-1',
            type: 'object.created',
            time: '2026-08-22T12:00:00Z',
            bucket: 'reports',
            object: 'daily/result.csv',
            version_id: 'v1',
            size: 2_048,
            metadata: {},
          },
        ],
        next_time: null,
        next_id: null,
      }),
    );
    vi.stubGlobal('fetch', fetch);
    renderWithProviders(<RecentActivity />);

    expect(await screen.findByText('object.created')).toBeTruthy();
    expect(screen.getByText('reports/daily/result.csv')).toBeTruthy();
    expect(screen.getByText('2.05 kB')).toBeTruthy();
    expect(String(fetch.mock.calls[0]?.[0])).toContain('/api/record-store/v1/events?limit=8');
  });

  it('does not fetch or render when events are unsupported', () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    renderWithProviders(<RecentActivity />, {
      info: systemInfo({ capabilities: { ...systemInfo().capabilities, events: false } }),
    });

    expect(screen.queryByText('Recent activity')).toBeNull();
    expect(fetch).not.toHaveBeenCalled();
  });
});
