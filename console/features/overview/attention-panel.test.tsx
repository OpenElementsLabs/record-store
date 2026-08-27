import { screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { AttentionPanel } from './attention-panel';
import { jsonResponse, renderWithProviders, systemInfo } from '@/test/render';
import type { StorageStatus, SystemMetrics, WebhookDeliveryLog } from '@/types/api';

function metrics(overrides: Partial<SystemMetrics> = {}): SystemMetrics {
  return {
    requests: 1_000,
    errors: 0,
    upload_bytes: 5_000_000,
    download_bytes: 9_000_000,
    storage: {
      object_count: 10,
      bucket_count: 2,
      version_count: 10,
      logical_bytes: 5_000_000,
      physical_bytes: 5_200_000,
      multipart_bytes: 0,
    },
    ...overrides,
  };
}

function status(usedFraction: number): StorageStatus {
  const capacity = 1_000_000_000;
  return {
    capacity_bytes: capacity,
    available_bytes: Math.round(capacity * (1 - usedFraction)),
    temporary_upload_bytes: 0,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

function respond(options: {
  metrics?: SystemMetrics;
  status?: StorageStatus;
  deliveries?: WebhookDeliveryLog[];
  health?: unknown;
}) {
  fetchMock.mockImplementation((url: string) => {
    const target = String(url);
    if (target.includes('/system/metrics')) {
      return Promise.resolve(jsonResponse(options.metrics ?? metrics()));
    }
    if (target.includes('/storage/status')) {
      return Promise.resolve(jsonResponse(options.status ?? status(0.1)));
    }
    if (target.includes('/webhook-deliveries')) {
      return Promise.resolve(jsonResponse(options.deliveries ?? []));
    }
    if (target.includes('/cluster/health')) {
      return Promise.resolve(jsonResponse(options.health ?? { reasons: [] }));
    }
    return Promise.resolve(jsonResponse({}));
  });
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('AttentionPanel', () => {
  it('says nothing needs attention when nothing does', async () => {
    respond({});
    renderWithProviders(<AttentionPanel />);

    expect(await screen.findByText('Nothing needs attention.')).toBeTruthy();
  });

  it('warns before a disk fills rather than after', async () => {
    respond({ status: status(0.88) });
    renderWithProviders(<AttentionPanel />);

    expect(await screen.findByText(/Disk is 88(\.0)?% full/)).toBeTruthy();
  });

  it('reports failures as a share of requests, never as a rate', async () => {
    respond({ metrics: metrics({ requests: 1_000, errors: 80 }) });
    renderWithProviders(<AttentionPanel />);

    const finding = await screen.findByText(/80 of 1,000 requests failed/);
    expect(finding.textContent).toMatch(/since this server started/i);
    // A per-second figure would be invented: Record Store exposes counters only.
    expect(document.body.textContent ?? '').not.toMatch(/per second|\/s\b|req\/s/i);
  });

  it('counts only failed webhook deliveries', async () => {
    const delivery = (success: boolean, id: string): WebhookDeliveryLog => ({
      webhook_id: 'w1',
      event_id: id,
      attempts: 1,
      success,
      status_code: success ? 200 : 500,
      error: success ? null : 'connection refused',
      delivered_at: '2026-08-22T10:00:00Z',
    });
    respond({ deliveries: [delivery(true, 'a'), delivery(false, 'b'), delivery(false, 'c')] });
    renderWithProviders(<AttentionPanel />);

    expect(await screen.findByText(/2 webhook deliveries have failed/)).toBeTruthy();
  });

  it('shows no cluster findings in a standalone deployment', async () => {
    respond({ metrics: metrics() });
    renderWithProviders(<AttentionPanel />, { info: systemInfo({ mode: 'standalone' }) });

    await screen.findByText('Nothing needs attention.');
    // A standalone server has no quorum and no replicas to be short of.
    expect(document.body.textContent ?? '').not.toMatch(/quorum|replica/i);
  });

  it('raises lost quorum and under-replication as critical', async () => {
    respond({
      metrics: metrics({
        cluster: {
          nodes: 3,
          healthy: false,
          quorum_writable: false,
          under_replicated_objects: 12,
          repair_active_tasks: 2,
          node_capacity_bytes: 1_000,
          node_used_bytes: 100,
          node_available_bytes: 900,
          logical_bytes: 1_000,
          physical_bytes: 3_000,
        },
      }),
    });
    renderWithProviders(<AttentionPanel />, {
      info: systemInfo({
        mode: 'cluster',
        capabilities: { ...systemInfo().capabilities, cluster: true },
      }),
    });

    expect(await screen.findByText(/no writable quorum/)).toBeTruthy();
    expect(screen.getByText(/12 objects hold fewer replicas/)).toBeTruthy();
    expect(screen.getByText(/2 repair tasks are running/)).toBeTruthy();

    // Critical findings sort above warnings so triage reads top-down.
    const items = screen.getAllByRole('listitem').map((item) => item.textContent ?? '');
    const quorum = items.findIndex((text) => text.includes('quorum'));
    const repair = items.findIndex((text) => text.includes('repair'));
    expect(quorum).toBeLessThan(repair);
  });

  it('labels severity for assistive technology, not by colour alone', async () => {
    respond({ status: status(0.97) });
    renderWithProviders(<AttentionPanel />);

    await screen.findByText(/Disk is/);
    expect(screen.getByText('Critical:')).toBeTruthy();
  });
});
