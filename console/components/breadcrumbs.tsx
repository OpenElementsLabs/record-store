import { ChevronRight } from 'lucide-react';
import Link from 'next/link';

export type Crumb = {
  readonly label: string;
  /** Omitted for the current page, which is not a link. */
  readonly href?: string;
};

/** A trail of ancestor links for nested screens. */
export function Breadcrumbs({ items }: { readonly items: readonly Crumb[] }) {
  if (items.length === 0) return null;
  return (
    <nav aria-label="Breadcrumb">
      <ol className="flex flex-wrap items-center gap-1 type-meta">
        {items.map((item, index) => {
          const last = index === items.length - 1;
          return (
            <li key={`${item.label}-${index}`} className="flex items-center gap-1">
              {item.href && !last ? (
                <Link href={item.href} className="hover:text-ink hover:underline">
                  {item.label}
                </Link>
              ) : (
                <span
                  className={last ? 'font-medium text-ink' : undefined}
                  aria-current={last ? 'page' : undefined}
                >
                  {item.label}
                </span>
              )}
              {last ? null : <ChevronRight aria-hidden className="size-3 text-ink-subtle" />}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
