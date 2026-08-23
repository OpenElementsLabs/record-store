import { act, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { useUploadManager } from './upload-manager';
import { UploadPanel } from './upload-panel';
import type { UploadObserver, UploadRequest, UploadTransport } from './upload-transport';

type Attempt = { readonly request: UploadRequest; readonly observer: UploadObserver };

/**
 * A transport whose outcome each test decides.
 *
 * Injecting one is the same seam a multipart transport would occupy, so these
 * tests exercise the queue, progress, cancellation, and retry behaviour without
 * any assumption that a transfer is one request.
 */
function recordingTransport() {
  const attempts: Attempt[] = [];
  const transport: UploadTransport = (request, observer) => {
    attempts.push({ request, observer });
    return { abort: () => observer.onSettled({ status: 'cancelled' }) };
  };
  return { attempts, transport };
}

function Harness({
  transport,
  files,
  concurrency = 1,
  onSettled,
}: {
  readonly transport: UploadTransport;
  readonly files: readonly File[];
  readonly concurrency?: number;
  readonly onSettled?: () => void;
}) {
  const uploads = useUploadManager(transport, concurrency);
  if (onSettled) uploads.setSettledHandler(onSettled);
  return (
    <>
      <button type="button" onClick={() => uploads.enqueue('uploads', 'docs/', files)}>
        Add files
      </button>
      <p data-testid="active">{uploads.active ? 'active' : 'idle'}</p>
      <UploadPanel
        tasks={uploads.tasks}
        onCancel={uploads.cancel}
        onRetry={uploads.retry}
        onClear={uploads.clearFinished}
      />
    </>
  );
}

function file(name: string, bytes = 4_000): File {
  const created = new File(['x'], name, { type: 'text/plain' });
  Object.defineProperty(created, 'size', { value: bytes });
  return created;
}

function row(name: string): HTMLElement {
  return screen.getByTitle(`docs/${name}`).closest('li') as HTMLElement;
}

let harness: ReturnType<typeof recordingTransport>;

beforeEach(() => {
  harness = recordingTransport();
});

async function add(files: readonly File[]) {
  render(<Harness transport={harness.transport} files={files} />);
  await userEvent.click(screen.getByRole('button', { name: 'Add files' }));
}

describe('useUploadManager', () => {
  it('hands the file itself to the transport under the browsed prefix', async () => {
    const uploaded = file('notes.txt');
    await add([uploaded]);

    expect(harness.attempts).toHaveLength(1);
    expect(harness.attempts[0]?.request).toEqual({
      bucket: 'uploads',
      key: 'docs/notes.txt',
      file: uploaded,
    });
  });

  it('shows measured progress as bytes and a percentage', async () => {
    await add([file('notes.txt')]);
    act(() => harness.attempts[0]?.observer.onProgress({ sent: 1_000, total: 4_000 }));

    expect(within(row('notes.txt')).getByText(/· 25%/)).toBeTruthy();
    expect(within(row('notes.txt')).getByRole('progressbar').getAttribute('aria-valuenow')).toBe(
      '25',
    );
  });

  it('says it is uploading rather than inventing a percentage it cannot measure', async () => {
    await add([file('notes.txt')]);
    act(() => harness.attempts[0]?.observer.onProgress({ sent: 8_192, total: null }));

    expect(within(row('notes.txt')).getByText('Uploading…')).toBeTruthy();
    expect(within(row('notes.txt')).queryByRole('progressbar')).toBeNull();
  });

  it('states plainly that an upload failed, and why', async () => {
    await add([file('notes.txt')]);
    act(() =>
      harness.attempts[0]?.observer.onSettled({
        status: 'failed',
        reason: 'The connection failed.',
      }),
    );

    expect(within(row('notes.txt')).getByRole('alert').textContent).toBe(
      'Upload failed. The connection failed.',
    );
    expect(screen.getByTestId('active').textContent).toBe('idle');
  });

  it('restarts a failed upload from the beginning when retried', async () => {
    const uploaded = file('notes.txt');
    await add([uploaded]);
    act(() => harness.attempts[0]?.observer.onProgress({ sent: 3_000, total: 4_000 }));
    act(() =>
      harness.attempts[0]?.observer.onSettled({
        status: 'failed',
        reason: 'The connection failed.',
      }),
    );

    await userEvent.click(
      within(row('notes.txt')).getByRole('button', {
        name: 'Upload notes.txt again from the beginning',
      }),
    );

    // A second attempt with the same file, and no memory of the 3 kB that the
    // first attempt had already sent.
    expect(harness.attempts).toHaveLength(2);
    expect(harness.attempts[1]?.request.file).toBe(uploaded);
    expect(within(row('notes.txt')).queryByRole('alert')).toBeNull();
    expect(within(row('notes.txt')).queryByText(/· 75%/)).toBeNull();

    act(() => harness.attempts[1]?.observer.onProgress({ sent: 400, total: 4_000 }));
    expect(within(row('notes.txt')).getByText(/· 10%/)).toBeTruthy();

    act(() => harness.attempts[1]?.observer.onSettled({ status: 'done' }));
    expect(within(row('notes.txt')).getByLabelText('Uploaded')).toBeTruthy();
  });

  it('never describes an upload as resumable', async () => {
    await add([file('notes.txt')]);
    act(() =>
      harness.attempts[0]?.observer.onSettled({
        status: 'failed',
        reason: 'The connection failed.',
      }),
    );

    expect(
      screen.getByText(/A retry sends the whole file again from the beginning\./),
    ).toBeTruthy();
    expect(document.body.textContent ?? '').not.toMatch(
      /resumable|resumes|resuming|pick up where/i,
    );
  });

  it('offers no retry once an upload has succeeded', async () => {
    await add([file('notes.txt')]);
    act(() => harness.attempts[0]?.observer.onSettled({ status: 'done' }));

    expect(
      within(row('notes.txt')).queryByRole('button', { name: /again from the beginning/ }),
    ).toBeNull();
  });

  it('cancels a transfer in flight and allows a fresh attempt', async () => {
    await add([file('notes.txt')]);
    await userEvent.click(
      within(row('notes.txt')).getByRole('button', { name: 'Cancel upload of notes.txt' }),
    );

    expect(within(row('notes.txt')).getByText('Upload cancelled.')).toBeTruthy();
    await userEvent.click(
      within(row('notes.txt')).getByRole('button', {
        name: 'Upload notes.txt again from the beginning',
      }),
    );
    expect(harness.attempts).toHaveLength(2);
  });

  it('runs transfers in parallel up to the concurrency bound', async () => {
    const files = ['one.txt', 'two.txt', 'three.txt', 'four.txt', 'five.txt'].map((name) =>
      file(name),
    );
    render(<Harness transport={harness.transport} files={files} concurrency={3} />);
    await userEvent.click(screen.getByRole('button', { name: 'Add files' }));

    // Three start immediately; the remaining two wait for a slot rather than
    // opening their own connections.
    expect(harness.attempts).toHaveLength(3);
    expect(harness.attempts.map((attempt) => attempt.request.key)).toEqual([
      'docs/one.txt',
      'docs/two.txt',
      'docs/three.txt',
    ]);

    await act(async () => {
      harness.attempts[0]?.observer.onSettled({ status: 'done' });
    });
    // One finished, so exactly one more starts.
    expect(harness.attempts).toHaveLength(4);
    expect(harness.attempts[3]?.request.key).toBe('docs/four.txt');
  });

  it('never exceeds the bound however many files are dropped', async () => {
    const files = Array.from({ length: 40 }, (_, index) => file(`f${index}.txt`));
    render(<Harness transport={harness.transport} files={files} concurrency={2} />);
    await userEvent.click(screen.getByRole('button', { name: 'Add files' }));

    expect(harness.attempts).toHaveLength(2);
  });

  it('reports the queue as drained only once every transfer has settled', async () => {
    const settled = vi.fn();
    render(
      <Harness
        transport={harness.transport}
        files={[file('one.txt'), file('two.txt')]}
        concurrency={2}
        onSettled={settled}
      />,
    );
    await userEvent.click(screen.getByRole('button', { name: 'Add files' }));
    expect(harness.attempts).toHaveLength(2);

    await act(async () => {
      harness.attempts[0]?.observer.onSettled({ status: 'done' });
    });
    // One worker is still transferring, so the queue is not drained.
    expect(settled).not.toHaveBeenCalled();

    await act(async () => {
      harness.attempts[1]?.observer.onSettled({ status: 'done' });
    });
    expect(settled).toHaveBeenCalledTimes(1);
  });

  it('drops settled rows on request and keeps the ones still running', async () => {
    await add([file('one.txt'), file('two.txt')]);
    await act(async () => {
      harness.attempts[0]?.observer.onSettled({ status: 'done' });
    });

    await userEvent.click(screen.getByRole('button', { name: 'Clear finished' }));

    expect(screen.queryByTitle('docs/one.txt')).toBeNull();
    expect(screen.getByTitle('docs/two.txt')).toBeTruthy();
  });
});
