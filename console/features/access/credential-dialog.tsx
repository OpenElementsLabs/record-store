'use client';

import * as React from 'react';

import { CopyButton } from '@/components/copy-button';
import { SecretOnceWarning, SecretReveal } from '@/components/secret-reveal';
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
import type { IssuedCredential } from '@/types/api';

/**
 * Presents a credential the backend returns exactly once.
 *
 * The dialog cannot be dismissed by clicking away, so the secret is not lost by
 * accident, and nothing here is written to storage.
 */
export function CredentialDialog({
  issued,
  onClose,
  title,
  description,
}: {
  readonly issued: IssuedCredential | null;
  readonly onClose: () => void;
  readonly title: string;
  readonly description: string;
}) {
  // Nothing is rendered until a credential exists, so the body below mounts
  // fresh for each issuance and its acknowledgement starts unchecked.
  if (!issued) return null;
  return (
    <CredentialBody issued={issued} onClose={onClose} title={title} description={description} />
  );
}

function CredentialBody({
  issued,
  onClose,
  title,
  description,
}: {
  readonly issued: IssuedCredential;
  readonly onClose: () => void;
  readonly title: string;
  readonly description: string;
}) {
  const [acknowledged, setAcknowledged] = React.useState(false);

  const environment = [
    `AWS_ACCESS_KEY_ID=${issued.credential.key_id}`,
    `AWS_SECRET_ACCESS_KEY=${issued.secret_access_key}`,
  ].join('\n');

  return (
    <Dialog open onOpenChange={(open) => !open && acknowledged && onClose()}>
      <DialogContent
        onPointerDownOutside={(event) => event.preventDefault()}
        onEscapeKeyDown={(event) => event.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <SecretOnceWarning what="secret key" />
          <div className="space-y-1.5">
            <div className="flex items-center justify-between gap-2">
              <span className="type-label">Access key ID</span>
              <CopyButton value={issued.credential.key_id} label="access key ID" />
            </div>
            <p className="break-all rounded-control border border-border bg-surface-muted px-3 py-2 font-mono text-xs text-ink">
              {issued.credential.key_id}
            </p>
          </div>
          <SecretReveal label="Secret access key" value={issued.secret_access_key} />
          <div>
            <CopyButton value={environment} label="environment variables" variant="secondary" />
          </div>
          <label className="flex items-start gap-2 text-xs text-ink">
            <input
              type="checkbox"
              checked={acknowledged}
              onChange={(event) => setAcknowledged(event.target.checked)}
              className="mt-0.5"
            />
            I have stored the secret key somewhere safe.
          </label>
        </DialogBody>
        <DialogFooter>
          <Button variant="primary" disabled={!acknowledged} onClick={onClose}>
            Done
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
