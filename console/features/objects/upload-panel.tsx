'use client';

import { CircleCheck, CircleX, RotateCcw, Upload, X } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { Card, CardHeader, CardTitle } from '@/components/ui/card';
import type { UploadTask } from '@/features/objects/upload-manager';
import { formatBytes } from '@/lib/format';

/**
 * Shows the upload queue with per-file progress, cancellation, and retry.
 *
 * Progress is only drawn when the transport reports a total to measure against;
 * otherwise the row says it is uploading rather than showing a percentage of an
 * unknown quantity. A retry restarts the transfer, and the panel says so, since
 * nothing here resumes from a byte offset.
 */
export function UploadPanel({
  tasks,
  onCancel,
  onRetry,
  onClear,
}: {
  readonly tasks: readonly UploadTask[];
  readonly onCancel: (id: string) => void;
  readonly onRetry: (id: string) => void;
  readonly onClear: () => void;
}) {
  if (tasks.length === 0) return null;
  const settled = tasks.filter((task) => task.state !== 'uploading' && task.state !== 'queued');
  const restartable = tasks.some((task) => task.state === 'failed' && task.retryable);

  return (
    <Card>
      <CardHeader>
        <CardTitle>Uploads</CardTitle>
        {settled.length > 0 ? (
          <Button size="sm" variant="ghost" onClick={onClear}>
            Clear finished
          </Button>
        ) : null}
      </CardHeader>
      <ul className="divide-y divide-border">
        {tasks.map((task) => {
          // A total the transport has actually reported is the only basis for a
          // percentage, so an unmeasured upload gets none.
          const measured =
            task.state === 'uploading' && task.total !== null && task.total > 0 ? task.total : null;
          const percent =
            measured === null ? null : Math.min(100, Math.round((task.sent / measured) * 100));
          return (
            <li key={task.id} className="space-y-1.5 px-4 py-2.5">
              <div className="flex items-center gap-2">
                <StateIcon state={task.state} />
                <span className="min-w-0 flex-1 truncate type-body" title={task.key}>
                  {task.name}
                </span>
                <span className="shrink-0 text-xs tabular-nums text-ink-muted">
                  {task.state !== 'uploading'
                    ? formatBytes(task.size)
                    : measured === null
                      ? 'Uploading…'
                      : `${formatBytes(task.sent)} of ${formatBytes(measured)} · ${percent}%`}
                </span>
                {task.state === 'uploading' || task.state === 'queued' ? (
                  <Button
                    size="icon"
                    variant="ghost"
                    aria-label={`Cancel upload of ${task.name}`}
                    onClick={() => onCancel(task.id)}
                  >
                    <X aria-hidden />
                  </Button>
                ) : task.retryable ? (
                  <Button
                    size="sm"
                    variant="secondary"
                    aria-label={`Upload ${task.name} again from the beginning`}
                    onClick={() => onRetry(task.id)}
                  >
                    <RotateCcw aria-hidden />
                    Retry
                  </Button>
                ) : null}
              </div>
              {percent === null ? null : (
                <div
                  className="h-1 w-full overflow-hidden rounded-full bg-surface-muted"
                  role="progressbar"
                  aria-label={`Upload progress for ${task.name}`}
                  aria-valuenow={percent}
                  aria-valuemin={0}
                  aria-valuemax={100}
                >
                  <div className="h-full bg-accent" style={{ width: `${percent}%` }} />
                </div>
              )}
              {task.state === 'failed' ? (
                <p className="text-xs text-danger" role="alert">
                  Upload failed. {task.reason}
                </p>
              ) : null}
              {task.state === 'cancelled' ? (
                <p className="type-meta-subtle">Upload cancelled.</p>
              ) : null}
            </li>
          );
        })}
      </ul>
      {restartable ? (
        <p className="border-t border-border px-4 py-2 type-meta-subtle">
          A retry sends the whole file again from the beginning. Uploads do not resume from where
          they stopped.
        </p>
      ) : null}
    </Card>
  );
}

function StateIcon({ state }: { readonly state: UploadTask['state'] }) {
  if (state === 'done') return <CircleCheck aria-label="Uploaded" className="size-4 text-ok" />;
  if (state === 'failed') return <CircleX aria-label="Failed" className="size-4 text-danger" />;
  if (state === 'cancelled') return <X aria-label="Cancelled" className="size-4 text-ink-subtle" />;
  return <Upload aria-label="Uploading" className="size-4 text-ink-muted" />;
}
