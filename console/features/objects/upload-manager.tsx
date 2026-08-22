'use client';

import * as React from 'react';

import { objectUploadUrl } from '@/lib/api/objects';

/** Where an upload has got to. */
export type UploadState = 'queued' | 'uploading' | 'done' | 'error' | 'cancelled';

export type UploadTask = {
  readonly id: string;
  readonly bucket: string;
  readonly key: string;
  readonly name: string;
  readonly size: number;
  readonly state: UploadState;
  /** Bytes the browser has handed to the network. */
  readonly sent: number;
  readonly error: string | null;
};

type Controller = { abort: () => void };

/**
 * Runs object uploads.
 *
 * The `File` is handed to the browser as the request body, so the bytes stream
 * from disk to the network and never pass through JavaScript memory. Progress is
 * read from `XMLHttpRequest`, which is the only transport that reports upload
 * progress reliably across browsers.
 *
 * Files are sent one at a time so a directory drop cannot saturate the link, and
 * the design keeps a single seam — this hook — for a future multipart strategy
 * that would split large files into parallel, resumable parts.
 */
export function useUploadManager() {
  const [tasks, setTasks] = React.useState<readonly UploadTask[]>([]);
  const controllers = React.useRef(new Map<string, Controller>());
  const queue = React.useRef<UploadTask[]>([]);
  const running = React.useRef(false);
  const onSettled = React.useRef<(() => void) | null>(null);

  const update = React.useCallback((id: string, patch: Partial<UploadTask>) => {
    setTasks((current) => current.map((task) => (task.id === id ? { ...task, ...patch } : task)));
  }, []);

  const send = React.useCallback(
    (task: UploadTask, file: File) =>
      new Promise<void>((resolve) => {
        const request = new XMLHttpRequest();
        controllers.current.set(task.id, { abort: () => request.abort() });
        request.open('PUT', objectUploadUrl(task.bucket, task.key), true);
        request.withCredentials = true;
        if (file.type) request.setRequestHeader('content-type', file.type);

        // Progress events fire far more often than the UI needs, so updates are
        // throttled to whole percent to avoid re-rendering on every chunk.
        let lastPercent = -1;
        request.upload.onprogress = (event) => {
          if (!event.lengthComputable) return;
          const percent = Math.floor((event.loaded / event.total) * 100);
          if (percent === lastPercent) return;
          lastPercent = percent;
          update(task.id, { sent: event.loaded, state: 'uploading' });
        };
        request.onload = () => {
          controllers.current.delete(task.id);
          if (request.status >= 200 && request.status < 300) {
            update(task.id, { state: 'done', sent: task.size, error: null });
          } else {
            update(task.id, { state: 'error', error: describeFailure(request) });
          }
          resolve();
        };
        request.onerror = () => {
          controllers.current.delete(task.id);
          update(task.id, { state: 'error', error: 'The upload connection failed.' });
          resolve();
        };
        request.onabort = () => {
          controllers.current.delete(task.id);
          update(task.id, { state: 'cancelled', error: null });
          resolve();
        };
        request.send(file);
      }),
    [update],
  );

  const pump = React.useCallback(async () => {
    if (running.current) return;
    running.current = true;
    try {
      for (;;) {
        const next = queue.current.shift();
        if (!next) break;
        const file = files.current.get(next.id);
        if (!file) continue;
        update(next.id, { state: 'uploading' });
        await send(next, file);
        files.current.delete(next.id);
      }
    } finally {
      running.current = false;
      onSettled.current?.();
    }
  }, [send, update]);

  const files = React.useRef(new Map<string, File>());

  /** Queues files under a prefix, deriving each key from the file name. */
  const enqueue = React.useCallback(
    (bucket: string, prefix: string, incoming: readonly File[]) => {
      const created: UploadTask[] = incoming.map((file) => {
        const id = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
        files.current.set(id, file);
        return {
          id,
          bucket,
          key: `${prefix}${file.name}`,
          name: file.name,
          size: file.size,
          state: 'queued',
          sent: 0,
          error: null,
        };
      });
      setTasks((current) => [...created, ...current]);
      queue.current.push(...created);
      void pump();
    },
    [pump],
  );

  const cancel = React.useCallback((id: string) => {
    controllers.current.get(id)?.abort();
    queue.current = queue.current.filter((task) => task.id !== id);
    files.current.delete(id);
    setTasks((current) =>
      current.map((task) =>
        task.id === id && task.state === 'queued' ? { ...task, state: 'cancelled' } : task,
      ),
    );
  }, []);

  const clearFinished = React.useCallback(() => {
    setTasks((current) =>
      current.filter((task) => task.state === 'uploading' || task.state === 'queued'),
    );
  }, []);

  const setSettledHandler = React.useCallback((handler: (() => void) | null) => {
    onSettled.current = handler;
  }, []);

  const active = tasks.some((task) => task.state === 'queued' || task.state === 'uploading');

  return { tasks, enqueue, cancel, clearFinished, active, setSettledHandler };
}

function describeFailure(request: XMLHttpRequest): string {
  try {
    const body = JSON.parse(request.responseText) as { error?: { message?: string } };
    if (body.error?.message) return body.error.message;
  } catch {
    // A non-JSON body means an intermediary answered; fall through.
  }
  if (request.status === 401) return 'Your session has expired. Sign in again.';
  if (request.status === 403) return 'Your role does not permit uploads.';
  if (request.status === 507) return 'The bucket quota would be exceeded.';
  return `The upload failed with status ${request.status}.`;
}
