'use client';

import { Download, Lock, ShieldCheck } from 'lucide-react';
import * as React from 'react';

import { BrandMark } from '@/components/brand-mark';
import { EmptyState } from '@/components/empty-state';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Field } from '@/components/ui/label';
import {
  AudioViewer,
  ImageViewer,
  PdfViewer,
  TextViewer,
  UnsupportedPreview,
  VideoViewer,
} from '@/features/objects/preview-viewers';
import { formatBytes, formatDateTime } from '@/lib/format';
import type { PreviewKind } from '@/lib/preview-kind';
import type { PublicShare } from '@/types/api';

/**
 * The public share page.
 *
 * Restrained on purpose. This page is opened by someone who was sent a link and
 * who has no relationship with Record Store, so it shows the file, the two things they
 * might want to do with it, and a single line saying where it came from. There
 * is no navigation, because there is nowhere else they are allowed to go.
 *
 * The password challenge deliberately discloses nothing — not even the file
 * name — until the password is verified. Telling someone what they are being
 * asked to unlock is most of what an attacker wanted.
 */
export function ShareViewer({
  token,
  initial,
}: {
  readonly token: string;
  readonly initial: PublicShare | null;
}) {
  const [share, setShare] = React.useState<PublicShare | null>(initial);

  return (
    <main className="mx-auto flex min-h-dvh w-full max-w-4xl flex-col gap-6 px-4 py-8 sm:px-6 sm:py-12">
      <header className="flex items-center gap-3">
        <BrandMark />
        <div className="min-w-0">
          <p className="type-eyebrow">Shared securely through Record Store</p>
        </div>
      </header>

      {share === null ? (
        <Unavailable />
      ) : share.state === 'password_required' ? (
        <PasswordChallenge
          token={token}
          onUnlocked={(unlocked, issuedTicket) => {
            // Written here, in the event handler, rather than from an effect
            // inside the viewer. An `<img>` or a media element cannot send a
            // header, so the bytes route reads the ticket from this cookie — and
            // those elements start loading the moment the viewer mounts. An
            // effect would set the cookie *after* the first request had already
            // gone out without it.
            publishTicket(token, issuedTicket);
            setShare(unlocked);
          }}
        />
      ) : (
        <SharedObject share={share} token={token} />
      )}

      <footer className="mt-auto flex items-center gap-2 border-t border-border pt-4">
        <ShieldCheck aria-hidden className="size-4 text-ink-subtle" />
        <p className="type-meta-subtle">
          This link grants read access to a single file. It can be withdrawn at any time by whoever
          shared it.
        </p>
      </footer>
    </main>
  );
}

function Unavailable() {
  return (
    <div className="rounded-panel border border-border bg-surface">
      <EmptyState
        icon={Lock}
        title="This link is not available"
        description="It may have expired, been revoked, or never existed. Ask whoever shared it for a new one."
      />
    </div>
  );
}

function PasswordChallenge({
  token,
  onUnlocked,
}: {
  readonly token: string;
  readonly onUnlocked: (share: PublicShare, ticket: string) => void;
}) {
  const [password, setPassword] = React.useState('');
  const [error, setError] = React.useState<string | null>(null);
  const [pending, setPending] = React.useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setPending(true);
    setError(null);
    try {
      const response = await fetch(`/s/${token}/unlock`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ password }),
      });
      if (response.status === 429) {
        const retry = response.headers.get('retry-after');
        setError(
          `Too many attempts. Try again in about ${retry ?? '60'} second${retry === '1' ? '' : 's'}.`,
        );
        return;
      }
      if (!response.ok) {
        setError('That password is not correct.');
        return;
      }
      const { ticket } = (await response.json()) as { ticket: string };
      // The descriptor is re-read with the ticket attached, so the file name
      // arrives from the server only after the password has actually been
      // verified rather than having been held back in the browser.
      const descriptor = await fetch(`/s/${token}/descriptor`, {
        headers: { accept: 'application/json', 'x-record-store-share-ticket': ticket },
      });
      if (!descriptor.ok) {
        setError('This link is no longer available.');
        return;
      }
      onUnlocked((await descriptor.json()) as PublicShare, ticket);
    } catch {
      setError('The request could not be completed. Check your connection and try again.');
    } finally {
      setPending(false);
    }
  }

  return (
    <section
      aria-labelledby="share-password-heading"
      className="rounded-panel border border-border bg-surface p-6"
    >
      <div className="mx-auto max-w-sm space-y-4">
        <div className="flex flex-col items-center gap-2 text-center">
          <Lock aria-hidden className="size-6 text-ink-subtle" />
          <h1 id="share-password-heading" className="type-page-title">
            This link is password protected
          </h1>
          <p className="type-page-description">
            Enter the password you were given to see the shared file.
          </p>
        </div>
        <form onSubmit={submit} className="space-y-4">
          <Field label="Password" htmlFor="share-password" error={error}>
            <Input
              type="password"
              value={password}
              autoComplete="off"
              autoFocus
              onChange={(event) => setPassword(event.target.value)}
            />
          </Field>
          <Button
            type="submit"
            variant="primary"
            size="lg"
            className="w-full"
            disabled={pending || password.length === 0}
          >
            {pending ? 'Checking…' : 'Unlock'}
          </Button>
        </form>
      </div>
    </section>
  );
}

function SharedObject({
  share,
  token,
}: {
  readonly share: Extract<PublicShare, { state: 'open' }>;
  readonly token: string;
}) {
  const contentUrl = `/s/${token}/content`;
  const downloadUrl = `${contentUrl}?download=true`;
  const kind = share.preview as PreviewKind;

  return (
    <section className="space-y-4">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0">
          <h1 className="type-page-title break-all">{share.file_name}</h1>
          <p className="type-page-description">
            {share.content_type ?? 'Unknown type'} · {formatBytes(share.size)}
            {share.expires_at ? (
              <>
                {' · '}
                <span>
                  Available until{' '}
                  <time dateTime={share.expires_at}>{formatDateTime(share.expires_at)}</time>
                </span>
              </>
            ) : null}
          </p>
        </div>
        {share.can_download ? (
          <Button asChild variant="primary">
            <a href={downloadUrl} download={share.file_name}>
              <Download aria-hidden />
              Download
            </a>
          </Button>
        ) : null}
      </div>

      <div className="rounded-panel border border-border bg-surface p-4">
        {!share.can_view ? (
          <EmptyState
            icon={Download}
            title="This file is available to download"
            description="Whoever shared it chose not to show it in the browser."
            action={
              share.can_download ? (
                <Button asChild variant="secondary">
                  <a href={downloadUrl} download={share.file_name}>
                    <Download aria-hidden />
                    Download
                  </a>
                </Button>
              ) : undefined
            }
          />
        ) : kind === 'image' ? (
          <ImageViewer url={contentUrl} alt={share.file_name} size={share.size} />
        ) : kind === 'video' ? (
          <VideoViewer url={contentUrl} />
        ) : kind === 'audio' ? (
          <AudioViewer url={contentUrl} />
        ) : kind === 'pdf' ? (
          <PdfViewer url={contentUrl} title={share.file_name} />
        ) : kind === 'text' || kind === 'json' ? (
          <TextViewer
            url={contentUrl}
            kind={kind}
            size={share.size}
            limitBytes={share.preview_text_limit_bytes}
          />
        ) : (
          <UnsupportedPreview
            kind={kind}
            contentType={share.content_type}
            size={share.size}
            action={
              share.can_download ? (
                <Button asChild variant="secondary">
                  <a href={downloadUrl} download={share.file_name}>
                    <Download aria-hidden />
                    Download
                  </a>
                </Button>
              ) : undefined
            }
          />
        )}
      </div>
    </section>
  );
}

/**
 * Publishes the unlock ticket to this share's bytes route.
 *
 * A password-protected share proves itself on every request, and an `<img>`, a
 * media element, or a framed PDF cannot send a header — so the proof travels as
 * a cookie rather than in the URL, where it would land in every log and referrer
 * along the way. Scoped to this one share's path so it is never sent anywhere
 * else, and `SameSite=Strict` so another site cannot cause it to be attached.
 *
 * Not `HttpOnly`, because the page itself sets it after an in-page unlock. That
 * is acceptable precisely because the ticket grants nothing on its own: every
 * request it accompanies re-checks the share's revocation, expiry, permission,
 * and budget against durable state, so it stops working the instant the link is
 * withdrawn.
 */
function publishTicket(token: string, ticket: string): void {
  const secure = window.location.protocol === 'https:' ? '; Secure' : '';
  document.cookie = `record_store_share_ticket=${encodeURIComponent(ticket)}; Path=/s/${token}; SameSite=Strict; Max-Age=43200${secure}`;
}
