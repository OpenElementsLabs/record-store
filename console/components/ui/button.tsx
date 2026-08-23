'use client';

import { Slot } from '@radix-ui/react-slot';
import { cva, type VariantProps } from 'class-variance-authority';
import * as React from 'react';

import { cn } from '@/lib/utils';

const buttonVariants = cva(
  [
    'inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-[--radius-control]',
    'text-sm font-medium transition-quiet',
    // A press that moves is a press that registered. One pixel is enough to
    // feel deliberate without looking springy.
    'active:translate-y-px',
    'disabled:pointer-events-none disabled:opacity-50 disabled:active:translate-y-0',
    '[&_svg]:size-4 [&_svg]:shrink-0',
  ],
  {
    variants: {
      variant: {
        // The one place a shadow is earned on a control: it lifts the single
        // primary action off the page without every button floating.
        primary: 'bg-accent text-accent-ink shadow-sm hover:bg-accent-hover',
        secondary: 'border border-border-strong bg-surface text-ink hover:bg-surface-muted',
        ghost: 'text-ink-muted hover:bg-surface-muted hover:text-ink',
        danger: 'bg-danger text-white shadow-sm hover:bg-danger-hover',
        link: 'text-accent underline-offset-4 hover:underline active:translate-y-0',
      },
      size: {
        sm: 'h-8 px-3',
        md: 'h-9 px-4',
        lg: 'h-10 px-5',
        icon: 'size-9',
      },
    },
    defaultVariants: { variant: 'secondary', size: 'md' },
  },
);

export type ButtonProps = React.ComponentProps<'button'> &
  VariantProps<typeof buttonVariants> & {
    /** Renders the child element instead of a `button`, keeping the styling. */
    readonly asChild?: boolean;
  };

export function Button({ className, variant, size, asChild = false, type, ...props }: ButtonProps) {
  const Component = asChild ? Slot : 'button';
  return (
    <Component
      // An unspecified type on a form button defaults to submit, which is a
      // common source of accidental submissions.
      {...(asChild ? {} : { type: type ?? 'button' })}
      className={cn(buttonVariants({ variant, size }), className)}
      {...props}
    />
  );
}

export { buttonVariants };
