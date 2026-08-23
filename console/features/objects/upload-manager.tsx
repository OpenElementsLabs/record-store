'use client';

import * as React from 'react';

import {
  singleRequestUpload,
  type UploadHandle,
  type UploadTransport,
} from '@/features/objects/upload-transport';

/** Where an upload has got to. */
export type UploadState = 'queued' | 'uploading' | 'done' | 'failed' | 'cancelled';

export type UploadTask = {
  readonly id: string;
  readonly bucket: string;
  readonly key: string;
  readonly name: string;
  /** The size the browser reports for the file. */
  readonly size: number;
  readonly state: UploadState;
  /** Bytes the transport has handed to the network. */
  readonly sent: number;
  /**
   * The total the transport is reporting, or `null` until it reports one.
   *
   * Kept separate from `size` so the UI can say "Uploading" rather than show a
   * percentage of a total nothing has confirmed.
   */
  readonly total: number | null;
  /** Why the upload stopped, when it failed. */
  readonly reason: string | null;
  /** Whether the same file is still in hand and can be sent again from the start. */
  readonly retryable: boolean;
};

/** What the queue needs to run one transfer, held outside React state. */
type QueuedUpload = { readonly bucket: string; readonly key: string; readonly file: File };

/**
 * How many transfers run at once.
 *
 * Browsers allow roughly six connections per origin, and the console needs some
 * of those for its own API calls while an upload is running. Three keeps a
 * directory drop moving without starving the rest of the page, and the bound
 * exists so a thousand-file selection cannot open a thousand requests.
 */
export const UPLOAD_CONCURRENCY = 3;

/**
 * Runs object uploads.
 *
 * This owns the queue, the per-file state the UI renders, cancellation, and
 * retry. It owns no part of the transfer itself: that is the injected
 * `UploadTransport`, which is the seam a future multipart strategy would
 * replace. Everything here — and everything in `UploadPanel` above it — is
 * written against progress reports and outcomes rather than against the fact
 * that today's transport happens to use a single request.
 *
 * Transfers run up to `concurrency` at a time, so a directory drop makes
 * progress without opening an unbounded number of connections. A retried upload
 * starts from the first byte, because the transport cannot resume; nothing here
 * pretends otherwise.
 */
export function useUploadManager(
  transport: UploadTransport = singleRequestUpload,
  concurrency: number = UPLOAD_CONCURRENCY,
) {
  const [tasks, setTasks] = React.useState<readonly UploadTask[]>([]);
  const handles = React.useRef(new Map<string, UploadHandle>());
  const pending = React.useRef(new Map<string, QueuedUpload>());
  const queue = React.useRef<string[]>([]);
  const running = React.useRef(0);
  const onSettled = React.useRef<(() => void) | null>(null);

  const update = React.useCallback((id: string, patch: Partial<UploadTask>) => {
    setTasks((current) => current.map((task) => (task.id === id ? { ...task, ...patch } : task)));
  }, []);

  const send = React.useCallback(
    (id: string, item: QueuedUpload) =>
      new Promise<void>((resolve) => {
        const handle = transport(
          { bucket: item.bucket, key: item.key, file: item.file },
          {
            onProgress: ({ sent, total }) => update(id, { state: 'uploading', sent, total }),
            onSettled: (result) => {
              handles.current.delete(id);
              if (result.status === 'done') {
                // The file handle is only needed for a retry, and there is none.
                pending.current.delete(id);
                update(id, { state: 'done', reason: null, retryable: false });
              } else if (result.status === 'cancelled') {
                update(id, { state: 'cancelled', reason: null, retryable: true });
              } else {
                update(id, { state: 'failed', reason: result.reason, retryable: true });
              }
              resolve();
            },
          },
        );
        handles.current.set(id, handle);
      }),
    [transport, update],
  );

  /**
   * Drains the queue.
   *
   * The queue holds ids and the files are held beside it, so the loop never
   * reads a rendered snapshot of `tasks` to decide what to send next.
   */
  /**
   * One worker, taking the next queued upload until the queue is empty.
   *
   * The queue holds ids and the files are held beside it, so a worker never
   * reads a rendered snapshot of `tasks` to decide what to send next.
   */
  const drain = React.useCallback(async () => {
    try {
      for (;;) {
        const id = queue.current.shift();
        if (id === undefined) break;
        const item = pending.current.get(id);
        if (!item) continue;
        update(id, { state: 'uploading', sent: 0, total: null, reason: null });
        await send(id, item);
      }
    } finally {
      running.current -= 1;
      // The last worker to finish is the one that drained the queue.
      if (running.current === 0) onSettled.current?.();
    }
  }, [send, update]);

  /** Starts workers up to the concurrency bound. */
  const pump = React.useCallback(() => {
    while (running.current < concurrency && queue.current.length > 0) {
      running.current += 1;
      void drain();
    }
  }, [concurrency, drain]);

  /** Queues files under a prefix, deriving each key from the file name. */
  const enqueue = React.useCallback(
    (bucket: string, prefix: string, incoming: readonly File[]) => {
      const created: UploadTask[] = incoming.map((file) => {
        const id = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
        pending.current.set(id, { bucket, key: `${prefix}${file.name}`, file });
        return {
          id,
          bucket,
          key: `${prefix}${file.name}`,
          name: file.name,
          size: file.size,
          state: 'queued',
          sent: 0,
          total: null,
          reason: null,
          retryable: false,
        };
      });
      setTasks((current) => [...created, ...current]);
      queue.current.push(...created.map((task) => task.id));
      pump();
    },
    [pump],
  );

  /**
   * Sends a settled upload again, from the first byte.
   *
   * No part of an interrupted transfer survives, which is why the control that
   * calls this says as much.
   */
  const retry = React.useCallback(
    (id: string) => {
      if (!pending.current.has(id)) return;
      update(id, { state: 'queued', sent: 0, total: null, reason: null, retryable: false });
      queue.current.push(id);
      pump();
    },
    [pump, update],
  );

  const cancel = React.useCallback((id: string) => {
    // A running transfer settles through its transport's abort path; a queued
    // one never starts, so it is marked here.
    handles.current.get(id)?.abort();
    queue.current = queue.current.filter((queued) => queued !== id);
    setTasks((current) =>
      current.map((task) =>
        task.id === id && task.state === 'queued'
          ? { ...task, state: 'cancelled', retryable: pending.current.has(id) }
          : task,
      ),
    );
  }, []);

  const clearFinished = React.useCallback(() => {
    setTasks((current) => {
      const kept = current.filter((task) => task.state === 'uploading' || task.state === 'queued');
      const keptIds = new Set(kept.map((task) => task.id));
      for (const id of [...pending.current.keys()]) {
        if (!keptIds.has(id)) pending.current.delete(id);
      }
      return kept;
    });
  }, []);

  const setSettledHandler = React.useCallback((handler: (() => void) | null) => {
    onSettled.current = handler;
  }, []);

  const active = tasks.some((task) => task.state === 'queued' || task.state === 'uploading');

  return { tasks, enqueue, cancel, retry, clearFinished, active, setSettledHandler };
}
