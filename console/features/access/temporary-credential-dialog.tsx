'use client';

import * as React from 'react';

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
import { Field } from '@/components/ui/label';
import { TEMPORARY_LIFETIMES } from '@/lib/credential-lifetime';
import type { ServiceAccountInfo } from '@/types/api';

/**
 * Chooses a lifetime for a temporary credential.
 *
 * The lifetime options are the only thing to decide: a temporary credential
 * inherits the account's policies rather than taking its own, so it can never
 * grant more than the account already has.
 */
export function TemporaryCredentialDialog({
  account,
  pending,
  onCancel,
  onIssue,
}: {
  readonly account: ServiceAccountInfo | null;
  readonly pending: boolean;
  readonly onCancel: () => void;
  readonly onIssue: (seconds: number) => void;
}) {
  return (
    <Dialog open={account !== null} onOpenChange={(open) => (open ? undefined : onCancel())}>
      <DialogContent>
        {account === null ? null : (
          <LifetimeForm
            key={account.account.id}
            name={account.account.name}
            pending={pending}
            onCancel={onCancel}
            onIssue={onIssue}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}

function LifetimeForm({
  name,
  pending,
  onCancel,
  onIssue,
}: {
  readonly name: string;
  readonly pending: boolean;
  readonly onCancel: () => void;
  readonly onIssue: (seconds: number) => void;
}) {
  const [seconds, setSeconds] = React.useState<number>(TEMPORARY_LIFETIMES[1].seconds);

  return (
    <>
      <DialogHeader>
        <DialogTitle>Issue a temporary credential</DialogTitle>
        <DialogDescription>
          For {name}. The credential expires on its own and carries the same policies as the
          account, so it grants nothing extra. The secret is shown once.
        </DialogDescription>
      </DialogHeader>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          onIssue(seconds);
        }}
      >
        <DialogBody>
          <Field label="Expires after" htmlFor="temporary-lifetime">
            <select
              id="temporary-lifetime"
              value={seconds}
              onChange={(event) => setSeconds(Number(event.target.value))}
              className="h-9 w-full rounded-control border border-border-strong bg-surface px-2 type-body"
            >
              {TEMPORARY_LIFETIMES.map((option) => (
                <option key={option.seconds} value={option.seconds}>
                  {option.label}
                </option>
              ))}
            </select>
          </Field>
        </DialogBody>
        <DialogFooter>
          <Button type="button" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={pending}>
            {pending ? 'Issuing…' : 'Issue credential'}
          </Button>
        </DialogFooter>
      </form>
    </>
  );
}
