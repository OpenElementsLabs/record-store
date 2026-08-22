'use client';

import { CircleCheck, CircleX, Upload, X } from 'lucide-react';
import * as React from 'react';

import { Button } from '@/components/ui/button';
import { Card, CardHeader, CardTitle } from '@/components/ui/card';
import type { UploadTask } from '@/features/objects/upload-manager';
import { formatBytes } from '@/lib/format';

/** Shows the upload queue with per-file progress and retry-safe cancellation. */
export function UploadPanel({
  tasks,
  onCancel,
  onClear,
}: {
  readonly tasks: readonly UploadTask[];
  readonly onCancel: (id: string) => void;
  readonly onClear: () => void;
}) {
  if (tasks.length === 0) return null;
  const settled = tasks.filter((task) => task.state !== 'uploading' && task.state !== 'queued');

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
          const percent = task.size > 0 ? Math.round((task.sent / task.size) * 100) : 0;
          return (
            <li key={task.id} className="space-y-1.5 px-4 py-2.5">
              <div className="flex items-center gap-2">
                <StateIcon state={task.state} />
                <span className="min-w-0 flex-1 truncate text-sm text-ink" title={task.key}>
                  {task.name}
                </span>
                <span className="shrink-0 text-xs tabular-nums text-ink-muted">
                  {task.state === 'uploading'
                    ? `${formatBytes(task.sent)} of ${formatBytes(task.size)}`
                    : formatBytes(task.size)}
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
                ) : null}
              </div>
              {task.state === 'uploading' ? (
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
              ) : null}
              {task.error ? (
                <p className="text-xs text-danger" role="alert">
                  {task.error}
                </p>
              ) : null}
            </li>
          );
        })}
      </ul>
    </Card>
  );
}

function StateIcon({ state }: { readonly state: UploadTask['state'] }) {
  if (state === 'done') return <CircleCheck aria-label="Uploaded" className="size-4 text-ok" />;
  if (state === 'error') return <CircleX aria-label="Failed" className="size-4 text-danger" />;
  if (state === 'cancelled') return <X aria-label="Cancelled" className="size-4 text-ink-subtle" />;
  return <Upload aria-label="Uploading" className="size-4 text-ink-muted" />;
}
