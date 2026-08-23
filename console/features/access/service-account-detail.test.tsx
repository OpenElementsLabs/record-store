import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ServiceAccountDetail } from './service-account-detail';
import { auditorPermissions, jsonResponse, renderWithProviders, session } from '@/test/render';
import type { Credential, Policy, ServiceAccountInfo } from '@/types/api';

function credential(overrides: Partial<Credential> = {}): Credential {
  return {
    id: 'cred-1',
    service_account_id: 'acc-1',
    key_id: 'AKIAEXAMPLE',
    disabled: false,
    created_at: '2026-08-20T10:00:00Z',
    expires_at: null,
    ...overrides,
  };
}

function info(overrides: Partial<ServiceAccountInfo> = {}): ServiceAccountInfo {
  return {
    account: {
      id: 'acc-1',
      organization_id: 'org',
      name: 'ingest',
      description: 'Writes incoming telemetry',
      disabled: false,
      created_at: '2026-08-01T10:00:00Z',
      updated_at: '2026-08-01T10:00:00Z',
    },
    credential: credential(),
    credentials: [credential()],
    policy_bindings: ['pol-1'],
    ...overrides,
  } as ServiceAccountInfo;
}

const policy: Policy = {
  id: 'pol-1',
  name: 'telemetry-writer',
  description: '',
  statements: [{ effect: 'allow', actions: ['s3:PutObject'], resources: ['bucket:telemetry/*'] }],
  created_at: '2026-08-01T00:00:00Z',
  updated_at: '2026-08-01T00:00:00Z',
};

let fetchMock: ReturnType<typeof vi.fn>;

function respond(account: ServiceAccountInfo, audit: unknown[] = []) {
  fetchMock.mockImplementation((url: string, init?: RequestInit) => {
    const target = String(url);
    if (init?.method === 'POST') {
      return Promise.resolve(
        jsonResponse({
          account: account.account,
          credential: credential({ id: 'cred-2', key_id: 'AKIANEW' }),
          secret_access_key: 'brand-new-secret',
        }),
      );
    }
    if (target.includes('/v1/policies')) return Promise.resolve(jsonResponse([policy]));
    if (target.includes('/v1/audit/events')) {
      return Promise.resolve(jsonResponse({ events: audit, next_time: null, next_id: null }));
    }
    return Promise.resolve(jsonResponse(account));
  });
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('ServiceAccountDetail', () => {
  it('identifies the account and its counts', async () => {
    respond(info());
    renderWithProviders(<ServiceAccountDetail accountId="acc-1" />);

    expect(await screen.findByText('Writes incoming telemetry')).toBeTruthy();
    expect(screen.getByText('Active')).toBeTruthy();
  });

  it('distinguishes a permanent credential from a temporary one', async () => {
    respond(
      info({
        credentials: [
          credential(),
          credential({ id: 'cred-2', key_id: 'AKIATEMP', expires_at: '2099-01-01T00:00:00Z' }),
        ],
      }),
    );
    renderWithProviders(<ServiceAccountDetail accountId="acc-1" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Credentials' }));

    expect(await screen.findByText('Permanent')).toBeTruthy();
    expect(screen.getByText(/Expires in/)).toBeTruthy();
  });

  it('rotates without revoking the existing credential', async () => {
    respond(info());
    renderWithProviders(<ServiceAccountDetail accountId="acc-1" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Credentials' }));

    // The copy has to be explicit: rotation that silently revoked would break
    // running applications.
    expect(screen.getByText(/leaves the old one working/)).toBeTruthy();
    await userEvent.click(screen.getByRole('button', { name: 'Rotate' }));

    expect(await screen.findByText('New credential issued')).toBeTruthy();
    expect(screen.queryByText('brand-new-secret')).toBeNull();
  });

  it('disables one credential without touching the others', async () => {
    respond(info({ credentials: [credential(), credential({ id: 'cred-2', key_id: 'AKIATWO' })] }));
    renderWithProviders(<ServiceAccountDetail accountId="acc-1" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Credentials' }));

    await userEvent.click(
      await screen.findByRole('button', { name: 'Disable credential AKIATWO' }),
    );
    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([url]) => String(url).includes('/cred-2/status'));
      expect(call).toBeTruthy();
    });
  });

  it('says an account with no policies is authorised for nothing', async () => {
    respond(info({ policy_bindings: [] }));
    renderWithProviders(<ServiceAccountDetail accountId="acc-1" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Policies' }));

    // Authenticating and being authorised are different, and this is the
    // distinction operators miss.
    expect(await screen.findByText(/authorised for nothing/)).toBeTruthy();
  });

  it('lists the resources an attached policy grants', async () => {
    respond(info());
    renderWithProviders(<ServiceAccountDetail accountId="acc-1" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Policies' }));

    expect(await screen.findByText('telemetry-writer')).toBeTruthy();
    expect(screen.getByText('bucket:telemetry/*')).toBeTruthy();
  });

  it('scopes the audit trail to this principal', async () => {
    respond(info(), [
      {
        event_id: 'a1',
        timestamp: '2026-08-23T10:00:00Z',
        request_id: 'req-1',
        principal: 'ingest',
        credential_id: 'cred-1',
        source_ip: '10.0.0.9',
        operation: 'PutObject',
        resource: 'telemetry/day.json',
        result: 'success',
        metadata: {},
      },
    ]);
    renderWithProviders(<ServiceAccountDetail accountId="acc-1" />);
    await userEvent.click(await screen.findByRole('tab', { name: 'Activity' }));

    expect(await screen.findByText('PutObject')).toBeTruthy();
    const call = fetchMock.mock.calls.find(([url]) => String(url).includes('/audit/events'));
    expect(String(call?.[0])).toContain('principal=ingest');
  });

  it('offers no credential controls or activity to an auditor', async () => {
    respond(info());
    renderWithProviders(<ServiceAccountDetail accountId="acc-1" />, {
      session: session(auditorPermissions),
    });
    await userEvent.click(await screen.findByRole('tab', { name: 'Credentials' }));

    expect(screen.queryByRole('button', { name: 'Rotate' })).toBeNull();
    expect(screen.queryByRole('button', { name: /Disable credential/ })).toBeNull();
    expect(screen.queryByRole('button', { name: /Temporary credential/ })).toBeNull();
  });
});
