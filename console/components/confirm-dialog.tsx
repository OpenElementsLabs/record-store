'use client';

import { TriangleAlert } from 'lucide-react';
import * as React from 'react';

import { ErrorDetails } from '@/components/error-state';
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
import { ApiError } from '@/lib/api/error';

/**
 * How much friction a destructive action requires.
 *
 * The strength is chosen per action so routine deletions stay quick while
 * irreversible loss demands deliberate confirmation.
 */
export type ConfirmStrength =
  /** A single confirming click. Used for reversible or low-blast-radius actions. */
  | 'acknowledge'
  /** The operator must type an exact identifier. Used for irreversible loss. */
  | 'type-to-confirm';

export type ConfirmDialogProps = {
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
  readonly title: string;
  readonly description: string;
  readonly confirmLabel: string;
  readonly strength?: ConfirmStrength;
  /** The exact text the operator must type when `strength` requires it. */
  readonly expectedText?: string;
  readonly consequence?: string;
  readonly pending?: boolean;
  readonly error?: unknown;
  readonly onConfirm: () => void;
};

export function ConfirmDialog(props: ConfirmDialogProps) {
  return (
    <Dialog open={props.open} onOpenChange={props.onOpenChange}>
      <DialogContent>
        {/*
          The body lives inside the dialog content, which Radix mounts only while
          the dialog is open. Its state therefore starts fresh on every open
          without any state being reset from an effect.
        */}
        <ConfirmBody {...props} />
      </DialogContent>
    </Dialog>
  );
}

function ConfirmBody({
  title,
  description,
  confirmLabel,
  strength = 'acknowledge',
  expectedText,
  consequence,
  pending = false,
  error,
  onConfirm,
  onOpenChange,
}: ConfirmDialogProps) {
  const [typed, setTyped] = React.useState('');
  const needsText = strength === 'type-to-confirm' && Boolean(expectedText);
  const satisfied = needsText ? typed === expectedText : true;
  const api = error instanceof ApiError ? error : null;

  return (
    <>
      <DialogHeader>
        <DialogTitle>{title}</DialogTitle>
        <DialogDescription>{description}</DialogDescription>
      </DialogHeader>
      <DialogBody>
        {consequence ? (
          <div className="flex items-start gap-2 rounded-[--radius-control] border border-danger/40 bg-danger-soft px-3 py-2">
            <TriangleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-danger" />
            <p className="text-xs text-ink">{consequence}</p>
          </div>
        ) : null}
        {needsText ? (
          <Field
            label={`Type ${expectedText} to confirm`}
            htmlFor="confirm-text"
            hint="This action cannot be undone."
          >
            <Input
              value={typed}
              autoComplete="off"
              onChange={(event) => setTyped(event.target.value)}
            />
          </Field>
        ) : null}
        {api ? (
          <div className="space-y-1" role="alert">
            <p className="text-xs text-danger">{api.message}</p>
            <ErrorDetails error={api} />
          </div>
        ) : null}
      </DialogBody>
      <DialogFooter>
        <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={pending}>
          Cancel
        </Button>
        <Button variant="danger" onClick={onConfirm} disabled={!satisfied || pending}>
          {pending ? 'Working…' : confirmLabel}
        </Button>
      </DialogFooter>
    </>
  );
}
