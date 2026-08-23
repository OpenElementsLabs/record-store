import * as React from 'react';

import { cn } from '@/lib/utils';

export function Input({ className, type, ...props }: React.ComponentProps<'input'>) {
  return (
    <input
      type={type ?? 'text'}
      className={cn(
        'h-9 w-full rounded-[--radius-control] border border-border-strong bg-surface px-3 type-body placeholder:text-ink-subtle disabled:opacity-50',
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
        'w-full rounded-[--radius-control] border border-border-strong bg-surface px-3 py-2 font-mono text-xs text-ink placeholder:text-ink-subtle disabled:opacity-50',
        className,
      )}
      {...props}
    />
  );
}
