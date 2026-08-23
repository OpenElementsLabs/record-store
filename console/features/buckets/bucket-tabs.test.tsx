import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { BucketDetail, resourceReachesBucket } from './bucket-detail';
import {
  auditorPermissions,
  jsonResponse,
  renderWithProviders,
  session,
  systemInfo,
} from '@/test/render';
import type { Bucket, Policy } from '@/types/api';

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
  usePathname: () => '/buckets/uploads',
  useSearchParams: () => new URLSearchParams(),
}));

function bucket(overrides: Partial<Bucket> = {}): Bucket {
  return {
    id: 'b1',
    organization_id: 'org',
    name: 'uploads',
    created_at: '2026-08-01T10:00:00Z',
    versioning: 'enabled',
    quota: { bytes: { mode: 'limit', bytes: 8_000_000_000 }, objects: { mode: 'unlimited' } },
    object_count: 120,
    logical_bytes: 4_000_000_000,
    version_count: 130,
    version_bytes: 4_500_000_000,
    multipart_bytes: 1_000,
    ...overrides,
  };
}

function policy(name: string, resources: string[]): Policy {
  return {
    id: `p-${name}`,
    name,
    description: '',
    statements: [{ effect: 'allow', actions: ['s3:GetObject'], resources }],
    created_at: '2026-08-01T00:00:00Z',
    updated_at: '2026-08-01T00:00:00Z',
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

function respond(options: { policies?: Policy[]; events?: unknown[] } = {}) {
  fetchMock.mockImplementation((url: string) => {
    const target = String(url);
    if (target.includes('/v1/policies')) {
      return Promise.resolve(jsonResponse(options.policies ?? []));
    }
    if (target.includes('/v1/events')) {
      return Promise.resolve(
        jsonResponse({ events: options.events ?? [], next_time: null, next_id: null }),
      );
    }
    if (target.includes('/lifecycle')) return Promise.resolve(jsonResponse([]));
    if (target.includes('/objects')) {
      return Promise.resolve(
        jsonResponse({
          objects: [],
          prefixes: [],
          is_truncated: false,
          next_continuation_token: null,
        }),
      );
    }
    return Promise.resolve(jsonResponse([bucket()]));
  });
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('resourceReachesBucket', () => {
  it('matches the bucket itself and keys inside it', () => {
    expect(resourceReachesBucket('bucket:uploads', 'uploads')).toBe(true);
    expect(resourceReachesBucket('bucket:uploads/reports/q1.pdf', 'uploads')).toBe(true);
    expect(resourceReachesBucket('bucket:uploads/*', 'uploads')).toBe(true);
  });

  it('matches a wildcard that spans buckets', () => {
    expect(resourceReachesBucket('bucket:*', 'uploads')).toBe(true);
    expect(resourceReachesBucket('bucket:up*', 'uploads')).toBe(true);
  });

  it('does not match a different bucket that merely shares a prefix', () => {
    // `uploads-archive` is a different bucket; a pattern for `uploads` must not
    // be reported as reaching it, or the access view would overstate exposure.
    expect(resourceReachesBucket('bucket:uploads', 'uploads-archive')).toBe(false);
    expect(resourceReachesBucket('bucket:uploads/*', 'reports')).toBe(false);
  });

  it('ignores anything that is not a bucket resource', () => {
    expect(resourceReachesBucket('*', 'uploads')).toBe(false);
    expect(resourceReachesBucket('uploads/*', 'uploads')).toBe(false);
  });
});

describe('BucketDetail tabs', () => {
  it('shows configuration and accounting on the overview tab', async () => {
    respond();
    renderWithProviders(<BucketDetail bucket="uploads" />);
    // Objects is the landing tab, so Overview is reached deliberately.
    await userEvent.click(await screen.findByRole('tab', { name: 'Overview' }));

    expect(await screen.findByText('8.00 GB')).toBeTruthy();
    expect(screen.getByText('Enabled')).toBeTruthy();
    expect(screen.getByText('Unlimited')).toBeTruthy();
    expect(screen.getByText(/130 \(4\.50 GB\)/)).toBeTruthy();
  });

  it('lists only the policies that can reach this bucket', async () => {
    respond({
      policies: [
        policy('uploads-readers', ['bucket:uploads/*']),
        policy('everything', ['bucket:*']),
        policy('other-bucket', ['bucket:reports/*']),
      ],
    });
    renderWithProviders(<BucketDetail bucket="uploads" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Access' }));

    expect(await screen.findByText('uploads-readers')).toBeTruthy();
    expect(screen.getByText('everything')).toBeTruthy();
    // A policy scoped to another bucket is not access to this one.
    expect(screen.queryByText('other-bucket')).toBeNull();
  });

  it('explains when no policy reaches the bucket', async () => {
    respond({ policies: [policy('other', ['bucket:reports/*'])] });
    renderWithProviders(<BucketDetail bucket="uploads" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Access' }));

    expect(await screen.findByText('No policy reaches this bucket')).toBeTruthy();
  });

  it('scopes activity to this bucket rather than the whole feed', async () => {
    respond({
      events: [
        {
          id: 'e1',
          type: 'object.created',
          time: '2026-08-22T10:00:00Z',
          bucket: 'uploads',
          object: 'reports/q1.pdf',
          version_id: 'v1',
          size: 10,
          metadata: {},
        },
      ],
    });
    renderWithProviders(<BucketDetail bucket="uploads" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Activity' }));

    expect(await screen.findByText('object.created')).toBeTruthy();
    const call = fetchMock.mock.calls.find(([url]) => String(url).includes('/v1/events'));
    expect(String(call?.[0])).toContain('bucket=uploads');
  });

  it('states the cost of verifying a whole bucket before running it', async () => {
    respond();
    renderWithProviders(<BucketDetail bucket="uploads" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Integrity' }));

    expect(await screen.findByText(/Re-reads and re-hashes every object/)).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Verify bucket' })).toBeTruthy();
  });

  it('says a checksum cannot repair what it detects', async () => {
    respond();
    renderWithProviders(<BucketDetail bucket="uploads" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Integrity' }));

    fetchMock.mockImplementation(() =>
      Promise.resolve(jsonResponse({ verified_objects: 120, failures: 3 })),
    );
    await userEvent.click(screen.getByRole('button', { name: 'Verify bucket' }));

    expect(await screen.findByText(/it cannot repair it/)).toBeTruthy();
  });

  it('offers no verification to a role that cannot manage storage', async () => {
    respond();
    renderWithProviders(<BucketDetail bucket="uploads" />, {
      session: session(auditorPermissions),
    });
    await userEvent.click(await screen.findByRole('tab', { name: 'Integrity' }));

    expect(screen.queryByRole('button', { name: 'Verify bucket' })).toBeNull();
  });

  it('omits tabs the deployment does not support', async () => {
    respond();
    renderWithProviders(<BucketDetail bucket="uploads" />, {
      info: systemInfo({
        capabilities: {
          ...systemInfo().capabilities,
          versioning: false,
          lifecycle: false,
          events: false,
        },
      }),
    });
    await screen.findByRole('tab', { name: 'Overview' });

    // A tab that is present is a tab that works.
    expect(screen.queryByRole('tab', { name: 'Versioning' })).toBeNull();
    expect(screen.queryByRole('tab', { name: 'Lifecycle' })).toBeNull();
    expect(screen.queryByRole('tab', { name: 'Activity' })).toBeNull();
    expect(screen.getByRole('tab', { name: 'Quota' })).toBeTruthy();
  });
});
