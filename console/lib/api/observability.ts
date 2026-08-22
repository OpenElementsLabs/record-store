import type {
  AuditPage,
  AuditResult,
  CreatedWebhook,
  StorageEventPage,
  StorageEventType,
  WebhookDeliveryLog,
  WebhookSubscription,
} from '@/types/api';

import { request, requestVoid } from './client';

export type AuditFilters = {
  readonly since?: string | null;
  readonly until?: string | null;
  readonly principal?: string | null;
  readonly operation?: string | null;
  readonly resource?: string | null;
  readonly result?: AuditResult | null;
  readonly afterTime?: string | null;
  readonly afterId?: string | null;
  readonly limit?: number;
};

/**
 * Reads one page of the audit trail.
 *
 * The audit history can be very large, so it is always queried server side with
 * a cursor rather than filtered in the browser.
 */
export function fetchAuditEvents(filters: AuditFilters, signal?: AbortSignal): Promise<AuditPage> {
  return request<AuditPage>('/v1/audit/events', {
    query: {
      since: filters.since,
      until: filters.until,
      principal: filters.principal,
      operation: filters.operation,
      resource: filters.resource,
      result: filters.result,
      after_time: filters.afterTime,
      after_id: filters.afterId,
      limit: filters.limit ?? 50,
    },
    ...(signal ? { signal } : {}),
  });
}

export type EventFilters = {
  readonly since?: string | null;
  readonly until?: string | null;
  readonly bucket?: string | null;
  readonly type?: StorageEventType | null;
  readonly prefix?: string | null;
  readonly afterTime?: string | null;
  readonly afterId?: string | null;
  readonly limit?: number;
};

/**
 * Reads one page of storage events.
 *
 * Storage events describe what happened to data. They are a separate feed from
 * the audit trail, which describes who asked for it.
 */
export function fetchStorageEvents(
  filters: EventFilters,
  signal?: AbortSignal,
): Promise<StorageEventPage> {
  return request<StorageEventPage>('/v1/events', {
    query: {
      since: filters.since,
      until: filters.until,
      bucket: filters.bucket,
      type: filters.type,
      prefix: filters.prefix,
      after_time: filters.afterTime,
      after_id: filters.afterId,
      limit: filters.limit ?? 50,
    },
    ...(signal ? { signal } : {}),
  });
}

export function fetchWebhooks(signal?: AbortSignal): Promise<WebhookSubscription[]> {
  return request<WebhookSubscription[]>('/v1/webhooks', signal ? { signal } : {});
}

/** Creates a webhook. The signing secret is returned exactly once. */
export function createWebhook(input: {
  target_url: string;
  event_types: readonly StorageEventType[];
  bucket_filter: string | null;
  object_prefix_filter: string | null;
  enabled: boolean;
}): Promise<CreatedWebhook> {
  return request<CreatedWebhook>('/v1/webhooks', { method: 'POST', body: input });
}

export function setWebhookEnabled(id: string, enabled: boolean): Promise<WebhookSubscription> {
  return request<WebhookSubscription>(`/v1/webhooks/${encodeURIComponent(id)}/status`, {
    method: 'PUT',
    body: { enabled },
  });
}

export function deleteWebhook(id: string): Promise<void> {
  return requestVoid(`/v1/webhooks/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

export function fetchWebhookDeliveries(
  limit = 100,
  signal?: AbortSignal,
): Promise<WebhookDeliveryLog[]> {
  return request<WebhookDeliveryLog[]>('/v1/webhook-deliveries', {
    query: { limit },
    ...(signal ? { signal } : {}),
  });
}
