import type {
  MetricsHistory,
  Session,
  StorageStatus,
  StorageUsage,
  SystemInfo,
  SystemMetrics,
} from '@/types/api';

import { request } from './client';
import { ApiError } from './error';

/**
 * Reads deployment mode and capabilities.
 *
 * The backend is authoritative: the console never infers what a deployment can
 * do from its own environment.
 */
export function fetchSystemInfo(signal?: AbortSignal): Promise<SystemInfo> {
  return request<SystemInfo>('/v1/system/info', signal ? { signal } : {});
}

/**
 * What the server's readiness probe reported.
 *
 * Three outcomes, kept apart because they need different responses. `ready`
 * means the server completed its probe. `not-ready` means the server answered
 * and said it cannot serve — that is a working process with a storage problem.
 * `unreachable` means nothing answered at all, which is a different incident
 * entirely, and reporting it as "not ready" would send an operator looking at
 * the wrong thing.
 */
export type Readiness =
  | { readonly state: 'ready' }
  | { readonly state: 'not-ready'; readonly reason: string; readonly requestId: string | null }
  | { readonly state: 'unreachable'; readonly reason: string };

/**
 * Asks the server whether it can actually serve.
 *
 * This is a different question from "did something answer": the probe writes,
 * synchronises, and deletes a file on the storage path before reporting ready,
 * so it establishes write readiness rather than mere liveness.
 *
 * Failure is returned rather than thrown. Every outcome here is information the
 * screen has to render, so none of them is exceptional.
 */
export async function fetchReadiness(signal?: AbortSignal): Promise<Readiness> {
  // Its own console route rather than the forwarding proxy: the probe lives at
  // the management server's root, outside the versioned `/api` surface the
  // proxy forwards, because orchestrators poll it before any credential exists.
  let response: Response;
  try {
    response = await fetch('/api/readiness', {
      credentials: 'same-origin',
      headers: { accept: 'application/json' },
      ...(signal ? { signal } : {}),
    });
  } catch {
    return { state: 'unreachable', reason: 'The console could not be reached.' };
  }
  const body = (await response.json().catch(() => null)) as {
    state?: string;
    reason?: string;
    request_id?: string | null;
  } | null;
  if (body?.state === 'ready') return { state: 'ready' };
  if (body?.state === 'not-ready') {
    return {
      state: 'not-ready',
      reason: body.reason ?? 'The server reported it is not ready.',
      requestId: body.request_id ?? null,
    };
  }
  return {
    state: 'unreachable',
    reason: body?.reason ?? 'Readiness could not be determined.',
  };
}

/** Reads the identity behind the current session. */
export function fetchSession(signal?: AbortSignal): Promise<Session> {
  return request<Session>('/v1/auth/session', signal ? { signal } : {});
}

export function fetchStorageUsage(signal?: AbortSignal): Promise<StorageUsage> {
  return request<StorageUsage>('/v1/storage/usage', signal ? { signal } : {});
}

export function fetchStorageStatus(signal?: AbortSignal): Promise<StorageStatus> {
  return request<StorageStatus>('/v1/storage/status', signal ? { signal } : {});
}

/**
 * Reads the server's recent counter readings.
 *
 * Used once, to seed the charts. A failure here is not worth surfacing: the
 * screen still works by taking its own readings, it just starts empty, which is
 * exactly what it did before this existed.
 */
export function fetchSystemMetricsHistory(signal?: AbortSignal): Promise<MetricsHistory> {
  return request<MetricsHistory>('/v1/system/metrics/history', signal ? { signal } : {});
}

/** Reads current metric values through the management plane. */
export async function fetchSystemMetrics(signal?: AbortSignal): Promise<SystemMetrics> {
  const timeout = AbortSignal.timeout(10_000);
  try {
    return await request<SystemMetrics>('/v1/system/metrics', {
      signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
    });
  } catch (error) {
    if (timeout.aborted && !signal?.aborted) {
      throw new ApiError({
        status: 503,
        code: 'METRICS_TIMEOUT',
        message: 'Metrics took too long to respond. Try refreshing again.',
        requestId: null,
      });
    }
    throw error;
  }
}
