import type {
  BucketVerification,
  ObjectSummary,
  StorageInspection,
  StorageRepairResult,
} from '@/types/api';

import { encodeObjectKey, request } from './client';

/**
 * Cross-checks metadata against the bytes on disk.
 *
 * The scan is bounded by `maximumEntries` and reports whether it stopped early,
 * so a large deployment gets an answer rather than an unbounded walk.
 */
export function inspectStorage(
  maximumEntries: number,
  signal?: AbortSignal,
): Promise<StorageInspection> {
  return request<StorageInspection>('/v1/storage/inspect', {
    query: { maximum_entries: maximumEntries },
    ...(signal ? { signal } : {}),
  });
}

/**
 * Reclaims payloads that no metadata references.
 *
 * This does not recover anything: it deletes orphaned bytes. Objects whose
 * payload is missing cannot be rebuilt from a checksum, which is why the
 * console never presents this as a fix for data loss. `dryRun` reports what
 * would be removed without removing it, and is the default on the backend too.
 */
export function repairStorage(
  maximumEntries: number,
  dryRun: boolean,
): Promise<StorageRepairResult> {
  return request<StorageRepairResult>('/v1/storage/repair', {
    method: 'POST',
    body: { maximum_entries: maximumEntries, dry_run: dryRun },
  });
}

/** Re-reads and re-hashes every object in a bucket. */
export function verifyBucket(bucket: string): Promise<BucketVerification> {
  return request<BucketVerification>(`/v1/verify/buckets/${encodeURIComponent(bucket)}`, {
    method: 'POST',
  });
}

/** Re-reads and re-hashes one object, returning its confirmed metadata. */
export function verifyObject(bucket: string, key: string): Promise<ObjectSummary> {
  return request<ObjectSummary>(
    `/v1/verify/objects/${encodeURIComponent(bucket)}/${encodeObjectKey(key)}`,
    { method: 'POST' },
  );
}
