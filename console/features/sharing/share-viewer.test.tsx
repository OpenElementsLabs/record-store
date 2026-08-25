import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ShareViewer } from './share-viewer';
import type { PublicShare } from '@/types/api';

const TOKEN = 'AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-abc';

const openShare: PublicShare = {
  state: 'open',
  file_name: 'annual-report.pdf',
  content_type: 'application/pdf',
  size: 4_096,
  preview: 'pdf',
  can_view: true,
  can_download: true,
  expires_at: '2026-09-01T10:00:00Z',
  preview_text_limit_bytes: 1024 * 1024,
};

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('ShareViewer', () => {
  it('shows the file and its two actions, and nothing about OES itself', () => {
    const { container } = render(<ShareViewer token={TOKEN} initial={openShare} />);

    expect(screen.getByRole('heading', { name: 'annual-report.pdf' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Download' }).getAttribute('href')).toBe(
      `/s/${TOKEN}/content?download=true`,
    );
    expect(screen.getByTitle('Preview of annual-report.pdf')).toBeTruthy();

    // A recipient is not an administrator. Nothing about buckets, keys,
    // versions, or navigation belongs on this page.
    expect(screen.queryByRole('navigation')).toBeNull();
    const text = container.textContent ?? '';
    for (const internal of ['bucket', 'Bucket', 'version', 'NodeId', 'cluster']) {
      expect(text).not.toContain(internal);
    }
  });

  it('discloses nothing at all behind a password challenge', () => {
    const { container } = render(
      <ShareViewer token={TOKEN} initial={{ state: 'password_required' }} />,
    );

    expect(screen.getByRole('heading', { name: /password protected/i })).toBeTruthy();
    expect(screen.getByLabelText('Password')).toBeTruthy();
    // Not even the file name: telling someone what they are being asked to
    // unlock is most of what an attacker wanted.
    expect(container.textContent).not.toContain('annual-report.pdf');
    expect(screen.queryByRole('link', { name: 'Download' })).toBeNull();
  });

  it('reveals the file only after the password is verified', async () => {
    fetchMock
      .mockResolvedValueOnce(
        new Response(JSON.stringify({ ticket: 'ticket-value', expires_in_seconds: 43_200 }), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      )
      .mockResolvedValueOnce(
        new Response(JSON.stringify(openShare), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      );

    render(<ShareViewer token={TOKEN} initial={{ state: 'password_required' }} />);
    await userEvent.type(screen.getByLabelText('Password'), 'correct horse battery');
    await userEvent.click(screen.getByRole('button', { name: 'Unlock' }));

    expect(await screen.findByRole('heading', { name: 'annual-report.pdf' })).toBeTruthy();
    // The descriptor is re-read with the ticket attached, so the file name
    // arrives from the server after verification rather than being held back
    // in the browser.
    const second = fetchMock.mock.calls[1];
    expect(String(second?.[0])).toBe(`/s/${TOKEN}/descriptor`);
    expect(new Headers((second?.[1] as RequestInit).headers).get('x-oes-share-ticket')).toBe(
      'ticket-value',
    );
  });

  it('reports a wrong password without saying whether the link exists', async () => {
    fetchMock.mockResolvedValue(new Response(null, { status: 401 }));

    render(<ShareViewer token={TOKEN} initial={{ state: 'password_required' }} />);
    await userEvent.type(screen.getByLabelText('Password'), 'wrong');
    await userEvent.click(screen.getByRole('button', { name: 'Unlock' }));

    expect(await screen.findByRole('alert')).toHaveProperty(
      'textContent',
      'That password is not correct.',
    );
  });

  it('tells a throttled visitor when to try again', async () => {
    fetchMock.mockResolvedValue(
      new Response(null, { status: 429, headers: { 'retry-after': '45' } }),
    );

    render(<ShareViewer token={TOKEN} initial={{ state: 'password_required' }} />);
    await userEvent.type(screen.getByLabelText('Password'), 'guess');
    await userEvent.click(screen.getByRole('button', { name: 'Unlock' }));

    await waitFor(() => expect(screen.getByRole('alert').textContent).toContain('45 seconds'));
  });

  it('offers only a download when the share does not permit viewing', () => {
    render(<ShareViewer token={TOKEN} initial={{ ...openShare, can_view: false }} />);

    expect(screen.getByText(/chose not to show it in the browser/)).toBeTruthy();
    expect(screen.queryByTitle(/Preview of/)).toBeNull();
    expect(screen.getAllByRole('link', { name: 'Download' }).length).toBeGreaterThan(0);
  });

  it('offers no download when the share is view only', () => {
    render(<ShareViewer token={TOKEN} initial={{ ...openShare, can_download: false }} />);
    expect(screen.queryByRole('link', { name: 'Download' })).toBeNull();
    expect(screen.getByTitle('Preview of annual-report.pdf')).toBeTruthy();
  });

  it('mounts no viewer for a format that cannot be shown safely', () => {
    render(
      <ShareViewer
        token={TOKEN}
        initial={{
          ...openShare,
          file_name: 'page.html',
          content_type: null,
          preview: 'unsafe_inline',
        }}
      />,
    );

    expect(screen.getByText('Preview unavailable')).toBeTruthy();
    expect(screen.getByText(/can carry active content/)).toBeTruthy();
    expect(screen.queryByTitle(/Preview of/)).toBeNull();
  });

  it('says plainly when a link is gone, without saying which way', () => {
    render(<ShareViewer token={TOKEN} initial={null} />);
    // Expired, revoked, exhausted, and never-existed are deliberately one
    // message: distinguishing them would confirm a guess.
    expect(screen.getByText('This link is not available')).toBeTruthy();
    expect(screen.getByText(/expired, been revoked, or never existed/)).toBeTruthy();
  });
});
