/**
 * Typed access to share and embed capabilities.
 *
 * Capability URLs are deliberately not part of the list responses. Fetching one
 * is its own request, made at the moment someone copies a link, so a routine
 * listing never carries a working capability through logs, caches, or a React
 * devtools panel.
 */

import type {
  CapabilityUrl,
  EmbedDisposition,
  EmbedLink,
  IssuedEmbed,
  IssuedShare,
  SharePermission,
  ShareLink,
  SharingSettings,
} from '@/types/api';

import { encodeObjectKey, request, requestVoid } from './client';

/** What this deployment allows, so dialogs offer only what will be accepted. */
export function fetchSharingSettings(signal?: AbortSignal): Promise<SharingSettings> {
  return request<SharingSettings>('/v1/sharing/settings', signal ? { signal } : {});
}

export function fetchObjectShares(
  bucket: string,
  key: string,
  signal?: AbortSignal,
): Promise<readonly ShareLink[]> {
  return request<readonly ShareLink[]>(
    `/v1/buckets/${encodeURIComponent(bucket)}/object-shares/${encodeObjectKey(key)}`,
    signal ? { signal } : {},
  );
}

export type CreateShareInput = {
  readonly label: string;
  /** Present only for a share pinned to one immutable version. */
  readonly versionId?: string | null;
  readonly permission: SharePermission;
  readonly expiresAt?: string | null;
  readonly password?: string | null;
  readonly maximumAccessCount?: number | null;
};

export function createObjectShare(
  bucket: string,
  key: string,
  input: CreateShareInput,
): Promise<IssuedShare> {
  return request<IssuedShare>(
    `/v1/buckets/${encodeURIComponent(bucket)}/object-shares/${encodeObjectKey(key)}`,
    {
      method: 'POST',
      body: {
        label: input.label,
        permission: input.permission,
        ...(input.versionId ? { version_id: input.versionId } : {}),
        ...(input.expiresAt ? { expires_at: input.expiresAt } : {}),
        ...(input.password ? { password: input.password } : {}),
        ...(input.maximumAccessCount ? { maximum_access_count: input.maximumAccessCount } : {}),
      },
    },
  );
}

export function fetchShare(id: string, signal?: AbortSignal): Promise<ShareLink> {
  return request<ShareLink>(`/v1/shares/${encodeURIComponent(id)}`, signal ? { signal } : {});
}

/** Fetches a share's URL. Called when someone copies it, never on a listing. */
export function fetchShareUrl(id: string, signal?: AbortSignal): Promise<CapabilityUrl> {
  return request<CapabilityUrl>(
    `/v1/shares/${encodeURIComponent(id)}/url`,
    signal ? { signal } : {},
  );
}

/** Withdraws a share. The next request against it fails. */
export function revokeShare(id: string): Promise<ShareLink> {
  return request<ShareLink>(`/v1/shares/${encodeURIComponent(id)}/revoke`, { method: 'POST' });
}

/** Removes an already-withdrawn share's record. */
export function deleteShare(id: string): Promise<void> {
  return requestVoid(`/v1/shares/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

export function fetchObjectEmbeds(
  bucket: string,
  key: string,
  signal?: AbortSignal,
): Promise<readonly EmbedLink[]> {
  return request<readonly EmbedLink[]>(
    `/v1/buckets/${encodeURIComponent(bucket)}/object-embeds/${encodeObjectKey(key)}`,
    signal ? { signal } : {},
  );
}

export type CreateEmbedInput = {
  readonly label: string;
  readonly versionId?: string | null;
  readonly expiresAt?: string | null;
  readonly allowedOrigins: readonly string[];
  readonly disposition: EmbedDisposition;
};

export function createObjectEmbed(
  bucket: string,
  key: string,
  input: CreateEmbedInput,
): Promise<IssuedEmbed> {
  return request<IssuedEmbed>(
    `/v1/buckets/${encodeURIComponent(bucket)}/object-embeds/${encodeObjectKey(key)}`,
    {
      method: 'POST',
      body: {
        label: input.label,
        disposition: input.disposition,
        allowed_origins: input.allowedOrigins,
        ...(input.versionId ? { version_id: input.versionId } : {}),
        ...(input.expiresAt ? { expires_at: input.expiresAt } : {}),
      },
    },
  );
}

export function fetchEmbed(id: string, signal?: AbortSignal): Promise<EmbedLink> {
  return request<EmbedLink>(`/v1/embeds/${encodeURIComponent(id)}`, signal ? { signal } : {});
}

export function fetchEmbedUrl(id: string, signal?: AbortSignal): Promise<CapabilityUrl> {
  return request<CapabilityUrl>(
    `/v1/embeds/${encodeURIComponent(id)}/url`,
    signal ? { signal } : {},
  );
}

/**
 * Replaces an embed's origin allowlist.
 *
 * The backend refuses to turn a restricted embed into an unrestricted one this
 * way, because that widens access rather than adjusting it. Doing that
 * deliberately means revoking the embed and creating a new one.
 */
export function updateEmbedOrigins(
  id: string,
  allowedOrigins: readonly string[],
): Promise<EmbedLink> {
  return request<EmbedLink>(`/v1/embeds/${encodeURIComponent(id)}`, {
    method: 'PATCH',
    body: { allowed_origins: allowedOrigins },
  });
}

export function revokeEmbed(id: string): Promise<EmbedLink> {
  return request<EmbedLink>(`/v1/embeds/${encodeURIComponent(id)}/revoke`, { method: 'POST' });
}

export function deleteEmbed(id: string): Promise<void> {
  return requestVoid(`/v1/embeds/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/**
 * Completes a capability URL against the console's own public origin.
 *
 * The management API returns a path when no public base address is configured,
 * because guessing an external address from a request header is how links end up
 * pointing at somewhere OES was never deployed. The browser, unlike the backend,
 * genuinely knows which origin the operator is using.
 */
export function absoluteCapabilityUrl(url: string): string {
  if (/^https?:\/\//i.test(url)) return url;
  if (typeof window === 'undefined') return url;
  return `${window.location.origin}${url.startsWith('/') ? url : `/${url}`}`;
}
