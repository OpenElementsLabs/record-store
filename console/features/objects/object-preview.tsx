'use client';

import { Download, Maximize2 } from 'lucide-react';
import * as React from 'react';

import { EmptyState } from '@/components/empty-state';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { objectContentUrl, objectPreviewUrl } from '@/lib/api/objects';
import { previewKind } from '@/lib/preview-kind';
import { formatBytes } from '@/lib/format';
import type { ObjectSummary } from '@/types/api';

export function ObjectPreview({
  bucket,
  record,
}: {
  readonly bucket: string;
  readonly record: ObjectSummary | null;
}) {
  if (record === null) return <Card className="h-40 animate-pulse bg-surface-muted" />;

  const kind = previewKind(record.content_type);
  const previewUrl = objectPreviewUrl(bucket, record.key, record.version_id);
  const downloadUrl = objectContentUrl(bucket, record.key);

  return (
    <Card>
      <CardHeader className="flex-col items-start gap-1">
        <div className="flex w-full items-center justify-between gap-3">
          <div>
            <CardTitle>Preview</CardTitle>
            <CardDescription>{record.content_type ?? 'Unknown content type'}</CardDescription>
          </div>
          <Button asChild variant="secondary" size="sm">
            <a href={downloadUrl} download>
              <Download aria-hidden /> Download
            </a>
          </Button>
        </div>
      </CardHeader>
      <CardContent>
        {kind === 'image' ? (
          <div className="relative flex min-h-64 items-center justify-center overflow-auto rounded-inner bg-surface-muted p-4">
            {/* Direct object bytes preserve authenticated streaming and range behavior. */}
            {/* eslint-disable-next-line @next/next/no-img-element */}
            <img
              src={previewUrl}
              alt={record.key.split('/').at(-1) ?? 'Object preview'}
              className="max-h-[65vh] max-w-full object-contain"
            />
            <Maximize2 aria-hidden className="absolute right-3 top-3 size-4 text-ink-subtle" />
          </div>
        ) : kind === 'video' ? (
          <video
            controls
            preload="metadata"
            className="max-h-[65vh] w-full rounded-inner bg-black"
            src={previewUrl}
          />
        ) : kind === 'audio' ? (
          <audio controls preload="metadata" className="w-full" src={previewUrl} />
        ) : kind === 'pdf' ? (
          <iframe
            title={`Preview of ${record.key}`}
            src={previewUrl}
            className="h-[65vh] w-full rounded-inner border border-border"
          />
        ) : kind === 'text' || kind === 'json' ? (
          <TextPreview url={previewUrl} kind={kind} size={record.size} />
        ) : (
          <EmptyState
            title="Preview unavailable"
            description={`${record.content_type ?? 'application/octet-stream'} · ${formatBytes(record.size)}. This object type cannot be previewed safely.`}
          />
        )}
      </CardContent>
    </Card>
  );
}

function TextPreview({
  url,
  kind,
  size,
}: {
  readonly url: string;
  readonly kind: 'text' | 'json';
  readonly size: number;
}) {
  const [state, setState] = React.useState<{ text: string; error: boolean } | null>(null);
  React.useEffect(() => {
    let cancelled = false;
    void fetch(url, { headers: { Range: 'bytes=0-1048575' } })
      .then((response) => (response.ok ? response.text() : Promise.reject(new Error('preview'))))
      .then((text) => {
        if (!cancelled) {
          let output = text;
          if (kind === 'json') {
            try {
              output = JSON.stringify(JSON.parse(text), null, 2);
            } catch {
              // Invalid JSON remains safely escaped plain text.
            }
          }
          setState({ text: output, error: false });
        }
      })
      .catch(() => {
        if (!cancelled) setState({ text: '', error: true });
      });
    return () => {
      cancelled = true;
    };
  }, [kind, url]);

  if (state?.error)
    return (
      <EmptyState title="Preview failed" description="The object could not be read right now." />
    );
  if (state === null) return <div className="h-48 animate-pulse rounded-inner bg-surface-muted" />;
  return (
    <div>
      <pre className="max-h-[65vh] overflow-auto whitespace-pre-wrap break-words rounded-inner bg-surface-muted p-4 font-mono text-sm">
        {state.text}
      </pre>
      {size > 1024 * 1024 ? (
        <p className="mt-3 type-meta">
          Showing the first 1 MiB of this object. Download the file to view the complete content.
        </p>
      ) : null}
    </div>
  );
}
