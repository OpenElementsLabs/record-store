import { cva, type VariantProps } from 'class-variance-authority';
import * as React from 'react';

import { cn } from '@/lib/utils';

const badgeVariants = cva(
  'inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-xs font-medium [&_svg]:size-3 [&_svg]:shrink-0',
  {
    variants: {
      tone: {
        neutral: 'border-border-strong bg-surface-muted text-ink-muted',
        ok: 'border-transparent bg-ok-soft text-ok',
        warn: 'border-transparent bg-warn-soft text-warn',
        danger: 'border-transparent bg-danger-soft text-danger',
        info: 'border-transparent bg-info-soft text-info',
        accent: 'border-transparent bg-accent-soft text-accent',
      },
    },
    defaultVariants: { tone: 'neutral' },
  },
);

export type BadgeProps = React.ComponentProps<'span'> & VariantProps<typeof badgeVariants>;

/**
 * A compact state label.
 *
 * Callers pass an icon alongside the text so the badge never relies on colour
 * alone to convey meaning.
 */
export function Badge({ className, tone, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ tone }), className)} {...props} />;
}

export { badgeVariants };
