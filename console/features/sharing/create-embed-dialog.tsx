'use client';

import { useMutation, useQueryClient } from '@tanstack/react-query';
import { TriangleAlert, X } from 'lucide-react';
import * as React from 'react';

import { CopyButton } from '@/components/copy-button';
import { ErrorState } from '@/components/error-state';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Field } from '@/components/ui/label';
import { embedSnippet } from '@/features/sharing/embed-snippet';
import { queryKeys } from '@/hooks/use-system';
import { absoluteCapabilityUrl, createObjectEmbed } from '@/lib/api/sharing';
import {
  DEFAULT_EXPIRY_ID,
  availableExpiryChoices,
  expiryInstant,
  resolveExpiryChoice,
} from '@/lib/capability-expiry';
import { keyBasename } from '@/lib/format';
import { isElementEmbeddable, previewKind } from '@/lib/preview-kind';
import type { EmbedDisposition, IssuedEmbed, SharingSettings } from '@/types/api';

/**
 * Creates an embed link for one object.
 *
 * Embeds are for other people's pages, so the dialog is about two things a share
 * dialog never asks: which version an application should resolve to, and which
 * sites may load the bytes. A media type that cannot be rendered safely inline
 * is refused here rather than at delivery, because an administrator should not
 * be able to publish stored HTML as an application by pasting one snippet.
 */
export function CreateEmbedDialog({
  bucket,
  objectKey,
  contentType,
  versionId,
  settings,
  open,
  onOpenChange,
}: {
  readonly bucket: string;
  readonly objectKey: string;
  readonly contentType: string | null;
  readonly versionId?: string | undefined;
  readonly settings: SharingSettings;
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        {open ? (
          <EmbedForm
            bucket={bucket}
            objectKey={objectKey}
            contentType={contentType}
            versionId={versionId}
            settings={settings}
            onClose={() => onOpenChange(false)}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function EmbedForm({
  bucket,
  objectKey,
  contentType,
  versionId,
  settings,
  onClose,
}: {
  readonly bucket: string;
  readonly objectKey: string;
  readonly contentType: string | null;
  readonly versionId?: string | undefined;
  readonly settings: SharingSettings;
  readonly onClose: () => void;
}) {
  const client = useQueryClient();
  const choices = availableExpiryChoices(
    settings.maximum_lifetime_days,
    settings.require_expiration,
  );
  const kind = previewKind(contentType);
  const inlinePossible = isElementEmbeddable(kind);

  const [label, setLabel] = React.useState(keyBasename(objectKey));
  const [disposition, setDisposition] = React.useState<EmbedDisposition>(
    inlinePossible ? 'inline' : 'attachment',
  );
  const [pinned, setPinned] = React.useState(versionId !== undefined);
  const [expiryId, setExpiryId] = React.useState(
    choices.some((choice) => choice.id === DEFAULT_EXPIRY_ID)
      ? DEFAULT_EXPIRY_ID
      : (choices[0]?.id ?? DEFAULT_EXPIRY_ID),
  );
  const [origins, setOrigins] = React.useState<readonly string[]>([]);
  const [originDraft, setOriginDraft] = React.useState('');
  const [originError, setOriginError] = React.useState<string | null>(null);
  const [issued, setIssued] = React.useState<IssuedEmbed | null>(null);

  const creation = useMutation({
    mutationFn: () => {
      const choice = resolveExpiryChoice(choices, expiryId);
      return createObjectEmbed(bucket, objectKey, {
        label,
        disposition,
        allowedOrigins: origins,
        versionId: pinned ? (versionId ?? null) : null,
        expiresAt: choice ? expiryInstant(choice) : null,
      });
    },
    onSuccess: async (result) => {
      setIssued(result);
      await client.invalidateQueries({
        queryKey: queryKeys.objectEmbeds(bucket, objectKey),
      });
    },
  });

  function addOrigin() {
    const candidate = originDraft.trim();
    if (candidate.length === 0) return;
    // The backend validates authoritatively; this catches the obvious mistakes
    // without the operator having to submit the form to learn about them.
    if (!/^https?:\/\/[^/?#\s@\\]+$/i.test(candidate)) {
      setOriginError('Write an origin as https://example.com, with no path.');
      return;
    }
    if (origins.includes(candidate)) {
      setOriginError('That origin is already listed.');
      return;
    }
    setOrigins([...origins, candidate]);
    setOriginDraft('');
    setOriginError(null);
  }

  if (issued) {
    return (
      <IssuedEmbedView
        issued={issued}
        onClose={() => {
          setIssued(null);
          onClose();
        }}
      />
    );
  }

  return (
    <>
      <DialogHeader>
        <DialogTitle>Create an embed link</DialogTitle>
        <DialogDescription>
          An embed link lets a website or application load {keyBasename(objectKey)} directly. It is
          read-only, revocable, and never a storage credential.
        </DialogDescription>
      </DialogHeader>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          creation.mutate();
        }}
      >
        <DialogBody className="max-h-[60vh] overflow-y-auto">
          {!inlinePossible ? (
            <div className="flex items-start gap-2 rounded-control border border-warn/40 bg-warn-soft px-3 py-2">
              <TriangleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-warn" />
              <p className="text-xs text-ink">
                {contentType ?? 'This object'} cannot be rendered inline safely, so this embed will
                be served as a download. Record Store supports inline embeds for{' '}
                {settings.embeddable_content_types.join(', ')}.
              </p>
            </div>
          ) : null}

          <Field
            label="Name"
            htmlFor="embed-label"
            hint="Where this embed is used, so you can recognise it later."
          >
            <Input
              value={label}
              onChange={(event) => setLabel(event.target.value)}
              maxLength={120}
              required
            />
          </Field>

          <fieldset className="space-y-2">
            <legend className="type-label">Version</legend>
            <label className="flex cursor-pointer items-start gap-3 rounded-control border border-border p-3 hover:bg-surface-muted has-[:checked]:border-accent has-[:checked]:bg-accent-soft">
              <input
                type="radio"
                name="embed-version"
                checked={!pinned}
                onChange={() => setPinned(false)}
                className="mt-1 accent-[var(--color-accent)]"
              />
              <span className="min-w-0">
                <span className="block type-body">Current version</span>
                <span className="block type-meta">
                  Replacing the object updates every page using this embed. Right for a logo.
                </span>
              </span>
            </label>
            <label
              className={`flex items-start gap-3 rounded-control border border-border p-3 has-[:checked]:border-accent has-[:checked]:bg-accent-soft ${
                versionId ? 'cursor-pointer hover:bg-surface-muted' : 'opacity-60'
              }`}
            >
              <input
                type="radio"
                name="embed-version"
                checked={pinned}
                disabled={versionId === undefined}
                onChange={() => setPinned(true)}
                className="mt-1 accent-[var(--color-accent)]"
              />
              <span className="min-w-0">
                <span className="block type-body">Exact version</span>
                <span className="block type-meta">
                  {versionId
                    ? 'These bytes forever, whatever happens to the key. Right for a published asset.'
                    : 'Open a specific version from the Versions tab to pin it.'}
                </span>
              </span>
            </label>
          </fieldset>

          <Field label="Expires" htmlFor="embed-expiry">
            <select
              value={expiryId}
              onChange={(event) => setExpiryId(event.target.value)}
              className="h-10 w-full rounded-control border border-border-strong bg-surface px-2 type-body"
            >
              {choices.map((choice) => (
                <option key={choice.id} value={choice.id}>
                  {choice.label}
                </option>
              ))}
            </select>
          </Field>

          <div className="space-y-2 rounded-control border border-border p-3">
            <span className="type-label">Allowed origins</span>
            <p className="type-meta">
              Restricting origins stops a leaked URL from rendering on someone else&rsquo;s site. It
              is a narrowing, not the security boundary: the unguessable, revocable token is.
            </p>
            <div className="flex gap-2">
              <Input
                value={originDraft}
                placeholder="https://example.com"
                aria-label="Origin to allow"
                aria-invalid={originError ? true : undefined}
                onChange={(event) => {
                  setOriginDraft(event.target.value);
                  setOriginError(null);
                }}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') {
                    event.preventDefault();
                    addOrigin();
                  }
                }}
              />
              <Button type="button" variant="secondary" onClick={addOrigin}>
                Add
              </Button>
            </div>
            {originError ? (
              <p className="text-xs text-danger" role="alert">
                {originError}
              </p>
            ) : null}
            {origins.length > 0 ? (
              <ul className="flex flex-wrap gap-2">
                {origins.map((origin) => (
                  <li key={origin}>
                    <Badge tone="accent" className="gap-1 pr-1">
                      <span className="font-mono">{origin}</span>
                      <button
                        type="button"
                        aria-label={`Remove ${origin}`}
                        onClick={() => setOrigins(origins.filter((entry) => entry !== origin))}
                        className="rounded-full p-0.5 hover:bg-surface"
                      >
                        <X aria-hidden className="size-3" />
                      </button>
                    </Badge>
                  </li>
                ))}
              </ul>
            ) : (
              <p className="type-meta-subtle">
                No origins listed. Any site holding the URL will be able to load it.
              </p>
            )}
          </div>

          {inlinePossible ? (
            <fieldset className="space-y-2">
              <legend className="type-label">Delivery</legend>
              <label className="flex cursor-pointer items-start gap-3 rounded-control border border-border p-3 hover:bg-surface-muted has-[:checked]:border-accent has-[:checked]:bg-accent-soft">
                <input
                  type="radio"
                  name="embed-disposition"
                  checked={disposition === 'inline'}
                  onChange={() => setDisposition('inline')}
                  className="mt-1 accent-[var(--color-accent)]"
                />
                <span className="min-w-0">
                  <span className="block type-body">Render in place</span>
                  <span className="block type-meta">
                    For an <code>&lt;img&gt;</code>, <code>&lt;video&gt;</code>, or{' '}
                    <code>&lt;audio&gt;</code> element.
                  </span>
                </span>
              </label>
              <label className="flex cursor-pointer items-start gap-3 rounded-control border border-border p-3 hover:bg-surface-muted has-[:checked]:border-accent has-[:checked]:bg-accent-soft">
                <input
                  type="radio"
                  name="embed-disposition"
                  checked={disposition === 'attachment'}
                  onChange={() => setDisposition('attachment')}
                  className="mt-1 accent-[var(--color-accent)]"
                />
                <span className="min-w-0">
                  <span className="block type-body">Download</span>
                  <span className="block type-meta">
                    The browser saves the file instead of displaying it.
                  </span>
                </span>
              </label>
            </fieldset>
          ) : null}

          {creation.error ? <ErrorState error={creation.error} /> : null}
        </DialogBody>
        <DialogFooter>
          <Button type="button" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            type="submit"
            variant="primary"
            disabled={label.trim().length === 0 || creation.isPending}
          >
            {creation.isPending ? 'Creating…' : 'Create embed'}
          </Button>
        </DialogFooter>
      </form>
    </>
  );
}

function IssuedEmbedView({
  issued,
  onClose,
}: {
  readonly issued: IssuedEmbed;
  readonly onClose: () => void;
}) {
  const url = absoluteCapabilityUrl(issued.url);
  const snippet =
    issued.embed.disposition === 'inline' ? embedSnippet(url, issued.embed.content_type) : null;
  return (
    <>
      <DialogHeader>
        <DialogTitle>Embed link created</DialogTitle>
        <DialogDescription>
          Paste this into the site that needs it. Revoke it here to stop it working.
        </DialogDescription>
      </DialogHeader>
      <DialogBody className="max-h-[60vh] overflow-y-auto">
        <div className="space-y-2">
          <span className="type-label">URL</span>
          <p className="break-all rounded-control border border-border bg-surface-muted px-3 py-2 font-mono text-xs text-ink">
            {url}
          </p>
          <CopyButton value={url} label="embed URL" />
        </div>
        {snippet ? (
          <div className="space-y-2">
            <span className="type-label">HTML</span>
            {/* Wide snippets scroll inside their own box rather than stretching
                the dialog on a narrow screen. */}
            <pre className="overflow-x-auto rounded-control border border-border bg-surface-muted px-3 py-2 font-mono text-xs text-ink">
              {snippet.code}
            </pre>
            <CopyButton value={snippet.code} label="embed HTML" />
          </div>
        ) : null}
      </DialogBody>
      <DialogFooter>
        <Button variant="primary" onClick={onClose}>
          Done
        </Button>
      </DialogFooter>
    </>
  );
}
