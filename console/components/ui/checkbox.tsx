'use client';

import * as CheckboxPrimitive from '@radix-ui/react-checkbox';
import { Check, Minus } from 'lucide-react';
import type * as React from 'react';

import { cn } from '@/lib/utils';

/**
 * A tri-state checkbox.
 *
 * The indeterminate state carries its own glyph rather than a shaded box, so
 * "some selected" is distinguishable from "all selected" without relying on a
 * colour difference.
 */
export function Checkbox({
  className,
  ...props
}: React.ComponentProps<typeof CheckboxPrimitive.Root>) {
  return (
    <CheckboxPrimitive.Root
      className={cn(
        'peer size-4 shrink-0 rounded-[3px] border border-border-strong bg-surface',
        'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-1',
        'disabled:cursor-not-allowed disabled:opacity-50',
        'data-[state=checked]:border-accent data-[state=checked]:bg-accent',
        'data-[state=indeterminate]:border-accent data-[state=indeterminate]:bg-accent',
        className,
      )}
      {...props}
    >
      <CheckboxPrimitive.Indicator className="flex items-center justify-center text-accent-ink">
        {props.checked === 'indeterminate' ? (
          <Minus aria-hidden className="size-3" />
        ) : (
          <Check aria-hidden className="size-3" />
        )}
      </CheckboxPrimitive.Indicator>
    </CheckboxPrimitive.Root>
  );
}
