import { objectUploadUrl } from '@/lib/api/objects';

/** One object to store. */
export type UploadRequest = {
  readonly bucket: string;
  readonly key: string;
  /**
   * The browser's handle to the file on disk.
   *
   * Transports hand this to the network as-is. Reading it into an `ArrayBuffer`
   * first would bound uploads by the size of the page's heap.
   */
  readonly file: File;
};

/** Bytes handed to the network so far. */
export type UploadProgress = {
  readonly sent: number;
  /** `null` when the transport cannot measure the total, so no percentage is invented. */
  readonly total: number | null;
};

export type UploadResult =
  | { readonly status: 'done' }
  | { readonly status: 'cancelled' }
  | { readonly status: 'failed'; readonly reason: string };

export type UploadObserver = {
  readonly onProgress: (progress: UploadProgress) => void;
  /** Called exactly once, whatever the outcome. */
  readonly onSettled: (result: UploadResult) => void;
};

/**
 * A transfer in flight.
 *
 * An object rather than a bare abort function, so a transport that can suspend
 * a transfer may add `pause`/`resume` later without changing existing callers.
 */
export type UploadHandle = {
  readonly abort: () => void;
};

/**
 * How object bytes reach OES.
 *
 * The queue, progress, retry, and cancellation UI all sit above this one
 * function. Replacing it replaces the transfer strategy and nothing else.
 *
 * `singleRequestUpload` is the only implementation because it is the only one
 * the backend can support from a browser today. A multipart transport would
 * keep this signature: its control calls — create upload, request presigned
 * part URLs, complete or abort — would go to the management API on 7601, and
 * each part body would be sent straight to the S3 API on 7600 with a presigned
 * URL. Part retries, parallelism, upload ids, and part ETags would all live
 * inside the transport, reported outwards as one aggregate progress figure. No
 * long-lived S3 secret would enter the page and no object bytes would be
 * proxied through the console server. That API does not exist yet, so neither
 * does the transport.
 */
export type UploadTransport = (request: UploadRequest, observer: UploadObserver) => UploadHandle;

/**
 * Sends the whole object in one streaming `PUT`.
 *
 * `XMLHttpRequest` is used rather than `fetch` because it is the only transport
 * that reports upload progress reliably across browsers. There is no resume: an
 * interrupted request has to be sent again from the first byte.
 */
export const singleRequestUpload: UploadTransport = ({ bucket, key, file }, observer) => {
  const request = new XMLHttpRequest();
  request.open('PUT', objectUploadUrl(bucket, key), true);
  request.withCredentials = true;
  if (file.type) request.setRequestHeader('content-type', file.type);

  // Progress events fire far more often than the UI needs, so a measurable
  // upload only reports on whole-percent changes.
  let lastPercent = -1;
  request.upload.onprogress = (event) => {
    const total = event.lengthComputable && event.total > 0 ? event.total : null;
    if (total !== null) {
      const percent = Math.floor((event.loaded / total) * 100);
      if (percent === lastPercent) return;
      lastPercent = percent;
    }
    observer.onProgress({ sent: event.loaded, total });
  };
  request.onload = () =>
    observer.onSettled(
      request.status >= 200 && request.status < 300
        ? { status: 'done' }
        : { status: 'failed', reason: describeFailure(request) },
    );
  request.onerror = () =>
    observer.onSettled({ status: 'failed', reason: 'The connection failed.' });
  request.onabort = () => observer.onSettled({ status: 'cancelled' });

  // The `File` itself is the body, so the browser streams it from disk.
  request.send(file);

  return { abort: () => request.abort() };
};

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
  if (request.status === 0) return 'The connection failed.';
  return `The server answered with status ${request.status}.`;
}
