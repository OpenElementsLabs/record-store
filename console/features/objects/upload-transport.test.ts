import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { singleRequestUpload, type UploadResult } from './upload-transport';

type ProgressListener = (event: {
  loaded: number;
  total: number;
  lengthComputable: boolean;
}) => void;

/**
 * A stand-in for `XMLHttpRequest` that records what the transport handed it.
 *
 * jsdom has no network, and the point of these tests is what the transport
 * passes to the browser rather than what a server does with it.
 */
class FakeXhr {
  static instances: FakeXhr[] = [];

  method = '';
  url = '';
  status = 0;
  responseText = '';
  withCredentials = false;
  body: unknown = undefined;
  readonly headers = new Map<string, string>();
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  readonly upload: { onprogress: ProgressListener | null } = { onprogress: null };

  constructor() {
    FakeXhr.instances.push(this);
  }

  open(method: string, url: string) {
    this.method = method;
    this.url = url;
  }

  setRequestHeader(name: string, value: string) {
    this.headers.set(name, value);
  }

  send(body: unknown) {
    this.body = body;
  }

  abort() {
    this.onabort?.();
  }

  respond(status: number, responseText = '') {
    this.status = status;
    this.responseText = responseText;
    this.onload?.();
  }
}

function only(): FakeXhr {
  expect(FakeXhr.instances).toHaveLength(1);
  return FakeXhr.instances[0] as FakeXhr;
}

/** A file that claims to be large without allocating anything. */
function hugeFile(bytes: number): File {
  const file = new File(['x'], 'backup.tar', { type: 'application/x-tar' });
  Object.defineProperty(file, 'size', { value: bytes });
  return file;
}

let settled: UploadResult[];
let progress: { sent: number; total: number | null }[];

function observer() {
  return {
    onProgress: (report: { sent: number; total: number | null }) => progress.push(report),
    onSettled: (result: UploadResult) => settled.push(result),
  };
}

beforeEach(() => {
  FakeXhr.instances = [];
  settled = [];
  progress = [];
  vi.stubGlobal('XMLHttpRequest', FakeXhr);
});

afterEach(() => vi.unstubAllGlobals());

describe('singleRequestUpload', () => {
  it('streams the file itself instead of reading it into page memory', () => {
    const readAsBuffer = vi.spyOn(Blob.prototype, 'arrayBuffer');
    const readAsText = vi.spyOn(Blob.prototype, 'text');
    const file = hugeFile(8 * 1024 ** 3);

    singleRequestUpload({ bucket: 'uploads', key: 'archive/backup.tar', file }, observer());

    // The request body is the browser's own handle to the file on disk, so an
    // 8 GiB object costs the page nothing. Buffering it first would not.
    expect(only().body).toBe(file);
    expect(readAsBuffer).not.toHaveBeenCalled();
    expect(readAsText).not.toHaveBeenCalled();
  });

  it('sends one PUT to the console origin with the file’s content type', () => {
    const file = new File(['hello'], 'notes.txt', { type: 'text/plain' });

    singleRequestUpload({ bucket: 'uploads', key: 'docs/notes.txt', file }, observer());

    const request = only();
    expect(request.method).toBe('PUT');
    // Same-origin path: the console server holds the management credential.
    expect(request.url).toBe('/api/oes/v1/buckets/uploads/object/docs/notes.txt');
    expect(request.withCredentials).toBe(true);
    expect(request.headers.get('content-type')).toBe('text/plain');
  });

  it('reports measured progress once per whole percent', () => {
    singleRequestUpload({ bucket: 'uploads', key: 'a', file: hugeFile(1_000) }, observer());
    const emit = only().upload.onprogress;

    emit?.({ loaded: 100, total: 1_000, lengthComputable: true });
    emit?.({ loaded: 101, total: 1_000, lengthComputable: true });
    emit?.({ loaded: 200, total: 1_000, lengthComputable: true });

    // 10%, still 10%, then 20%: the middle event changes nothing on screen.
    expect(progress).toEqual([
      { sent: 100, total: 1_000 },
      { sent: 200, total: 1_000 },
    ]);
  });

  it('reports an unmeasurable transfer without a total to invent a percentage from', () => {
    singleRequestUpload({ bucket: 'uploads', key: 'a', file: hugeFile(1_000) }, observer());
    const emit = only().upload.onprogress;

    emit?.({ loaded: 4_096, total: 0, lengthComputable: false });
    emit?.({ loaded: 8_192, total: 0, lengthComputable: false });

    expect(progress).toEqual([
      { sent: 4_096, total: null },
      { sent: 8_192, total: null },
    ]);
  });

  it('settles as done on a 2xx answer', () => {
    singleRequestUpload({ bucket: 'uploads', key: 'a', file: hugeFile(1) }, observer());
    only().respond(200);

    expect(settled).toEqual([{ status: 'done' }]);
  });

  it('surfaces the management API’s own message on a rejected upload', () => {
    singleRequestUpload({ bucket: 'uploads', key: 'a', file: hugeFile(1) }, observer());
    only().respond(507, JSON.stringify({ error: { message: 'Bucket quota exceeded' } }));

    expect(settled).toEqual([{ status: 'failed', reason: 'Bucket quota exceeded' }]);
  });

  it('explains a status an intermediary answered with a non-JSON body', () => {
    singleRequestUpload({ bucket: 'uploads', key: 'a', file: hugeFile(1) }, observer());
    only().respond(502, '<html>Bad gateway</html>');

    expect(settled).toEqual([{ status: 'failed', reason: 'The server answered with status 502.' }]);
  });

  it('settles as failed when the connection drops mid-transfer', () => {
    singleRequestUpload({ bucket: 'uploads', key: 'a', file: hugeFile(1) }, observer());
    const request = only();
    request.upload.onprogress?.({ loaded: 500, total: 1_000, lengthComputable: true });
    request.onerror?.();

    // There is no resume: a dropped connection is a failed upload, not a paused one.
    expect(settled).toEqual([{ status: 'failed', reason: 'The connection failed.' }]);
  });

  it('settles as cancelled when the handle is aborted', () => {
    const handle = singleRequestUpload(
      { bucket: 'uploads', key: 'a', file: hugeFile(1) },
      observer(),
    );

    handle.abort();

    expect(settled).toEqual([{ status: 'cancelled' }]);
  });
});
