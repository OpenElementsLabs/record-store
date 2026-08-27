'use client';

import { Slot } from '@radix-ui/react-slot';
import { cva, type VariantProps } from 'class-variance-authority';
import * as React from 'react';

import { cn } from '@/lib/utils';

const buttonVariants = cva(
  [
    'inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-control',
    'text-sm transition-quiet',
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
        // primary action off the page without every button floating. Semibold
        // and the softened hover both come from the login's submit button.
        primary:
          'bg-primary font-semibold text-primary-foreground shadow-sm hover:bg-primary-hover',
        secondary:
          'border border-border-strong bg-surface font-medium text-foreground hover:bg-surface-muted',
        ghost: 'font-medium text-foreground-muted hover:bg-surface-muted hover:text-foreground',
        danger: 'bg-danger font-semibold text-white shadow-sm hover:bg-danger-hover',
        link: 'font-medium text-primary underline-offset-4 hover:underline active:translate-y-0',
      },
      size: {
        sm: 'h-8 px-3',
        md: 'h-10 px-4',
        // The login's control height, for a screen's single committing action.
        lg: 'h-12 px-5 text-[0.9375rem]',
        icon: 'size-10',
        'icon-sm': 'size-8',
      },
    },
    defaultVariants: { variant: 'secondary', size: 'md' },
  },
);

/**
 * shadcn's variant names, mapped onto the Record Store ones.
 *
 * The registry generates components against these names, so accepting them
 * means a pasted component renders in the Record Store palette rather than failing to
 * compile or falling back to Tailwind's defaults.
 */
const VARIANT_ALIAS = {
  default: 'primary',
  destructive: 'danger',
  outline: 'secondary',
} as const;

type RecordStoreVariant = NonNullable<VariantProps<typeof buttonVariants>['variant']>;
type RecordStoreSize = NonNullable<VariantProps<typeof buttonVariants>['size']>;

export type ButtonProps = Omit<React.ComponentProps<'button'>, 'type'> & {
  readonly variant?: RecordStoreVariant | keyof typeof VARIANT_ALIAS;
  readonly size?: RecordStoreSize | 'default';
  readonly type?: React.ComponentProps<'button'>['type'];
  /** Renders the child element instead of a `button`, keeping the styling. */
  readonly asChild?: boolean;
};

export function Button({ className, variant, size, asChild = false, type, ...props }: ButtonProps) {
  const Component = asChild ? Slot : 'button';
  const resolvedVariant: RecordStoreVariant | undefined =
    variant && variant in VARIANT_ALIAS
      ? VARIANT_ALIAS[variant as keyof typeof VARIANT_ALIAS]
      : (variant as RecordStoreVariant | undefined);
  const resolvedSize: RecordStoreSize | undefined = size === 'default' ? 'md' : size;

  return (
    <Component
      // An unspecified type on a form button defaults to submit, which is a
      // common source of accidental submissions.
      {...(asChild ? {} : { type: type ?? 'button' })}
      className={cn(buttonVariants({ variant: resolvedVariant, size: resolvedSize }), className)}
      {...props}
    />
  );
}

export { buttonVariants };
