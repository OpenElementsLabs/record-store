import { cn } from '@/lib/utils';

type BrandProps = {
  readonly className?: string;
};

type BrandLockupProps = BrandProps & {
  readonly size?: 'compact' | 'default' | 'large';
  readonly tone?: 'brand' | 'inverse';
};

/**
 * The Record Store symbol supplied with the product identity.
 *
 * Keeping the geometry inline means the favicon, sign-in screen, public share
 * pages, and both sidebar states all render the same crisp mark at any size.
 * The mark is decorative because callers provide the accessible product name.
 */
export function BrandMark({ className }: BrandProps) {
  return (
    <svg
      aria-hidden="true"
      viewBox="0 0 96 116"
      className={cn('h-auto w-9 shrink-0', className)}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      <path d="M41 10 57 1l16 9-16 9-16-9Z" fill="#0BB3B8" />
      <path d="m10 19 10-6 10 6-10 6-10-6Zm28 13 8-5 8 5-8 5-8-5Z" fill="#195477" />

      <path d="m4 40 44 25v14L4 54V40Z" fill="#FFCA76" />
      <path d="m48 65 44-25v14L48 79V65Z" fill="#FFBB3D" />

      <path d="m4 58 44 25v14L4 72V58Z" fill="#FFCA76" />
      <path d="m48 83 44-25v14L48 97V83Z" fill="#FFBB3D" />

      <path d="m4 76 44 25v14L4 90V76Z" fill="#FFCA76" />
      <path d="m48 101 44-25v14l-44 25v-14Z" fill="#FFBB3D" />
    </svg>
  );
}

/** The horizontal symbol-and-wordmark lockup used on product entry points. */
export function BrandLockup({ className, size = 'default', tone = 'brand' }: BrandLockupProps) {
  const compact = size === 'compact';
  const large = size === 'large';

  return (
    <span className={cn('flex items-center gap-3.5', className)}>
      <BrandMark className={cn(compact ? 'w-8' : large ? 'w-16' : 'w-12')} />
      <span
        className={cn(
          'flex flex-col font-sans uppercase leading-none',
          tone === 'inverse' ? 'text-white' : 'text-brand-navy dark:text-foreground',
        )}
      >
        <span
          className={cn(
            'font-medium tracking-[-0.055em]',
            compact ? 'text-base' : large ? 'text-[1.75rem]' : 'text-[1.45rem]',
          )}
        >
          Record
        </span>
        <span
          className={cn(
            'mt-1 font-light tracking-[0.075em]',
            compact ? 'text-sm' : large ? 'text-[1.45rem]' : 'text-[1.22rem]',
          )}
        >
          Store
        </span>
      </span>
    </span>
  );
}
