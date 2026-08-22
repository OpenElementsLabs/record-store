/**
 * The console's representation of an OES management API failure.
 *
 * The API always answers with `{ error: { code, message, request_id } }`, so the
 * console can show an actionable message and keep the request identifier for
 * support without ever surfacing internals.
 */

export type ApiErrorBody = {
  readonly error: {
    readonly code: string;
    readonly message: string;
    readonly request_id: string;
  };
};

/** Failure categories the UI reacts to differently. */
export type ApiErrorKind =
  | 'unauthorized'
  | 'forbidden'
  | 'not-found'
  | 'conflict'
  | 'invalid'
  | 'unavailable'
  | 'server'
  | 'network';

export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly requestId: string | null;
  readonly kind: ApiErrorKind;

  constructor(options: {
    status: number;
    code: string;
    message: string;
    requestId: string | null;
  }) {
    super(options.message);
    this.name = 'ApiError';
    this.status = options.status;
    this.code = options.code;
    this.requestId = options.requestId;
    this.kind = classify(options.status, options.code);
  }

  /** Whether the session is gone and the user must sign in again. */
  get isUnauthorized(): boolean {
    return this.kind === 'unauthorized';
  }

  /** Whether the role is authenticated but lacks permission. */
  get isForbidden(): boolean {
    return this.kind === 'forbidden';
  }

  get isNotFound(): boolean {
    return this.kind === 'not-found';
  }
}

function classify(status: number, code: string): ApiErrorKind {
  if (code === 'NETWORK_UNREACHABLE') return 'network';
  if (status === 401) return 'unauthorized';
  if (status === 403) return 'forbidden';
  if (status === 404) return 'not-found';
  if (status === 409) return 'conflict';
  if (status === 503) return 'unavailable';
  if (status >= 400 && status < 500) return 'invalid';
  return 'server';
}

function isApiErrorBody(value: unknown): value is ApiErrorBody {
  if (typeof value !== 'object' || value === null) return false;
  const error = (value as { error?: unknown }).error;
  if (typeof error !== 'object' || error === null) return false;
  const candidate = error as Record<string, unknown>;
  return typeof candidate.code === 'string' && typeof candidate.message === 'string';
}

/**
 * Turns a failed response into a typed error.
 *
 * A body that does not match the documented envelope still produces a usable
 * error rather than a parse failure, because a proxy or load balancer can answer
 * on the API's behalf.
 */
export async function apiErrorFromResponse(response: Response): Promise<ApiError> {
  let body: unknown = null;
  try {
    body = await response.json();
  } catch {
    body = null;
  }
  if (isApiErrorBody(body)) {
    return new ApiError({
      status: response.status,
      code: body.error.code,
      message: body.error.message,
      requestId: typeof body.error.request_id === 'string' ? body.error.request_id : null,
    });
  }
  return new ApiError({
    status: response.status,
    code: `HTTP_${response.status}`,
    message: fallbackMessage(response.status),
    requestId: response.headers.get('x-request-id'),
  });
}

function fallbackMessage(status: number): string {
  switch (status) {
    case 401:
      return 'Your session has expired. Sign in again to continue.';
    case 403:
      return 'Your management role does not permit this operation.';
    case 404:
      return 'The requested resource was not found.';
    case 503:
      return 'OES is not ready to serve this request yet.';
    default:
      return `The management API returned an unexpected status (${status}).`;
  }
}

/** The error used when the management API cannot be reached at all. */
export function networkError(): ApiError {
  return new ApiError({
    status: 0,
    code: 'NETWORK_UNREACHABLE',
    message: 'The OES management API is unreachable.',
    requestId: null,
  });
}

/** Narrows an unknown thrown value to an `ApiError`. */
export function asApiError(error: unknown): ApiError | null {
  return error instanceof ApiError ? error : null;
}
