'use client';

import { Download } from 'lucide-react';
import * as React from 'react';

import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import {
  AudioViewer,
  ImageViewer,
  PdfViewer,
  TextViewer,
  UnsupportedPreview,
  VideoViewer,
} from '@/features/objects/preview-viewers';
import { objectContentUrl, objectPreviewUrl } from '@/lib/api/objects';
import { formatBytes, keyBasename } from '@/lib/format';
import { previewKind, previewKindLabel } from '@/lib/preview-kind';
import type { ObjectSummary } from '@/types/api';

/** Fallback slice size, used until the deployment reports its own limit. */
const DEFAULT_TEXT_LIMIT = 1024 * 1024;

/**
 * The console's preview of one stored object.
 *
 * Preview is a different promise from download: download hands over whatever
 * bytes exist, while preview asks the browser to interpret them, so it is
 * offered only for media types Record Store is prepared to be responsible for. The
 * management API refuses the rest, and this screen is honest about which case
 * the reader is looking at instead of mounting a viewer that will fail.
 *
 * A version can be pinned. When one is, every request on this screen names it,
 * so opening an old version never shows the current bytes.
 */
export function ObjectPreview({
  bucket,
  record,
  versionId,
  textLimitBytes = DEFAULT_TEXT_LIMIT,
}: {
  readonly bucket: string;
  readonly record: ObjectSummary | null;
  readonly versionId?: string | undefined;
  readonly textLimitBytes?: number;
}) {
  if (record === null) {
    return (
      <Card>
        <CardHeader className="flex-col items-start gap-1">
          <CardTitle>Preview</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-64 w-full" />
        </CardContent>
      </Card>
    );
  }

  const kind = previewKind(record.content_type);
  // The pinned version is threaded through explicitly rather than defaulting to
  // the record's own version: a caller viewing history must never be silently
  // handed the current bytes.
  const previewUrl = objectPreviewUrl(bucket, record.key, versionId);
  // The download inside the preview card names the same version the viewer is
  // showing. Handing a reader who is looking at history the current bytes would
  // be handing them the wrong file.
  const downloadUrl = objectContentUrl(bucket, record.key, versionId);
  const name = keyBasename(record.key);
  const download = (
    <Button asChild variant="secondary" size="sm">
      <a href={downloadUrl} download>
        <Download aria-hidden /> Download
      </a>
    </Button>
  );

  return (
    <Card>
      <CardHeader className="flex-col items-start gap-3 sm:flex-row sm:items-center">
        <div className="min-w-0">
          <CardTitle>Preview</CardTitle>
          <CardDescription className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span>{record.content_type ?? 'Unknown content type'}</span>
            <span aria-hidden>·</span>
            <span>{formatBytes(record.size)}</span>
          </CardDescription>
        </div>
        <div className="flex items-center gap-2 sm:ml-auto">
          <Badge tone="neutral">{previewKindLabel(kind)}</Badge>
          {versionId ? <Badge tone="info">Historical version</Badge> : null}
          {download}
        </div>
      </CardHeader>
      <CardContent>
        {kind === 'image' ? (
          <ImageViewer url={previewUrl} alt={name} size={record.size} />
        ) : kind === 'video' ? (
          <VideoViewer url={previewUrl} />
        ) : kind === 'audio' ? (
          <AudioViewer url={previewUrl} />
        ) : kind === 'pdf' ? (
          <PdfViewer url={previewUrl} title={name} />
        ) : kind === 'text' || kind === 'json' ? (
          <TextViewer url={previewUrl} kind={kind} size={record.size} limitBytes={textLimitBytes} />
        ) : (
          <UnsupportedPreview
            kind={kind}
            contentType={record.content_type}
            size={record.size}
            action={download}
          />
        )}
      </CardContent>
    </Card>
  );
}
