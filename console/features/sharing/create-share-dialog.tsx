'use client';

import { useMutation, useQueryClient } from '@tanstack/react-query';
import { TriangleAlert } from 'lucide-react';
import * as React from 'react';

import { CopyButton } from '@/components/copy-button';
import { ErrorState } from '@/components/error-state';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
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
import { queryKeys } from '@/hooks/use-system';
import { absoluteCapabilityUrl, createObjectShare } from '@/lib/api/sharing';
import {
  DEFAULT_EXPIRY_ID,
  availableExpiryChoices,
  expiryInstant,
  resolveExpiryChoice,
} from '@/lib/capability-expiry';
import { keyBasename } from '@/lib/format';
import type { IssuedShare, SharePermission, SharingSettings } from '@/types/api';

const PERMISSIONS: readonly {
  readonly value: SharePermission;
  readonly label: string;
  readonly hint: string;
}[] = [
  { value: 'view', label: 'View only', hint: 'Opens in the browser. No download button.' },
  { value: 'download', label: 'Download only', hint: 'Saves the file. Nothing is shown inline.' },
  {
    value: 'view_and_download',
    label: 'View and download',
    hint: 'Both. The usual choice for a document sent to a colleague.',
  },
];

/**
 * Creates a share link for one object.
 *
 * The dialog offers only what the deployment will accept: if an operator has
 * capped link lifetimes or required passwords, those rules shape the form rather
 * than producing a rejection after the fact.
 */
export function CreateShareDialog({
  bucket,
  objectKey,
  versionId,
  settings,
  open,
  onOpenChange,
}: {
  readonly bucket: string;
  readonly objectKey: string;
  /** Present when the operator opened this from a specific historical version. */
  readonly versionId?: string | undefined;
  readonly settings: SharingSettings;
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        {/*
          Mounted only while open, so every field starts fresh and no effect has
          to remember to clear a password from a previous attempt.
        */}
        {open ? (
          <ShareForm
            bucket={bucket}
            objectKey={objectKey}
            versionId={versionId}
            settings={settings}
            onClose={() => onOpenChange(false)}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function ShareForm({
  bucket,
  objectKey,
  versionId,
  settings,
  onClose,
}: {
  readonly bucket: string;
  readonly objectKey: string;
  readonly versionId?: string | undefined;
  readonly settings: SharingSettings;
  readonly onClose: () => void;
}) {
  const client = useQueryClient();
  const choices = availableExpiryChoices(
    settings.maximum_lifetime_days,
    settings.require_expiration,
  );
  const [label, setLabel] = React.useState(keyBasename(objectKey));
  const [permission, setPermission] = React.useState<SharePermission>('view_and_download');
  const [pinned, setPinned] = React.useState(versionId !== undefined);
  const [expiryId, setExpiryId] = React.useState(
    choices.some((choice) => choice.id === DEFAULT_EXPIRY_ID)
      ? DEFAULT_EXPIRY_ID
      : (choices[0]?.id ?? DEFAULT_EXPIRY_ID),
  );
  const [usePassword, setUsePassword] = React.useState(settings.require_share_password);
  const [password, setPassword] = React.useState('');
  const [useLimit, setUseLimit] = React.useState(false);
  const [limit, setLimit] = React.useState('5');
  const [issued, setIssued] = React.useState<IssuedShare | null>(null);

  const creation = useMutation({
    mutationFn: () => {
      const choice = resolveExpiryChoice(choices, expiryId);
      return createObjectShare(bucket, objectKey, {
        label,
        permission,
        versionId: pinned ? (versionId ?? null) : null,
        expiresAt: choice ? expiryInstant(choice) : null,
        password: usePassword ? password : null,
        maximumAccessCount: useLimit ? Number(limit) : null,
      });
    },
    onSuccess: async (result) => {
      setIssued(result);
      await client.invalidateQueries({
        queryKey: queryKeys.objectShares(bucket, objectKey),
      });
    },
  });

  if (issued) {
    return (
      <IssuedShareView
        issued={issued}
        onClose={() => {
          setIssued(null);
          onClose();
        }}
      />
    );
  }

  const passwordTooShort =
    usePassword && password.length > 0 && password.length < settings.minimum_password_length;
  const canSubmit =
    label.trim().length > 0 &&
    (!usePassword || password.length >= settings.minimum_password_length) &&
    (!useLimit || (Number(limit) >= 1 && Number(limit) <= settings.maximum_access_count));

  return (
    <>
      <DialogHeader>
        <DialogTitle>Create a share link</DialogTitle>
        <DialogDescription>
          A share link lets someone read {keyBasename(objectKey)} without an OES account. It grants
          nothing else, and you can revoke it at any time.
        </DialogDescription>
      </DialogHeader>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          creation.mutate();
        }}
      >
        <DialogBody className="max-h-[60vh] overflow-y-auto">
          <Field
            label="Name"
            htmlFor="share-label"
            hint="Only you see this. It is how you will recognise the link later."
          >
            <Input
              value={label}
              onChange={(event) => setLabel(event.target.value)}
              maxLength={120}
              required
            />
          </Field>

          <fieldset className="space-y-2">
            <legend className="type-label">Access</legend>
            {PERMISSIONS.map((option) => (
              <label
                key={option.value}
                className="flex cursor-pointer items-start gap-3 rounded-control border border-border p-3 hover:bg-surface-muted has-[:checked]:border-accent has-[:checked]:bg-accent-soft"
              >
                <input
                  type="radio"
                  name="share-permission"
                  value={option.value}
                  checked={permission === option.value}
                  onChange={() => setPermission(option.value)}
                  className="mt-1 accent-[var(--color-accent)]"
                />
                <span className="min-w-0">
                  <span className="block type-body">{option.label}</span>
                  <span className="block type-meta">{option.hint}</span>
                </span>
              </label>
            ))}
          </fieldset>

          <fieldset className="space-y-2">
            <legend className="type-label">Version</legend>
            <label className="flex cursor-pointer items-start gap-3 rounded-control border border-border p-3 hover:bg-surface-muted has-[:checked]:border-accent has-[:checked]:bg-accent-soft">
              <input
                type="radio"
                name="share-version"
                checked={!pinned}
                onChange={() => setPinned(false)}
                className="mt-1 accent-[var(--color-accent)]"
              />
              <span className="min-w-0">
                <span className="block type-body">Follow the current version</span>
                <span className="block type-meta">
                  Recipients always see whatever is stored under this key. Right for a logo or a
                  living document.
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
                name="share-version"
                checked={pinned}
                disabled={versionId === undefined}
                onChange={() => setPinned(true)}
                className="mt-1 accent-[var(--color-accent)]"
              />
              <span className="min-w-0">
                <span className="block type-body">Pin this exact version</span>
                <span className="block type-meta">
                  {versionId
                    ? 'Recipients always see these bytes, even after the object is replaced. Right for a signed contract.'
                    : 'Open a specific version from the Versions tab to pin it.'}
                </span>
              </span>
            </label>
          </fieldset>

          <Field
            label="Expires"
            htmlFor="share-expiry"
            hint={
              settings.require_expiration
                ? 'This deployment requires every link to expire.'
                : 'A link with no expiry is one nobody remembers to retire.'
            }
          >
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
            <label className="flex items-center gap-3">
              <Checkbox
                checked={usePassword}
                disabled={settings.require_share_password}
                onCheckedChange={(next) => setUsePassword(next === true)}
              />
              <span className="type-body">Require a password</span>
            </label>
            {usePassword ? (
              <Field
                label="Password"
                htmlFor="share-password"
                hint={`At least ${settings.minimum_password_length} characters. Send it separately from the link.`}
                error={passwordTooShort ? 'Too short.' : null}
              >
                <Input
                  type="password"
                  value={password}
                  autoComplete="new-password"
                  onChange={(event) => setPassword(event.target.value)}
                />
              </Field>
            ) : null}
          </div>

          <div className="space-y-2 rounded-control border border-border p-3">
            <label className="flex items-center gap-3">
              <Checkbox checked={useLimit} onCheckedChange={(next) => setUseLimit(next === true)} />
              <span className="type-body">Limit how many times it can be opened</span>
            </label>
            {useLimit ? (
              <>
                <Field label="Maximum opens" htmlFor="share-limit">
                  <Input
                    type="number"
                    min={1}
                    max={settings.maximum_access_count}
                    value={limit}
                    onChange={(event) => setLimit(event.target.value)}
                  />
                </Field>
                <p className="type-meta-subtle">
                  Counted per delivery and enforced exactly. Limited links send the whole file each
                  time, so seeking within a video or a long PDF is unavailable.
                </p>
              </>
            ) : null}
          </div>

          {creation.error ? <ErrorState error={creation.error} /> : null}
        </DialogBody>
        <DialogFooter>
          <Button type="button" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={!canSubmit || creation.isPending}>
            {creation.isPending ? 'Creating…' : 'Create link'}
          </Button>
        </DialogFooter>
      </form>
    </>
  );
}

function IssuedShareView({
  issued,
  onClose,
}: {
  readonly issued: IssuedShare;
  readonly onClose: () => void;
}) {
  const url = absoluteCapabilityUrl(issued.url);
  return (
    <>
      <DialogHeader>
        <DialogTitle>Share link created</DialogTitle>
        <DialogDescription>
          Anyone with this link can read the object until it expires or you revoke it.
        </DialogDescription>
      </DialogHeader>
      <DialogBody>
        <div className="space-y-2">
          <span className="type-label">Link</span>
          <p className="break-all rounded-control border border-border bg-surface-muted px-3 py-2 font-mono text-xs text-ink">
            {url}
          </p>
          <CopyButton value={url} label="share link" />
        </div>
        {issued.share.password_protected ? (
          <div className="flex items-start gap-2 rounded-control border border-warn/40 bg-warn-soft px-3 py-2">
            <TriangleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-warn" />
            <p className="text-xs text-ink">
              Send the password through a different channel from the link. A message containing both
              is a message containing the file.
            </p>
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
