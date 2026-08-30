import { screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { StorageClassesScreen } from './storage-classes-screen';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { StoragePolicy } from '@/types/cluster';

function policy(overrides: Partial<StoragePolicy> = {}): StoragePolicy {
  return {
    class: 'standard',
    description: null,
    device_filter: { allowed_kinds: [] },
    durability: { strategy: 'replication', replicas: 3 },
    failure_domain: 'rack',
    strict_failure_domains: false,
    minimum_free_space_percent: 0,
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

function respond(policies: StoragePolicy[]) {
  fetchMock.mockImplementation(() => Promise.resolve(jsonResponse(policies)));
}

describe('StorageClassesScreen', () => {
  it('says an unfiltered class accepts any device rather than leaving it blank', async () => {
    // An empty cell reads as "nothing allowed", which is the opposite of what
    // an empty filter means.
    respond([policy()]);
    renderWithProviders(<StorageClassesScreen />);

    expect(await screen.findByText('Any kind')).toBeTruthy();
  });

  it('lists the device kinds a filtered class is restricted to', async () => {
    respond([policy({ device_filter: { allowed_kinds: ['nvme', 'sata_ssd'] } })]);
    renderWithProviders(<StorageClassesScreen />);

    expect(await screen.findByText('nvme')).toBeTruthy();
    expect(screen.getByText('sata_ssd')).toBeTruthy();
    expect(screen.queryByText('Any kind')).toBeNull();
  });

  it('marks a strict class, because strictness decides whether a write fails', async () => {
    respond([policy({ strict_failure_domains: true })]);
    renderWithProviders(<StorageClassesScreen />);

    expect(await screen.findByText('strict')).toBeTruthy();
  });

  it('describes durability in the terms the class was defined in', async () => {
    respond([
      policy({ class: 'hot', durability: { strategy: 'replication', replicas: 2 } }),
      policy({
        class: 'archive',
        durability: {
          strategy: 'erasure_coding',
          profile: { data_shards: 4, parity_shards: 2 },
        },
      }),
    ]);
    renderWithProviders(<StorageClassesScreen />);

    expect(await screen.findByText('2 copies')).toBeTruthy();
    expect(screen.getByText('4+2 erasure coding')).toBeTruthy();
  });

  it('shows a dash rather than zero when no space is reserved', async () => {
    respond([policy({ minimum_free_space_percent: 0 })]);
    renderWithProviders(<StorageClassesScreen />);

    expect(await screen.findByText('—')).toBeTruthy();
  });
});
