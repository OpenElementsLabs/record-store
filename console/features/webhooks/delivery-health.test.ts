import { describe, expect, it } from 'vitest';

import { summariseDeliveries } from './delivery-health';
import type { WebhookDeliveryLog } from '@/types/api';

function delivery(
  webhookId: string,
  success: boolean,
  deliveredAt: string,
  error: string | null = null,
): WebhookDeliveryLog {
  return {
    webhook_id: webhookId,
    event_id: `${webhookId}-${deliveredAt}`,
    attempts: success ? 1 : 3,
    success,
    status_code: success ? 200 : 500,
    error,
    delivered_at: deliveredAt,
  };
}

describe('summariseDeliveries', () => {
  it('counts only this webhook’s deliveries', () => {
    const health = summariseDeliveries(
      [
        delivery('w1', true, '2026-08-23T10:00:00Z'),
        delivery('w2', false, '2026-08-23T10:01:00Z', 'timeout'),
        delivery('w1', false, '2026-08-23T10:02:00Z', 'connection refused'),
      ],
      'w1',
    );
    expect(health.total).toBe(2);
    expect(health.failed).toBe(1);
  });

  it('reports the most recent attempt regardless of order', () => {
    // The window is not guaranteed sorted, so the newest is computed, not
    // assumed to be first.
    const health = summariseDeliveries(
      [
        delivery('w1', true, '2026-08-23T10:00:00Z'),
        delivery('w1', true, '2026-08-23T12:00:00Z'),
        delivery('w1', true, '2026-08-23T11:00:00Z'),
      ],
      'w1',
    );
    expect(health.lastAttemptAt).toBe('2026-08-23T12:00:00Z');
  });

  it('surfaces the most recent failure’s reason', () => {
    const health = summariseDeliveries(
      [
        delivery('w1', false, '2026-08-23T10:00:00Z', 'older failure'),
        delivery('w1', false, '2026-08-23T11:00:00Z', 'newer failure'),
        delivery('w1', true, '2026-08-23T12:00:00Z'),
      ],
      'w1',
    );
    // The latest attempt succeeded, but the last known error is still useful.
    expect(health.lastError).toBe('newer failure');
  });

  it('reports an empty window rather than inventing success', () => {
    const health = summariseDeliveries([delivery('w2', true, '2026-08-23T10:00:00Z')], 'w1');
    expect(health).toEqual({ total: 0, failed: 0, lastAttemptAt: null, lastError: null });
  });
});
