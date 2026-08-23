'use client';

import { Eye, EyeOff, TriangleAlert } from 'lucide-react';
import * as React from 'react';

import { CopyButton } from '@/components/copy-button';
import { Button } from '@/components/ui/button';

/**
 * Shows a secret that the backend returns exactly once.
 *
 * The value starts masked, is revealed only on explicit action, and is never
 * written to storage or the console log. Once the surrounding screen unmounts
 * the value is gone, which is why the warning is prominent.
 */
export function SecretReveal({
  label,
  value,
  description,
}: {
  readonly label: string;
  readonly value: string;
  readonly description?: string;
}) {
  const [revealed, setRevealed] = React.useState(false);
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between gap-2">
        <span className="type-label">{label}</span>
        <div className="flex items-center gap-1">
          <Button
            size="sm"
            variant="ghost"
            onClick={() => setRevealed((current) => !current)}
            aria-pressed={revealed}
          >
            {revealed ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
            <span>{revealed ? 'Hide' : 'Reveal'}</span>
          </Button>
          <CopyButton value={value} label={label} />
        </div>
      </div>
      <p
        className="break-all rounded-[--radius-control] border border-border bg-surface-muted px-3 py-2 font-mono text-xs text-ink"
        data-testid="secret-value"
      >
        {revealed ? value : '•'.repeat(Math.min(value.length, 48))}
      </p>
      {description ? <p className="type-meta">{description}</p> : null}
    </div>
  );
}

/** The standing warning shown next to a one-time secret. */
export function SecretOnceWarning({ what }: { readonly what: string }) {
  return (
    <div className="flex items-start gap-2 rounded-[--radius-control] border border-warn/40 bg-warn-soft px-3 py-2">
      <TriangleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-warn" />
      <p className="text-xs text-ink">
        This {what} will not be shown again. Copy it now and store it somewhere safe.
      </p>
    </div>
  );
}
