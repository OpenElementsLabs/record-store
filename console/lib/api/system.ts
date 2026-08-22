import type { Session, StorageStatus, StorageUsage, SystemInfo } from '@/types/api';

import { request } from './client';

/**
 * Reads deployment mode and capabilities.
 *
 * The backend is authoritative: the console never infers what a deployment can
 * do from its own environment.
 */
export function fetchSystemInfo(signal?: AbortSignal): Promise<SystemInfo> {
  return request<SystemInfo>('/v1/system/info', signal ? { signal } : {});
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
