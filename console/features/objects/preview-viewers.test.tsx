import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ImageViewer, TextViewer, UnsupportedPreview } from './preview-viewers';

let fetchMock: ReturnType<typeof vi.fn>;

function textResponse(body: string, status = 206): Response {
  return new Response(body, { status, headers: { 'content-type': 'text/plain' } });
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('TextViewer', () => {
  it('reads a bounded slice rather than the whole object', async () => {
    // "Render this file" must not mean "read four gigabytes into the tab".
    fetchMock.mockResolvedValue(textResponse('hello'));
    render(<TextViewer url="/preview" kind="text" size={64} limitBytes={1024} />);

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const headers = new Headers((fetchMock.mock.calls[0]?.[1] as RequestInit).headers);
    expect(headers.get('Range')).toBe('bytes=0-1023');
    expect(await screen.findByText('hello')).toBeTruthy();
  });

  it('says when it is showing only the first part of an object', async () => {
    fetchMock.mockResolvedValue(textResponse('the beginning'));
    render(<TextViewer url="/preview" kind="text" size={10_000_000} limitBytes={1024 * 1024} />);

    // Silently truncating would be the one outcome worse than not showing it.
    expect(await screen.findByRole('status')).toHaveProperty(
      'textContent',
      expect.stringContaining('Showing the first'),
    );
  });

  it('stays quiet when the whole object fits', async () => {
    fetchMock.mockResolvedValue(textResponse('short'));
    render(<TextViewer url="/preview" kind="text" size={5} limitBytes={1024} />);

    await screen.findByText('short');
    expect(screen.queryByRole('status')).toBeNull();
  });

  it('renders text as characters, never as markup', async () => {
    fetchMock.mockResolvedValue(textResponse('<script>alert(1)</script><img src=x onerror=1>'));
    const { container } = render(
      <TextViewer url="/preview" kind="text" size={40} limitBytes={1024} />,
    );

    expect(await screen.findByText(/<script>alert\(1\)<\/script>/)).toBeTruthy();
    expect(container.querySelector('script')).toBeNull();
    expect(container.querySelector('img')).toBeNull();
  });

  it('formats valid JSON and falls back to text for invalid JSON', async () => {
    fetchMock.mockResolvedValue(textResponse('{"a":1}'));
    const { unmount } = render(
      <TextViewer url="/preview" kind="json" size={7} limitBytes={1024} />,
    );
    expect(await screen.findByText(/"a": 1/)).toBeTruthy();
    unmount();

    fetchMock.mockResolvedValue(textResponse('{"a":'));
    render(<TextViewer url="/preview2" kind="json" size={5} limitBytes={1024} />);
    // Invalid JSON says something about the file, not about OES's storage.
    expect(await screen.findByText(/not valid JSON/)).toBeTruthy();
    expect(screen.queryByText(/corrupt/i)).toBeNull();
  });

  it('explains that a truncated JSON slice is why it is unparsed', async () => {
    fetchMock.mockResolvedValue(textResponse('{"a": 1, "b": [1, 2'));
    render(<TextViewer url="/preview" kind="json" size={5_000_000} limitBytes={1024} />);
    expect(await screen.findByText(/the slice is incomplete/)).toBeTruthy();
  });

  it('reports a failed read without implying the object is gone', async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 500 }));
    render(<TextViewer url="/preview" kind="text" size={10} limitBytes={1024} />);
    expect(await screen.findByText('This object could not be read right now')).toBeTruthy();
    expect(screen.getByText(/The object is still stored/)).toBeTruthy();
  });
});

describe('ImageViewer', () => {
  it('offers keyboard-reachable zoom controls with labels', async () => {
    render(<ImageViewer url="/preview" alt="photo.png" size={2048} />);

    const zoomIn = screen.getByRole('button', { name: 'Zoom in' });
    expect(screen.getByRole('button', { name: 'Zoom out' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Reset zoom' })).toBeTruthy();
    expect(screen.getByRole('group', { name: 'Image zoom' })).toBeTruthy();

    // The current level is announced rather than only drawn.
    expect(screen.getByText('100%')).toBeTruthy();
    await userEvent.click(zoomIn);
    expect(screen.getByText('150%')).toBeTruthy();
    await userEvent.click(screen.getByRole('button', { name: 'Reset zoom' }));
    expect(screen.getByText('100%')).toBeTruthy();
  });

  it('loads the bytes through an element rather than into memory', () => {
    const { container } = render(<ImageViewer url="/preview" alt="photo.png" size={2048} />);
    const image = container.querySelector('img');
    expect(image?.getAttribute('src')).toBe('/preview');
    expect(image?.getAttribute('alt')).toBe('photo.png');
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe('UnsupportedPreview', () => {
  it('names the type and size, and distinguishes refusal from ignorance', () => {
    const { rerender } = render(
      <UnsupportedPreview
        kind="unsupported"
        contentType="application/octet-stream"
        size={4_800_000_000}
      />,
    );
    expect(screen.getByText(/application\/octet-stream · 4.80 GB/)).toBeTruthy();
    expect(screen.getByText(/cannot be previewed safely/)).toBeTruthy();

    rerender(<UnsupportedPreview kind="unsafe_inline" contentType="text/html" size={512} />);
    // "We will not show this" is a different message from "we do not know what
    // this is", and the reader deserves the accurate one.
    expect(screen.getByText(/can carry active content/)).toBeTruthy();
    expect(screen.getByText(/somewhere isolated/)).toBeTruthy();
  });
});
