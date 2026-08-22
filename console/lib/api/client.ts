/**
 * The single entry point for every management API call the browser makes.
 *
 * Requests go to this application's own origin under `/api/oes`, which forwards
 * them to the OES management API with the session credential attached. That
 * keeps the credential in an HTTP-only cookie the browser cannot read, avoids
 * cross-origin configuration, and means no component ever handles a token.
 */

import { ApiError, apiErrorFromResponse, networkError } from './error';

/** Path prefix served by the console's own forwarding route. */
export const API_BASE = '/api/oes';

export type QueryValue = string | number | boolean | null | undefined;

export type RequestOptions = {
  readonly method?: 'GET' | 'POST' | 'PUT' | 'DELETE';
  readonly query?: Readonly<Record<string, QueryValue>>;
  readonly body?: unknown;
  readonly signal?: AbortSignal;
};

/** Builds a console-relative management API URL with encoded query values. */
export function apiUrl(path: string, query?: Readonly<Record<string, QueryValue>>): string {
  const normalized = path.startsWith('/') ? path : `/${path}`;
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(query ?? {})) {
    if (value === null || value === undefined || value === '') continue;
    search.set(key, String(value));
  }
  const suffix = search.toString();
  return `${API_BASE}${normalized}${suffix ? `?${suffix}` : ''}`;
}

/**
 * Encodes an object key for use in a path segment.
 *
 * Keys may contain slashes, spaces, and other characters that would otherwise be
 * interpreted as path structure. Each segment is encoded individually so the
 * logical hierarchy survives while the segments stay inert.
 */
export function encodeObjectKey(key: string): string {
  return key.split('/').map(encodeURIComponent).join('/');
}

async function send(path: string, options: RequestOptions): Promise<Response> {
  const init: RequestInit = {
    method: options.method ?? 'GET',
    // The session cookie is same-origin and HTTP-only.
    credentials: 'same-origin',
    headers: { accept: 'application/json' },
    ...(options.signal ? { signal: options.signal } : {}),
  };
  if (options.body !== undefined) {
    init.headers = { ...init.headers, 'content-type': 'application/json' };
    init.body = JSON.stringify(options.body);
  }
  try {
    return await fetch(apiUrl(path, options.query), init);
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === 'AbortError') throw cause;
    throw networkError();
  }
}

/** Performs a request and decodes a JSON response. */
export async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const response = await send(path, options);
  if (!response.ok) throw await apiErrorFromResponse(response);
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

/** Performs a request that returns no body. */
export async function requestVoid(path: string, options: RequestOptions = {}): Promise<void> {
  const response = await send(path, options);
  if (!response.ok) throw await apiErrorFromResponse(response);
}

export { ApiError };
