'use client';

/**
 * The viewers OES is willing to mount over stored bytes.
 *
 * Each one is deliberately small and deliberately dumb. Stored objects are
 * untrusted content, so nothing here parses markup, injects HTML, or hands bytes
 * to a rendering library: an image goes to `<img>`, media goes to the browser's
 * own player, a PDF goes to a sandboxed frame, and text goes into a `<pre>` as
 * escaped characters. The interesting decisions are all about what is *not*
 * done.
 *
 * These viewers are shared by the console's object detail screen and the public
 * share page, which is why they take a URL rather than an object: the two
 * screens authorize very differently and agree only on the bytes.
 */

import { AlertTriangle, Maximize2, Minus, Plus, RotateCcw } from 'lucide-react';
import * as React from 'react';

import { EmptyState } from '@/components/empty-state';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { formatBytes } from '@/lib/format';
import type { PreviewKind } from '@/lib/preview-kind';

/** Zoom stops, so the control is predictable rather than continuous. */
const ZOOM_STEPS = [0.25, 0.5, 0.75, 1, 1.5, 2, 3, 4] as const;
const DEFAULT_ZOOM_INDEX = 3;

/**
 * An image, sized to the viewport until someone asks for more.
 *
 * Zoom is applied with a CSS transform rather than by changing the element's
 * width. The browser keeps one decoded copy of the bitmap either way, which
 * matters for the large originals OES stores: re-laying out a 12000px-wide
 * photograph on every zoom step would be an avoidable stall, and this milestone
 * deliberately previews the original rather than generating a derivative.
 */
export function ImageViewer({
  url,
  alt,
  size,
}: {
  readonly url: string;
  readonly alt: string;
  readonly size: number;
}) {
  const [zoomIndex, setZoomIndex] = React.useState<number>(DEFAULT_ZOOM_INDEX);
  const [state, setState] = React.useState<'loading' | 'ready' | 'failed'>('loading');
  const zoom = ZOOM_STEPS[zoomIndex] ?? 1;

  if (state === 'failed') {
    return <ViewerError title="This image could not be loaded" />;
  }

  return (
    <figure className="space-y-3">
      <div className="relative flex max-h-[65vh] min-h-64 items-center justify-center overflow-auto rounded-inner bg-surface-muted p-4">
        {state === 'loading' ? <Skeleton className="absolute inset-4 h-auto w-auto" /> : null}
        {/*
          A direct element load keeps the bytes out of JavaScript memory: the
          browser streams them, and a very large original costs the page nothing
          beyond what the decoder needs.
        */}
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img
          src={url}
          alt={alt}
          onLoad={() => setState('ready')}
          onError={() => setState('failed')}
          style={{ transform: `scale(${zoom})` }}
          className="max-h-[60vh] max-w-full origin-center object-contain transition-transform"
        />
      </div>
      <figcaption className="flex flex-wrap items-center gap-2">
        <div className="flex items-center gap-1" role="group" aria-label="Image zoom">
          <Button
            size="icon"
            variant="secondary"
            aria-label="Zoom out"
            disabled={zoomIndex === 0}
            onClick={() => setZoomIndex((index) => Math.max(0, index - 1))}
          >
            <Minus aria-hidden />
          </Button>
          <span className="min-w-14 text-center tabular-nums type-meta" aria-live="polite">
            {Math.round(zoom * 100)}%
          </span>
          <Button
            size="icon"
            variant="secondary"
            aria-label="Zoom in"
            disabled={zoomIndex === ZOOM_STEPS.length - 1}
            onClick={() => setZoomIndex((index) => Math.min(ZOOM_STEPS.length - 1, index + 1))}
          >
            <Plus aria-hidden />
          </Button>
          <Button
            size="sm"
            variant="ghost"
            aria-label="Reset zoom"
            disabled={zoomIndex === DEFAULT_ZOOM_INDEX}
            onClick={() => setZoomIndex(DEFAULT_ZOOM_INDEX)}
          >
            <RotateCcw aria-hidden />
            Reset
          </Button>
        </div>
        <span className="ml-auto type-meta-subtle">{formatBytes(size)}</span>
      </figcaption>
    </figure>
  );
}

/**
 * The browser's own video player.
 *
 * Native rather than a framework: it already handles play, pause, seeking,
 * volume, fullscreen, captions, and keyboard and screen-reader access, and every
 * one of those would have to be rebuilt — and rebuilt accessibly — to gain
 * nothing. `preload="metadata"` means opening an object's page fetches a header,
 * not twenty gigabytes, and seeking then works through ordinary range requests.
 */
export function VideoViewer({ url, poster }: { readonly url: string; readonly poster?: string }) {
  const [failed, setFailed] = React.useState(false);
  if (failed) return <ViewerError title="This video could not be played" />;
  return (
    <video
      controls
      preload="metadata"
      playsInline
      onError={() => setFailed(true)}
      className="max-h-[65vh] w-full rounded-inner bg-black"
      src={url}
      {...(poster ? { poster } : {})}
    />
  );
}

/** A compact audio player, using the browser's native transport. */
export function AudioViewer({ url }: { readonly url: string }) {
  const [failed, setFailed] = React.useState(false);
  if (failed) return <ViewerError title="This audio could not be played" />;
  return (
    <div className="rounded-inner bg-surface-muted p-4">
      <audio
        controls
        preload="metadata"
        onError={() => setFailed(true)}
        className="w-full"
        src={url}
      />
    </div>
  );
}

/**
 * A PDF, rendered by the browser inside an isolated frame.
 *
 * The bytes are served with a `sandbox` content policy, which drops the document
 * into an opaque origin: the browser's viewer still renders it, and anything the
 * document itself tries to do — script, form submission, top-level navigation —
 * has no origin to act against. That is why the frame points at a bytes route
 * rather than at anything that could be mistaken for an application page.
 */
export function PdfViewer({ url, title }: { readonly url: string; readonly title: string }) {
  return (
    <iframe
      title={`Preview of ${title}`}
      src={url}
      className="h-[70vh] w-full rounded-inner border border-border bg-surface-muted"
    />
  );
}

/**
 * Text and JSON, as escaped characters.
 *
 * The slice is fetched with a range request and its size is bounded, because
 * "render this file" must not mean "read four gigabytes into the tab". When the
 * object is longer than the slice the reader is told so explicitly; silently
 * truncating would be the one outcome worse than not showing it at all.
 */
export function TextViewer({
  url,
  kind,
  size,
  limitBytes,
}: {
  readonly url: string;
  readonly kind: 'text' | 'json';
  readonly size: number;
  readonly limitBytes: number;
}) {
  type Slice =
    | {
        readonly url: string;
        readonly status: 'ready';
        readonly text: string;
        readonly parsed: boolean;
      }
    | { readonly url: string; readonly status: 'failed' };

  const [slice, setSlice] = React.useState<Slice | null>(null);

  React.useEffect(() => {
    const controller = new AbortController();
    const last = Math.max(0, limitBytes - 1);
    void fetch(url, {
      headers: { Range: `bytes=0-${last}` },
      credentials: 'same-origin',
      signal: controller.signal,
    })
      .then((response) => {
        if (!response.ok && response.status !== 206) throw new Error('preview');
        return response.text();
      })
      .then((text) => {
        if (kind !== 'json') {
          setSlice({ url, status: 'ready', text, parsed: false });
          return;
        }
        try {
          // A truncated slice is not valid JSON, and neither is a file that was
          // never valid. Both fall back to the raw text rather than being
          // reported as corrupt storage, because neither says anything about
          // the object's integrity.
          const formatted = JSON.stringify(JSON.parse(text) as unknown, null, 2);
          setSlice({ url, status: 'ready', text: formatted, parsed: true });
        } catch {
          setSlice({ url, status: 'ready', text, parsed: false });
        }
      })
      .catch((error: unknown) => {
        if (error instanceof DOMException && error.name === 'AbortError') return;
        setSlice({ url, status: 'failed' });
      });
    return () => controller.abort();
  }, [kind, limitBytes, url]);

  // Derived rather than reset in the effect: a slice belongs to the URL it was
  // read from, so pointing this viewer at a different version shows a loading
  // state without a second render pass to clear the old one.
  const current = slice?.url === url ? slice : null;

  if (current === null) {
    return <Skeleton className="h-64 w-full" />;
  }
  if (current.status === 'failed') {
    return <ViewerError title="This object could not be read right now" />;
  }

  const truncated = size > limitBytes;
  return (
    <div className="space-y-3">
      <pre className="max-h-[65vh] overflow-auto whitespace-pre-wrap break-words rounded-inner bg-surface-muted p-4 font-mono text-sm text-ink">
        {current.text}
      </pre>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        {truncated ? (
          <p className="type-meta" role="status">
            Showing the first {formatBytes(limitBytes)} of this object. Download the file to view
            the complete content.
          </p>
        ) : null}
        {kind === 'json' && !current.parsed ? (
          <p className="type-meta-subtle">
            {truncated
              ? 'Shown as plain text because the slice is incomplete.'
              : 'Shown as plain text because this object is not valid JSON.'}
          </p>
        ) : null}
      </div>
    </div>
  );
}

/**
 * The state for objects OES will not render.
 *
 * It names the media type and the size, says plainly that the type cannot be
 * shown safely, and offers the download. A broken viewer would communicate less
 * while looking like a defect.
 */
export function UnsupportedPreview({
  kind,
  contentType,
  size,
  action,
}: {
  readonly kind: PreviewKind;
  readonly contentType: string | null;
  readonly size: number;
  readonly action?: React.ReactNode;
}) {
  const unsafe = kind === 'unsafe_inline';
  return (
    <EmptyState
      icon={unsafe ? AlertTriangle : Maximize2}
      title="Preview unavailable"
      description={
        unsafe
          ? `${contentType ?? 'This format'} · ${formatBytes(size)}. This format can carry active content, so OES will not display it in the console. Download it to inspect it somewhere isolated.`
          : `${contentType ?? 'application/octet-stream'} · ${formatBytes(size)}. This object type cannot be previewed safely.`
      }
      action={action}
    />
  );
}

function ViewerError({ title }: { readonly title: string }) {
  return (
    <EmptyState
      icon={AlertTriangle}
      title={title}
      description="The object is still stored. Reload the page, or download it to check the bytes directly."
    />
  );
}
