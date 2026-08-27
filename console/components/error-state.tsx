'use client';

import { AlertTriangle, RefreshCw } from 'lucide-react';
import * as React from 'react';

import { Button } from '@/components/ui/button';
import { ApiError } from '@/lib/api/error';

/**
 * Renders an API failure in terms the operator can act on.
 *
 * The API's own error code and message are shown rather than a generic apology,
 * and the request identifier stays available for support without being pushed
 * into the primary message.
 */
export function ErrorState({
  error,
  onRetry,
  title,
}: {
  readonly error: unknown;
  readonly onRetry?: () => void;
  readonly title?: string;
}) {
  const api = error instanceof ApiError ? error : null;
  const heading = title ?? headingFor(api);
  const message = api?.message ?? 'An unexpected error occurred in the console.';

  return (
    <div className="flex flex-col items-start gap-3 px-4 py-8" role="alert">
      <div className="flex items-center gap-2 text-danger">
        <AlertTriangle aria-hidden className="size-4" />
        <p className="text-sm font-medium">{heading}</p>
      </div>
      <p className="max-w-2xl text-sm text-ink-muted">{message}</p>
      {api ? <ErrorDetails error={api} /> : null}
      {onRetry ? (
        <Button size="sm" onClick={onRetry}>
          <RefreshCw aria-hidden />
          Try again
        </Button>
      ) : null}
    </div>
  );
}

function headingFor(error: ApiError | null): string {
  if (!error) return 'Something went wrong';
  switch (error.kind) {
    case 'network':
      return 'The management API is unreachable';
    case 'unauthorized':
      return 'Your session has ended';
    case 'forbidden':
      return 'Not permitted for your role';
    case 'not-found':
      return 'Not found';
    case 'unavailable':
      return 'Record Store is not ready';
    case 'conflict':
      return 'Conflicts with current state';
    case 'invalid':
      return 'The request was rejected';
    default:
      return 'The management API reported an error';
  }
}

/** Collapsed technical detail, including the request identifier. */
export function ErrorDetails({ error }: { readonly error: ApiError }) {
  if (!error.code && !error.requestId) return null;
  return (
    <details className="type-meta-subtle">
      <summary className="cursor-pointer select-none">Details</summary>
      <dl className="mt-2 space-y-1 font-mono">
        <div className="flex gap-2">
          <dt className="text-ink-subtle">Code</dt>
          <dd className="text-ink-muted">{error.code}</dd>
        </div>
        {error.status > 0 ? (
          <div className="flex gap-2">
            <dt className="text-ink-subtle">Status</dt>
            <dd className="text-ink-muted">{error.status}</dd>
          </div>
        ) : null}
        {error.requestId ? (
          <div className="flex gap-2">
            <dt className="text-ink-subtle">Request ID</dt>
            <dd className="text-ink-muted">{error.requestId}</dd>
          </div>
        ) : null}
      </dl>
    </details>
  );
}
