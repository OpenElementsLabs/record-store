import { cn } from '@/lib/utils';

/**
 * The Record Store monogram.
 *
 * A single component rather than repeated markup, so the sign-in screen and the
 * signed-in sidebar carry the same identity and it can only change in one place.
 * The mark is decorative — callers supply the accessible name.
 */
export function BrandMark({ className }: { readonly className?: string }) {
  return (
    <span
      aria-hidden="true"
      className={cn(
        'flex size-9 shrink-0 items-center justify-center rounded-control bg-foreground font-mono text-sm font-bold text-background',
        className,
      )}
    >
      O
    </span>
  );
}

/** The mark beside the product name, as the sign-in screen presents it. */
export function BrandLockup({ className }: { readonly className?: string }) {
  return (
    <span className={cn('flex items-center gap-3', className)}>
      <BrandMark />
      <span className="type-wordmark">Record Store CONSOLE</span>
    </span>
  );
}
