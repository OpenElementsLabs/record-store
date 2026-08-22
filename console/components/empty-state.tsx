import type * as React from 'react';

/**
 * A useful empty state.
 *
 * Empty states explain what the screen is for and offer the action that fills
 * it, rather than showing decoration.
 */
export function EmptyState({
  title,
  description,
  action,
}: {
  readonly title: string;
  readonly description: string;
  readonly action?: React.ReactNode;
}) {
  return (
    <div className="flex flex-col items-center gap-3 px-6 py-12 text-center">
      <p className="text-sm font-medium text-ink">{title}</p>
      <p className="max-w-md text-sm text-ink-muted">{description}</p>
      {action ? <div className="pt-1">{action}</div> : null}
    </div>
  );
}
