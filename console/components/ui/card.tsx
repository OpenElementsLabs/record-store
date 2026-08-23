import * as React from 'react';

import { cn } from '@/lib/utils';

export function Card({ className, ...props }: React.ComponentProps<'section'>) {
  return (
    <section
      className={cn(
        'rounded-[--radius-panel] border border-border bg-surface transition-quiet',
        className,
      )}
      {...props}
    />
  );
}

export function CardHeader({ className, ...props }: React.ComponentProps<'header'>) {
  return (
    <header
      className={cn('flex items-start justify-between gap-4 px-4 py-3', className)}
      {...props}
    />
  );
}

export function CardTitle({ className, ...props }: React.ComponentProps<'h2'>) {
  return <h2 className={cn('type-section-title', className)} {...props} />;
}

export function CardDescription({ className, ...props }: React.ComponentProps<'p'>) {
  return <p className={cn('type-meta', className)} {...props} />;
}

export function CardContent({ className, ...props }: React.ComponentProps<'div'>) {
  // `first:pt-4` is what lets content stand alone in a card. Following a
  // header it needs no top padding, but as the only child it does — and
  // without this every such call site invents its own vertical padding.
  return <div className={cn('px-4 pb-4 first:pt-4', className)} {...props} />;
}

export function CardFooter({ className, ...props }: React.ComponentProps<'footer'>) {
  return (
    <footer
      className={cn('flex items-center gap-2 border-t border-border px-4 py-3', className)}
      {...props}
    />
  );
}
