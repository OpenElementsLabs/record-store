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
    <header className="flex flex-wrap items-start justify-between gap-3">
      <div className="space-y-1">
        <h1 className="text-lg font-semibold tracking-tight text-ink">{title}</h1>
        {description ? <p className="max-w-2xl text-sm text-ink-muted">{description}</p> : null}
      </div>
      {actions ? <div className="flex items-center gap-2">{actions}</div> : null}
    </header>
  );
}
