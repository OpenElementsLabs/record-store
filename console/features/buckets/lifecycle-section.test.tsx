import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { BucketDetail } from './bucket-detail';
import { auditorPermissions, jsonResponse, renderWithProviders, session } from '@/test/render';
import type { Bucket, LifecycleRule } from '@/types/api';

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
  usePathname: () => '/buckets/uploads',
  useSearchParams: () => new URLSearchParams(),
}));

function bucket(): Bucket {
  return {
    id: 'b1',
    organization_id: 'org',
    name: 'uploads',
    created_at: '2026-08-01T10:00:00Z',
    versioning: 'enabled',
    quota: { bytes: { mode: 'unlimited' }, objects: { mode: 'unlimited' } },
    object_count: 4,
    logical_bytes: 1_000,
    version_count: 4,
    version_bytes: 1_000,
    multipart_bytes: 0,
  };
}

function rule(overrides: Partial<LifecycleRule> = {}): LifecycleRule {
  return {
    id: 'rule-1',
    bucket_id: 'b1',
    prefix: 'backups/',
    enabled: true,
    expiration: 90,
    noncurrent_version_expiration: null,
    created_at: '2026-08-01T00:00:00Z',
    updated_at: '2026-08-01T00:00:00Z',
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

function respond(rules: LifecycleRule[]) {
  fetchMock.mockImplementation((url: string, init?: RequestInit) => {
    const target = String(url);
    if (init?.method === 'PUT') return Promise.resolve(jsonResponse(rules[0]));
    if (target.includes('/lifecycle')) return Promise.resolve(jsonResponse(rules));
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

async function openLifecycle() {
  renderWithProviders(<BucketDetail bucket="uploads" />);
  await userEvent.click(await screen.findByRole('tab', { name: 'Lifecycle' }));
}

describe('bucket lifecycle rules', () => {
  it('states what a rule will actually do', async () => {
    respond([rule()]);
    await openLifecycle();

    expect(
      await screen.findByText('Objects under backups/ are deleted 90 days after creation.'),
    ).toBeTruthy();
  });

  it('disables a rule by replacing it with enabled flipped', async () => {
    respond([rule()]);
    await openLifecycle();
    await screen.findByText(/deleted 90 days/);

    await userEvent.click(screen.getByRole('button', { name: /disable lifecycle rule backups/i }));

    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'PUT');
      expect(call).toBeTruthy();
      // The whole rule is sent, so nothing else is silently changed.
      expect(JSON.parse(String(call?.[1]?.body))).toEqual({
        prefix: 'backups/',
        enabled: false,
        expiration: 90,
        noncurrent_version_expiration: null,
      });
      expect(String(call?.[0])).toContain('/buckets/uploads/lifecycle/rule-1');
    });
  });

  it('offers to enable a rule that is currently disabled', async () => {
    respond([rule({ enabled: false })]);
    await openLifecycle();

    expect(await screen.findByText(/This rule is currently disabled\./)).toBeTruthy();
    expect(screen.getByRole('button', { name: /enable lifecycle rule backups/i })).toBeTruthy();
  });

  it('seeds the edit form from the rule being edited', async () => {
    respond([rule({ noncurrent_version_expiration: 30 })]);
    await openLifecycle();
    await screen.findByText(/deleted 90 days/);

    await userEvent.click(screen.getByRole('button', { name: /edit lifecycle rule backups/i }));
    const dialog = await screen.findByRole('dialog');

    expect(within(dialog).getByText('Edit lifecycle rule')).toBeTruthy();
    expect((within(dialog).getByLabelText('Key prefix') as HTMLInputElement).value).toBe(
      'backups/',
    );
    expect(
      (within(dialog).getByLabelText(/Expire current objects/) as HTMLInputElement).value,
    ).toBe('90');
    expect(
      (within(dialog).getByLabelText(/Expire non-current versions/) as HTMLInputElement).value,
    ).toBe('30');
  });

  it('clears an expiry as an explicit null rather than omitting it', async () => {
    respond([rule({ noncurrent_version_expiration: 30 })]);
    await openLifecycle();
    await screen.findByText(/deleted 90 days/);

    await userEvent.click(screen.getByRole('button', { name: /edit lifecycle rule backups/i }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.clear(within(dialog).getByLabelText(/Expire current objects/));
    await userEvent.click(within(dialog).getByRole('button', { name: 'Save rule' }));

    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'PUT');
      expect(JSON.parse(String(call?.[1]?.body)).expiration).toBeNull();
    });
  });

  it('refuses to save a rule with no expiry at all', async () => {
    respond([rule()]);
    await openLifecycle();
    await screen.findByText(/deleted 90 days/);

    await userEvent.click(screen.getByRole('button', { name: /edit lifecycle rule backups/i }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.clear(within(dialog).getByLabelText(/Expire current objects/));

    // A rule that expires nothing is not a rule; the backend rejects it too.
    expect(within(dialog).getByRole('button', { name: 'Save rule' }).hasAttribute('disabled')).toBe(
      true,
    );
  });

  it('offers no rule changes to a role that cannot manage buckets', async () => {
    respond([rule()]);
    renderWithProviders(<BucketDetail bucket="uploads" />, {
      session: session(auditorPermissions),
    });
    await userEvent.click(await screen.findByRole('tab', { name: 'Lifecycle' }));
    await screen.findByText(/deleted 90 days/);

    expect(screen.queryByRole('button', { name: /edit lifecycle rule/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /disable lifecycle rule/i })).toBeNull();
    expect(screen.queryByRole('button', { name: /delete lifecycle rule/i })).toBeNull();
  });
});
