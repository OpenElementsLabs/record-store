import type { Bucket, BucketQuota, LifecycleRule, VersioningState } from '@/types/api';

import { request, requestVoid } from './client';

/**
 * Lists buckets with their accounting.
 *
 * Usage arrives with the list, so rendering a bucket table costs one request
 * regardless of how many buckets exist.
 */
export function fetchBuckets(signal?: AbortSignal): Promise<Bucket[]> {
  return request<Bucket[]>('/v1/buckets', signal ? { signal } : {});
}

export function createBucket(name: string): Promise<Bucket> {
  return request<Bucket>('/v1/buckets', { method: 'POST', body: { name } });
}

export function deleteBucket(name: string): Promise<void> {
  return requestVoid(`/v1/buckets/${encodeURIComponent(name)}`, { method: 'DELETE' });
}

export function setBucketVersioning(name: string, versioning: VersioningState): Promise<Bucket> {
  return request<Bucket>(`/v1/buckets/${encodeURIComponent(name)}/versioning`, {
    method: 'PUT',
    body: { versioning },
  });
}

/**
 * Replaces a bucket's quota.
 *
 * Both limits are sent together because the backend stores them as one value;
 * sending a partial quota would silently reset the other half.
 */
export function setBucketQuota(name: string, quota: BucketQuota): Promise<Bucket> {
  return request<Bucket>(`/v1/buckets/${encodeURIComponent(name)}/quota`, {
    method: 'PUT',
    body: { quota },
  });
}

export function fetchLifecycleRules(name: string, signal?: AbortSignal): Promise<LifecycleRule[]> {
  return request<LifecycleRule[]>(
    `/v1/buckets/${encodeURIComponent(name)}/lifecycle`,
    signal ? { signal } : {},
  );
}

export function createLifecycleRule(
  name: string,
  input: {
    readonly prefix: string;
    readonly enabled: boolean;
    readonly expiration: number | null;
    readonly noncurrent_version_expiration: number | null;
  },
): Promise<LifecycleRule> {
  return request<LifecycleRule>(`/v1/buckets/${encodeURIComponent(name)}/lifecycle`, {
    method: 'POST',
    body: input,
  });
}

export function deleteLifecycleRule(id: string): Promise<void> {
  return requestVoid(`/v1/lifecycle-rules/${encodeURIComponent(id)}`, { method: 'DELETE' });
}
