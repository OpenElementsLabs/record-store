import * as React from 'react';

import { cn } from '@/lib/utils';

export function Input({ className, type, ...props }: React.ComponentProps<'input'>) {
  return (
    <input
      type={type ?? 'text'}
      className={cn(
        'h-9 w-full rounded-[--radius-control] border border-border-strong bg-surface px-3 type-body transition-quiet placeholder:text-ink-subtle hover:border-ink-subtle disabled:opacity-50 aria-[invalid=true]:border-danger',
        className,
      )}
      {...props}
    />
  );
}

export function Textarea({ className, ...props }: React.ComponentProps<'textarea'>) {
  return (
    <textarea
      className={cn(
        'w-full rounded-[--radius-control] border border-border-strong bg-surface px-3 py-2 type-identifier text-ink transition-quiet placeholder:text-ink-subtle hover:border-ink-subtle disabled:opacity-50',
        className,
      )}
      {...props}
    />
  );
}
