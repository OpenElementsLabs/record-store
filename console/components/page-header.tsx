import type * as React from 'react';

/** The standard heading block for a screen. */
export function PageHeader({
  title,
  description,
  actions,
}: {
  readonly title: string;
  readonly description?: string;
  readonly actions?: React.ReactNode;
}) {
  return (
    <header className="flex flex-col gap-4 border-b border-border pb-4 pt-1 sm:flex-row sm:items-end sm:justify-between">
      <div className="space-y-1.5">
        <h1 className="text-2xl font-semibold tracking-tight text-ink">{title}</h1>
        {description ? <p className="max-w-2xl text-sm text-ink-muted">{description}</p> : null}
      </div>
      {actions ? (
        <div className="flex items-center gap-2 self-start sm:self-auto">{actions}</div>
      ) : null}
    </header>
  );
}
