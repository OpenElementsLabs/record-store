import type { ObjectListPage, ObjectSummary, ObjectVersionPage } from '@/types/api';

import { apiUrl, encodeObjectKey, request, requestVoid } from './client';

export type ListObjectsParams = {
  readonly bucket: string;
  readonly prefix?: string;
  /** Set to `/` to group keys into logical folders. */
  readonly delimiter?: string;
  readonly continuationToken?: string | null;
  readonly limit?: number;
};

/**
 * Reads one page of a prefix listing.
 *
 * Listing is always paged. A bucket can hold millions of keys, so the console
 * never asks for a whole bucket.
 */
export function fetchObjects(
  params: ListObjectsParams,
  signal?: AbortSignal,
): Promise<ObjectListPage> {
  return request<ObjectListPage>(`/v1/buckets/${encodeURIComponent(params.bucket)}/objects`, {
    query: {
      prefix: params.prefix ?? '',
      delimiter: params.delimiter,
      continuation_token: params.continuationToken,
      limit: params.limit ?? 100,
    },
    ...(signal ? { signal } : {}),
  });
}

export function fetchObject(
  bucket: string,
  key: string,
  signal?: AbortSignal,
): Promise<ObjectSummary> {
  return request<ObjectSummary>(
    `/v1/buckets/${encodeURIComponent(bucket)}/object/${encodeObjectKey(key)}`,
    signal ? { signal } : {},
  );
}

export function deleteObject(bucket: string, key: string): Promise<void> {
  return requestVoid(`/v1/buckets/${encodeURIComponent(bucket)}/object/${encodeObjectKey(key)}`, {
    method: 'DELETE',
  });
}

export type ListVersionsParams = {
  readonly bucket: string;
  readonly prefix?: string;
  readonly keyMarker?: string | null;
  readonly versionIdMarker?: string | null;
  readonly limit?: number;
};

export function fetchObjectVersions(
  params: ListVersionsParams,
  signal?: AbortSignal,
): Promise<ObjectVersionPage> {
  return request<ObjectVersionPage>(
    `/v1/buckets/${encodeURIComponent(params.bucket)}/object-versions`,
    {
      query: {
        prefix: params.prefix ?? '',
        key_marker: params.keyMarker,
        version_id_marker: params.versionIdMarker,
        limit: params.limit ?? 100,
      },
      ...(signal ? { signal } : {}),
    },
  );
}

/**
 * Copies an object server side.
 *
 * The bytes never travel through the browser: OES streams them internally, so
 * copying a large object costs the console nothing. Supplying `contentType` or
 * `customMetadata` replaces the source's metadata instead of carrying it over.
 */
export function copyObject(params: {
  readonly sourceBucket: string;
  readonly sourceKey: string;
  readonly sourceVersionId?: string | null;
  readonly destinationBucket: string;
  readonly destinationKey: string;
  readonly contentType?: string;
  readonly customMetadata?: Readonly<Record<string, string>>;
}): Promise<ObjectSummary> {
  return request<ObjectSummary>(
    `/v1/buckets/${encodeURIComponent(params.destinationBucket)}/object-copy/${encodeObjectKey(params.destinationKey)}`,
    {
      method: 'POST',
      body: {
        source_bucket: params.sourceBucket,
        source_key: params.sourceKey,
        ...(params.sourceVersionId ? { source_version_id: params.sourceVersionId } : {}),
        ...(params.contentType ? { content_type: params.contentType } : {}),
        ...(params.customMetadata ? { custom_metadata: params.customMetadata } : {}),
      },
    },
  );
}

/** Permanently removes one version. This cannot be undone. */
export function deleteObjectVersion(bucket: string, key: string, versionId: string): Promise<void> {
  return requestVoid(
    `/v1/buckets/${encodeURIComponent(bucket)}/object-versions/${encodeObjectKey(key)}`,
    { method: 'DELETE', query: { version_id: versionId } },
  );
}

/** Restores a historical version, making it current again. */
export function restoreObjectVersion(
  bucket: string,
  key: string,
  versionId: string,
): Promise<ObjectSummary> {
  return request<ObjectSummary>(
    `/v1/restore/${encodeURIComponent(bucket)}/${encodeObjectKey(key)}`,
    { method: 'POST', body: { version_id: versionId } },
  );
}

/**
 * The URL that streams an object's bytes.
 *
 * The browser fetches this directly so payloads never pass through JavaScript
 * memory, and the session cookie authorises the transfer.
 */
export function objectContentUrl(bucket: string, key: string): string {
  return apiUrl(`/v1/buckets/${encodeURIComponent(bucket)}/object-content/${encodeObjectKey(key)}`);
}

/** The URL an upload is sent to. */
export function objectUploadUrl(bucket: string, key: string): string {
  return apiUrl(`/v1/buckets/${encodeURIComponent(bucket)}/object/${encodeObjectKey(key)}`);
}
