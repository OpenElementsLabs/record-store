import type * as React from 'react';

/**
 * A useful empty state.
 *
 * Empty states explain what the screen is for and offer the action that fills
 * it, rather than showing decoration. The optional icon exists for the states
 * that are a refusal rather than an absence — "this cannot be shown safely"
 * reads differently from "there is nothing here yet", and the difference should
 * survive a glance.
 */
export function EmptyState({
  icon: Icon,
  title,
  description,
  action,
}: {
  readonly icon?: React.ComponentType<{ className?: string; 'aria-hidden'?: boolean }>;
  readonly title: string;
  readonly description: string;
  readonly action?: React.ReactNode;
}) {
  return (
    <div className="flex flex-col items-center gap-3 px-6 py-12 text-center">
      {Icon ? <Icon aria-hidden className="size-6 text-ink-subtle" /> : null}
      <p className="text-sm font-medium text-ink">{title}</p>
      <p className="max-w-md text-sm text-ink-muted">{description}</p>
      {action ? <div className="pt-1">{action}</div> : null}
    </div>
  );
}
