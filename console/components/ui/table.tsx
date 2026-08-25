import * as React from 'react';

import { cn } from '@/lib/utils';

/**
 * A horizontally scrollable table shell.
 *
 * Wide operational tables scroll inside their own container so the page body
 * never scrolls sideways.
 */
/**
 * The scroll container a wide table lives in.
 *
 * `relative` is load-bearing rather than decorative. Tables carry visually
 * hidden labels, and `.sr-only` positions them absolutely; without a positioned
 * ancestor those land against the viewport instead of this box, escape its
 * `overflow-x-auto`, and widen the whole document — a table that should scroll
 * inside its own card instead gives the page a horizontal scrollbar on a phone.
 */
export function TableShell({ className, ...props }: React.ComponentProps<'div'>) {
  return <div className={cn('relative w-full overflow-x-auto', className)} {...props} />;
}

export function Table({ className, ...props }: React.ComponentProps<'table'>) {
  return (
    <table className={cn('w-full caption-bottom border-collapse text-sm', className)} {...props} />
  );
}

export function TableHeader({ className, ...props }: React.ComponentProps<'thead'>) {
  return <thead className={cn('border-b border-border', className)} {...props} />;
}

export function TableBody({ className, ...props }: React.ComponentProps<'tbody'>) {
  return <tbody className={cn('divide-y divide-border', className)} {...props} />;
}

export function TableRow({ className, ...props }: React.ComponentProps<'tr'>) {
  return <tr className={cn('transition-quiet hover:bg-surface-muted/60', className)} {...props} />;
}

export function TableHead({ className, ...props }: React.ComponentProps<'th'>) {
  return (
    <th
      scope="col"
      className={cn(
        'whitespace-nowrap px-3 py-2 text-left text-xs font-medium text-ink-muted',
        className,
      )}
      {...props}
    />
  );
}

export function TableCell({ className, ...props }: React.ComponentProps<'td'>) {
  return <td className={cn('px-3 py-2 align-middle text-ink', className)} {...props} />;
}
