import * as React from 'react';

import { cn } from '@/lib/utils';

export function Input({ className, type, ...props }: React.ComponentProps<'input'>) {
  return (
    <input
      type={type ?? 'text'}
      className={cn(
        'h-10 w-full rounded-control border border-border-strong bg-surface px-3 type-body transition-quiet placeholder:text-foreground-subtle hover:border-foreground-subtle focus-visible:border-primary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-primary disabled:opacity-50 aria-[invalid=true]:border-danger aria-[invalid=true]:focus-visible:ring-danger',
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
        'w-full rounded-control border border-border-strong bg-surface px-3 py-2 type-identifier text-foreground transition-quiet placeholder:text-foreground-subtle hover:border-foreground-subtle focus-visible:border-primary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-primary disabled:opacity-50',
        className,
      )}
      {...props}
    />
  );
}
