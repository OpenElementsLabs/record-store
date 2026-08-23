import type * as React from 'react';

/** The standard heading block for a screen. */
export function PageHeader({
  eyebrow,
  title,
  description,
  actions,
}: {
  /** The kind of thing this screen shows, e.g. `Bucket`. */
  readonly eyebrow?: string;
  readonly title: string;
  readonly description?: string;
  readonly actions?: React.ReactNode;
}) {
  return (
    <header className="flex flex-col gap-4 border-b border-border pb-4 pt-1 sm:flex-row sm:items-end sm:justify-between">
      <div className="space-y-1.5">
        {eyebrow ? <p className="type-eyebrow-accent">{eyebrow}</p> : null}
        <h1 className="type-page-title">{title}</h1>
        {description ? <p className="max-w-2xl type-page-description">{description}</p> : null}
      </div>
      {actions ? (
        <div className="flex items-center gap-2 self-start sm:self-auto">{actions}</div>
      ) : null}
    </header>
  );
}
