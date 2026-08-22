import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { WebhooksScreen } from './webhooks-screen';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { WebhookDeliveryLog, WebhookSubscription } from '@/types/api';

const subscription: WebhookSubscription = {
  id: 'webhook-1',
  target_url: 'https://hooks.example.test/oes',
  event_types: ['object.created'],
  bucket_filter: 'reports',
  object_prefix_filter: 'daily/',
  enabled: true,
  created_at: '2026-08-22T12:00:00Z',
};

let webhooks: WebhookSubscription[];
let deliveries: WebhookDeliveryLog[];
let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  webhooks = [];
  deliveries = [];
  fetchMock = vi.fn().mockImplementation(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? 'GET';
    if (url.includes('/v1/webhook-deliveries')) return jsonResponse(deliveries);
    if (url.endsWith('/v1/webhooks') && method === 'GET') return jsonResponse(webhooks);
    if (url.endsWith('/v1/webhooks') && method === 'POST') {
      webhooks = [subscription];
      return jsonResponse({ subscription, signing_secret: 'one-time-signing-secret-value' }, 201);
    }
    if (url.endsWith('/status') && method === 'PUT') {
      webhooks = [{ ...subscription, enabled: false }];
      return jsonResponse(webhooks[0]);
    }
    if (url.includes('/v1/webhooks/webhook-1') && method === 'DELETE') {
      webhooks = [];
      return new Response(null, { status: 204 });
    }
    throw new Error(`unexpected request: ${method} ${url}`);
  });
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('WebhooksScreen', () => {
  it('creates, displays, disables, and deletes a webhook without retaining its secret', async () => {
    renderWithProviders(<WebhooksScreen />);
    await screen.findByText('No webhooks');

    await userEvent.click(screen.getAllByRole('button', { name: 'Create webhook' })[0]!);
    const create = await screen.findByRole('dialog');
    await userEvent.type(
      within(create).getByLabelText('Endpoint URL'),
      'https://hooks.example.test/oes',
    );
    await userEvent.type(within(create).getByLabelText('Bucket filter'), 'reports');
    await userEvent.type(within(create).getByLabelText('Key prefix filter'), 'daily/');
    await userEvent.click(within(create).getByRole('button', { name: 'Create webhook' }));

    const secretDialog = await screen.findByRole('dialog');
    expect(within(secretDialog).getByText(/will not be shown again/i)).toBeTruthy();
    const secret = within(secretDialog).getByTestId('secret-value');
    expect(secret.textContent).toMatch(/^•+$/);
    await userEvent.click(within(secretDialog).getByRole('button', { name: /reveal/i }));
    expect(secret.textContent).toBe('one-time-signing-secret-value');
    await userEvent.click(within(secretDialog).getByRole('button', { name: 'Done' }));

    expect(await screen.findByText(subscription.target_url)).toBeTruthy();
    expect(screen.queryByText('one-time-signing-secret-value')).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: 'Webhook actions' }));
    await userEvent.click(await screen.findByRole('menuitem', { name: 'Disable' }));
    expect(await screen.findByText('Disabled')).toBeTruthy();

    await userEvent.click(screen.getByRole('button', { name: 'Webhook actions' }));
    await userEvent.click(await screen.findByRole('menuitem', { name: 'Delete webhook' }));
    const deleteDialog = await screen.findByRole('dialog');
    await userEvent.click(within(deleteDialog).getByRole('button', { name: 'Delete webhook' }));
    expect(await screen.findByText('No webhooks')).toBeTruthy();

    const methods = fetchMock.mock.calls.map(([, init]) => init?.method ?? 'GET');
    expect(methods).toEqual(expect.arrayContaining(['POST', 'PUT', 'DELETE']));
  });

  it('renders successful and failed delivery history', async () => {
    deliveries = [
      {
        webhook_id: 'webhook-1',
        event_id: 'event-1',
        attempts: 1,
        success: true,
        status_code: 204,
        error: null,
        delivered_at: '2026-08-22T12:00:00Z',
      },
      {
        webhook_id: 'webhook-2',
        event_id: 'event-2',
        attempts: 6,
        success: false,
        status_code: 503,
        error: 'unavailable',
        delivered_at: '2026-08-22T12:01:00Z',
      },
    ];
    renderWithProviders(<WebhooksScreen />);
    await screen.findByText('No webhooks');
    await userEvent.click(screen.getByRole('tab', { name: 'Delivery history' }));

    expect(await screen.findByText('Delivered')).toBeTruthy();
    expect(screen.getByText('Failed')).toBeTruthy();
    expect(screen.getByText('204')).toBeTruthy();
    expect(screen.getByText('503')).toBeTruthy();
    await waitFor(() => {
      expect(
        fetchMock.mock.calls.some(([url]) => String(url).includes('/v1/webhook-deliveries')),
      ).toBe(true);
    });
  });
});
