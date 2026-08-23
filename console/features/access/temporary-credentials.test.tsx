import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ServiceAccountsScreen } from './service-accounts-screen';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { Credential, ServiceAccountInfo } from '@/types/api';

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
  usePathname: () => '/service-accounts',
  useSearchParams: () => new URLSearchParams(),
}));

function credential(overrides: Partial<Credential> = {}): Credential {
  return {
    id: 'cred-1',
    service_account_id: 'acc-1',
    key_id: 'AKIA000',
    disabled: false,
    created_at: '2026-08-23T10:00:00Z',
    expires_at: null,
    ...overrides,
  };
}

function account(credentials: Credential[]): ServiceAccountInfo {
  return {
    account: {
      id: 'acc-1',
      name: 'ingest',
      description: '',
      disabled: false,
      created_at: '2026-08-01T10:00:00Z',
    },
    credential: credentials[0]!,
    credentials,
    policy_bindings: [],
  } as unknown as ServiceAccountInfo;
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(new Date('2026-08-23T12:00:00Z'));
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('temporary credentials', () => {
  it('shows how long a temporary credential has left', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse([account([credential({ expires_at: '2026-08-23T14:30:00Z' })])]),
      ),
    );
    renderWithProviders(<ServiceAccountsScreen />);

    expect(await screen.findByText(/temporary expires in 2h 30m/)).toBeTruthy();
  });

  it('says a lapsed credential has expired rather than counting backwards', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse([account([credential({ expires_at: '2026-08-23T11:00:00Z' })])]),
      ),
    );
    renderWithProviders(<ServiceAccountsScreen />);

    expect(await screen.findByText(/temporary credential expired/)).toBeTruthy();
  });

  it('says nothing about expiry for a permanent credential', async () => {
    fetchMock.mockImplementation(() => Promise.resolve(jsonResponse([account([credential()])])));
    renderWithProviders(<ServiceAccountsScreen />);

    await screen.findByText('ingest');
    expect(screen.queryByText(/temporary/)).toBeNull();
  });

  it('issues a temporary credential with the chosen lifetime', async () => {
    fetchMock.mockImplementation((url: string, init?: RequestInit) => {
      if (init?.method === 'POST') {
        return Promise.resolve(
          jsonResponse({
            account: account([credential()]).account,
            credential: credential({ expires_at: '2026-08-23T20:00:00Z' }),
            secret_access_key: 'super-secret-value',
          }),
        );
      }
      return Promise.resolve(jsonResponse([account([credential()])]));
    });
    renderWithProviders(<ServiceAccountsScreen />);
    await screen.findByText('ingest');

    await userEvent.click(screen.getByRole('button', { name: /actions for ingest/i }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /issue temporary/i }));

    const dialog = await screen.findByRole('dialog');
    // The only decision is the lifetime: policies are inherited, not chosen.
    expect(dialog.textContent).toMatch(/same policies as the account/);
    await userEvent.selectOptions(within(dialog).getByLabelText('Expires after'), '28800');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Issue credential' }));

    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'POST');
      expect(String(call?.[0])).toContain('/temporary-credentials');
      expect(JSON.parse(String(call?.[1]?.body))).toEqual({ expires_in_seconds: 28_800 });
    });
  });

  it('reveals the secret once and says it expires by itself', async () => {
    fetchMock.mockImplementation((url: string, init?: RequestInit) => {
      if (init?.method === 'POST') {
        return Promise.resolve(
          jsonResponse({
            account: account([credential()]).account,
            credential: credential({ expires_at: '2026-08-23T20:00:00Z' }),
            secret_access_key: 'super-secret-value',
          }),
        );
      }
      return Promise.resolve(jsonResponse([account([credential()])]));
    });
    renderWithProviders(<ServiceAccountsScreen />);
    await screen.findByText('ingest');

    await userEvent.click(screen.getByRole('button', { name: /actions for ingest/i }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /issue temporary/i }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByRole('button', { name: 'Issue credential' }));

    expect(await screen.findByText('Temporary credential issued')).toBeTruthy();
    expect(screen.getByText(/expires on its own/)).toBeTruthy();
    // The secret is masked until deliberately revealed.
    expect(screen.queryByText('super-secret-value')).toBeNull();
  });
});
