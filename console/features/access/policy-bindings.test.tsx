import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { PoliciesScreen } from './policies-screen';
import { auditorPermissions, jsonResponse, renderWithProviders, session } from '@/test/render';
import type { Policy, ServiceAccountInfo } from '@/types/api';

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
  usePathname: () => '/policies',
  useSearchParams: () => new URLSearchParams(),
}));

const policy: Policy = {
  id: 'pol-1',
  name: 'uploads-readers',
  description: '',
  statements: [{ effect: 'allow', actions: ['s3:GetObject'], resources: ['bucket:uploads/*'] }],
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-01T00:00:00Z',
};

function account(name: string, id: string, bindings: string[]): ServiceAccountInfo {
  return {
    account: { id, name, description: '', disabled: false, created_at: '2026-08-01T00:00:00Z' },
    credential: {
      id: `c-${id}`,
      service_account_id: id,
      key_id: 'AKIA',
      disabled: false,
      created_at: '2026-08-01T00:00:00Z',
      expires_at: null,
    },
    credentials: [],
    policy_bindings: bindings,
  } as unknown as ServiceAccountInfo;
}

let fetchMock: ReturnType<typeof vi.fn>;

function respond(accounts: ServiceAccountInfo[]) {
  fetchMock.mockImplementation((url: string, init?: RequestInit) => {
    if (init?.method === 'PUT' || init?.method === 'DELETE') {
      return Promise.resolve(new Response(null, { status: 204 }));
    }
    return Promise.resolve(
      jsonResponse(String(url).includes('/service-accounts') ? accounts : [policy]),
    );
  });
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('policy bindings', () => {
  it('says a policy grants nothing until it is attached', async () => {
    respond([account('ingest', 'acc-1', [])]);
    renderWithProviders(<PoliciesScreen />);

    // A policy with no bindings is inert, and that is the single most
    // misunderstood thing about a policy list.
    expect(await screen.findByText(/grants nothing until it is attached/)).toBeTruthy();
    expect(screen.getByText('Attached to no accounts')).toBeTruthy();
  });

  it('names the accounts a policy is attached to', async () => {
    respond([account('ingest', 'acc-1', ['pol-1']), account('reader', 'acc-2', [])]);
    renderWithProviders(<PoliciesScreen />);

    expect(await screen.findByText('Attached to 1 account')).toBeTruthy();
    expect(screen.getByText('ingest')).toBeTruthy();
  });

  it('attaches a policy to a chosen account', async () => {
    respond([account('ingest', 'acc-1', []), account('reader', 'acc-2', [])]);
    renderWithProviders(<PoliciesScreen />);
    await screen.findByText('Attached to no accounts');

    await userEvent.selectOptions(
      screen.getByLabelText(/Attach uploads-readers to a service account/),
      'acc-2',
    );
    await userEvent.click(screen.getByRole('button', { name: 'Attach' }));

    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'PUT');
      expect(String(call?.[0])).toContain('/v1/policies/pol-1/bindings/acc-2');
    });
  });

  it('offers only accounts that are not already attached', async () => {
    respond([account('ingest', 'acc-1', ['pol-1']), account('reader', 'acc-2', [])]);
    renderWithProviders(<PoliciesScreen />);
    await screen.findByText('Attached to 1 account');

    const select = screen.getByLabelText(/Attach uploads-readers to a service account/);
    const options = Array.from(select.querySelectorAll('option')).map((node) => node.textContent);
    expect(options).toContain('reader');
    expect(options).not.toContain('ingest');
  });

  it('detaches a policy from an account', async () => {
    respond([account('ingest', 'acc-1', ['pol-1'])]);
    renderWithProviders(<PoliciesScreen />);
    await screen.findByText('Attached to 1 account');

    await userEvent.click(
      screen.getByRole('button', { name: 'Detach uploads-readers from ingest' }),
    );

    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'DELETE');
      expect(String(call?.[0])).toContain('/v1/policies/pol-1/bindings/acc-1');
    });
  });

  it('shows bindings but no controls to a role that cannot manage policies', async () => {
    respond([account('ingest', 'acc-1', ['pol-1']), account('reader', 'acc-2', [])]);
    renderWithProviders(<PoliciesScreen />, { session: session(auditorPermissions) });

    expect(await screen.findByText('Attached to 1 account')).toBeTruthy();
    expect(screen.queryByRole('button', { name: /^Detach/ })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Attach' })).toBeNull();
  });
});
