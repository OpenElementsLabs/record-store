'use client';

import * as LabelPrimitive from '@radix-ui/react-label';
import * as React from 'react';

import { cn } from '@/lib/utils';

export function Label({ className, ...props }: React.ComponentProps<typeof LabelPrimitive.Root>) {
  return (
    <LabelPrimitive.Root className={cn('text-xs font-medium text-ink', className)} {...props} />
  );
}

/** A labelled form row with optional help and error text. */
export function Field({
  label,
  htmlFor,
  hint,
  error,
  children,
}: {
  readonly label: string;
  readonly htmlFor: string;
  readonly hint?: string;
  readonly error?: string | null;
  readonly children: React.ReactNode;
}) {
  const hintId = hint ? `${htmlFor}-hint` : undefined;
  const errorId = error ? `${htmlFor}-error` : undefined;
  return (
    <div className="space-y-1.5">
      <Label htmlFor={htmlFor}>{label}</Label>
      {React.isValidElement(children)
        ? React.cloneElement(children as React.ReactElement<Record<string, unknown>>, {
            id: htmlFor,
            'aria-describedby': [hintId, errorId].filter(Boolean).join(' ') || undefined,
            'aria-invalid': error ? true : undefined,
          })
        : children}
      {hint && !error ? (
        <p id={hintId} className="text-xs text-ink-subtle">
          {hint}
        </p>
      ) : null}
      {error ? (
        <p id={errorId} className="text-xs text-danger" role="alert">
          {error}
        </p>
      ) : null}
    </div>
  );
}
