import type { WebhookDeliveryLog } from '@/types/api';

/** Delivery outcomes for one webhook, within whatever window was fetched. */
export type DeliveryHealth = {
  readonly total: number;
  readonly failed: number;
  readonly lastAttemptAt: string | null;
  readonly lastError: string | null;
};

/**
 * Summarises a webhook's recent deliveries.
 *
 * The management API returns a bounded, unfiltered delivery log, so this can
 * only describe the deliveries in that window. Callers must say so: a webhook
 * with no rows here has had no *recent* deliveries, which is not the same as
 * never having been delivered to.
 */
export function summariseDeliveries(
  deliveries: readonly WebhookDeliveryLog[],
  webhookId: string,
): DeliveryHealth {
  const mine = deliveries.filter((entry) => entry.webhook_id === webhookId);
  const failures = mine.filter((entry) => !entry.success);
  // The log is newest-first, so the first failure is the most recent one.
  const newest = mine.reduce<WebhookDeliveryLog | null>(
    (latest, entry) =>
      latest === null || entry.delivered_at > latest.delivered_at ? entry : latest,
    null,
  );
  const newestFailure = failures.reduce<WebhookDeliveryLog | null>(
    (latest, entry) =>
      latest === null || entry.delivered_at > latest.delivered_at ? entry : latest,
    null,
  );
  return {
    total: mine.length,
    failed: failures.length,
    lastAttemptAt: newest?.delivered_at ?? null,
    lastError: newestFailure?.error ?? null,
  };
}
