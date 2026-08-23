'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Plus, TriangleAlert, X } from 'lucide-react';
import { useSearchParams } from 'next/navigation';
import * as React from 'react';
import { toast } from 'sonner';

import { isBroad, resourceProblem } from '@/features/access/policy-resource';
import { EmptyState } from '@/components/empty-state';
import { ErrorDetails, ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
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
import { Skeleton, TableSkeleton } from '@/components/ui/skeleton';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import {
  attachPolicy,
  createPolicy,
  detachPolicy,
  fetchPolicies,
  fetchServiceAccounts,
} from '@/lib/api/access';
import { ApiError } from '@/lib/api/error';
import { formatCount, formatDate } from '@/lib/format';
import { readString } from '@/lib/search-params';
import type { Policy, PolicyAction, PolicyEffect, PolicyStatement } from '@/types/api';

const ACTIONS: readonly PolicyAction[] = [
  's3:ListBucket',
  's3:GetObject',
  's3:PutObject',
  's3:DeleteObject',
  's3:GetObjectVersion',
  's3:DeleteObjectVersion',
  's3:ManageBucket',
];

export function PoliciesScreen() {
  const params = useSearchParams();
  const [creating, setCreating] = React.useState(() => readString(params, 'create', '') === '1');
  const policies = useQuery({
    queryKey: queryKeys.policies,
    queryFn: ({ signal }) => fetchPolicies(signal),
  });

  return (
    <>
      <PageHeader
        title="Policies"
        description="Policies grant or deny S3 actions on resources, and are attached to service accounts."
        actions={
          <Button variant="primary" onClick={() => setCreating(true)}>
            <Plus aria-hidden />
            Create policy
          </Button>
        }
      />

      {policies.isError ? (
        <Card>
          <ErrorState error={policies.error} onRetry={() => void policies.refetch()} />
        </Card>
      ) : policies.isPending ? (
        <Card>
          <TableSkeleton columns={3} />
        </Card>
      ) : policies.data.length === 0 ? (
        <Card>
          <EmptyState
            title="No policies"
            description="Create a policy to grant a service account access to specific buckets and actions."
            action={
              <Button variant="primary" onClick={() => setCreating(true)}>
                <Plus aria-hidden />
                Create policy
              </Button>
            }
          />
        </Card>
      ) : (
        <div className="space-y-4">
          {policies.data.map((policy) => (
            <Card key={policy.id}>
              <CardHeader className="flex-col items-start">
                <div className="flex w-full items-start justify-between gap-3">
                  <div>
                    <CardTitle>{policy.name}</CardTitle>
                    {policy.description ? (
                      <p className="mt-0.5 type-meta">{policy.description}</p>
                    ) : null}
                  </div>
                  <span className="shrink-0 type-meta-subtle">
                    Created {formatDate(policy.created_at)}
                  </span>
                </div>
              </CardHeader>
              <CardContent className="space-y-4">
                {policy.statements.map((statement, index) => (
                  <StatementView key={index} statement={statement} />
                ))}
                <PolicyBindings policy={policy} />
              </CardContent>
            </Card>
          ))}
        </div>
      )}

      <CreatePolicyDialog open={creating} onOpenChange={setCreating} />
    </>
  );
}

/**
 * Which service accounts a policy grants to, and the controls to change that.
 *
 * A policy on its own does nothing: it takes effect only where it is bound.
 * Showing the bindings beside the rules is what makes the blast radius of a
 * broad policy visible at the point of reading it.
 */
function PolicyBindings({ policy }: { readonly policy: Policy }) {
  const client = useQueryClient();
  const permissions = usePermissions();
  const [attaching, setAttaching] = React.useState('');

  const accounts = useQuery({
    queryKey: queryKeys.serviceAccounts,
    queryFn: ({ signal }) => fetchServiceAccounts(signal),
  });

  const invalidate = async () => {
    await client.invalidateQueries({ queryKey: queryKeys.serviceAccounts });
    await client.invalidateQueries({ queryKey: queryKeys.policies });
  };

  const attach = useMutation({
    mutationFn: (accountId: string) => attachPolicy(policy.id, accountId),
    onSuccess: async () => {
      toast.success('Policy attached');
      setAttaching('');
      await invalidate();
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : 'Could not attach the policy'),
  });

  const detach = useMutation({
    mutationFn: (accountId: string) => detachPolicy(policy.id, accountId),
    onSuccess: async () => {
      toast.success('Policy detached');
      await invalidate();
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : 'Could not detach the policy'),
  });

  const all = accounts.data ?? [];
  const bound = all.filter((account) => account.policy_bindings.includes(policy.id));
  const unbound = all.filter((account) => !account.policy_bindings.includes(policy.id));
  const busy = attach.isPending || detach.isPending;

  return (
    <div className="space-y-2 border-t border-border pt-3">
      <p className="text-xs font-medium text-ink-muted">
        {/*
          The count is only stated once the accounts are known. Saying "attached
          to no accounts" while the list is still loading would be a false
          statement about who has access.
        */}
        {accounts.isPending
          ? 'Attachments'
          : `Attached to ${
              bound.length === 0
                ? 'no accounts'
                : `${formatCount(bound.length)} account${bound.length === 1 ? '' : 's'}`
            }`}
      </p>
      {accounts.isPending ? (
        <Skeleton className="h-8 w-full" />
      ) : bound.length === 0 ? (
        <p className="type-meta-subtle">
          This policy grants nothing until it is attached to a service account.
        </p>
      ) : (
        <ul className="flex flex-wrap gap-1.5">
          {bound.map((account) => (
            <li key={account.account.id} className="flex items-center gap-1">
              <Badge tone="neutral">{account.account.name}</Badge>
              {permissions.manage_policies ? (
                <Button
                  size="icon"
                  variant="ghost"
                  aria-label={`Detach ${policy.name} from ${account.account.name}`}
                  disabled={busy}
                  onClick={() => detach.mutate(account.account.id)}
                >
                  <X aria-hidden />
                </Button>
              ) : null}
            </li>
          ))}
        </ul>
      )}

      {permissions.manage_policies && unbound.length > 0 ? (
        <div className="flex flex-wrap items-center gap-2 pt-1">
          <label className="sr-only" htmlFor={`attach-${policy.id}`}>
            Attach {policy.name} to a service account
          </label>
          <select
            id={`attach-${policy.id}`}
            value={attaching}
            disabled={busy}
            onChange={(event) => setAttaching(event.target.value)}
            className="h-8 rounded-control border border-border-strong bg-surface px-2 text-xs text-ink"
          >
            <option value="">Attach to…</option>
            {unbound.map((account) => (
              <option key={account.account.id} value={account.account.id}>
                {account.account.name}
              </option>
            ))}
          </select>
          <Button
            size="sm"
            variant="secondary"
            disabled={attaching === '' || busy}
            onClick={() => attach.mutate(attaching)}
          >
            Attach
          </Button>
        </div>
      ) : null}
    </div>
  );
}

function StatementView({ statement }: { readonly statement: PolicyStatement }) {
  const broad = statement.resources.some(isBroad);
  return (
    <div className="space-y-2 rounded-control border border-border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <Badge tone={statement.effect === 'allow' ? 'ok' : 'danger'}>
          {statement.effect === 'allow' ? 'Allow' : 'Deny'}
        </Badge>
        {broad && statement.effect === 'allow' ? (
          <Badge tone="warn">
            <TriangleAlert aria-hidden />
            Broad access
          </Badge>
        ) : null}
      </div>
      <div className="flex flex-wrap gap-1">
        {statement.actions.map((action) => (
          <span
            key={action}
            className="rounded-control border border-border bg-surface-muted px-1.5 py-0.5 font-mono text-[0.6875rem] text-ink-muted"
          >
            {action}
          </span>
        ))}
      </div>
      <div className="flex flex-wrap gap-1">
        {statement.resources.map((resource) => (
          <span key={resource} className="font-mono text-xs text-ink">
            {resource}
          </span>
        ))}
      </div>
    </div>
  );
}

/**
 * A structured policy editor.
 *
 * Effect, actions, and resources are edited as fields rather than as raw JSON,
 * so a policy can be written without knowing the document shape. The backend
 * still validates the result.
 */
function CreatePolicyDialog({
  open,
  onOpenChange,
}: {
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        {/* Mounted only while open, so the editor starts from defaults. */}
        <CreatePolicyForm onOpenChange={onOpenChange} />
      </DialogContent>
    </Dialog>
  );
}

function CreatePolicyForm({ onOpenChange }: { readonly onOpenChange: (open: boolean) => void }) {
  const client = useQueryClient();
  const [name, setName] = React.useState('');
  const [description, setDescription] = React.useState('');
  const [effect, setEffect] = React.useState<PolicyEffect>('allow');
  const [actions, setActions] = React.useState<readonly PolicyAction[]>(['s3:GetObject']);
  const [resources, setResources] = React.useState<readonly string[]>(['bucket:uploads/*']);
  const [resourceDraft, setResourceDraft] = React.useState('');
  const [resourceError, setResourceError] = React.useState<string | null>(null);

  const mutation = useMutation({
    mutationFn: () =>
      createPolicy({
        name,
        description,
        statements: [{ effect, actions, resources }],
      }),
    onSuccess: async () => {
      toast.success('Policy created');
      await client.invalidateQueries({ queryKey: queryKeys.policies });
      onOpenChange(false);
    },
  });

  const error = mutation.error instanceof ApiError ? mutation.error : null;
  const grantsBroadAccess = effect === 'allow' && resources.some(isBroad);
  const valid = name.trim().length > 0 && actions.length > 0 && resources.length > 0;

  return (
    <>
      <DialogHeader>
        <DialogTitle>Create policy</DialogTitle>
        <DialogDescription>
          One statement is created. Explicit denies always take precedence over allows.
        </DialogDescription>
      </DialogHeader>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (valid) mutation.mutate();
        }}
      >
        <DialogBody>
          <Field label="Name" htmlFor="policy-name" error={error?.message ?? null}>
            <Input
              value={name}
              autoComplete="off"
              required
              onChange={(event) => setName(event.target.value)}
            />
          </Field>
          <Field label="Description" htmlFor="policy-description">
            <Input
              value={description}
              autoComplete="off"
              onChange={(event) => setDescription(event.target.value)}
            />
          </Field>

          <fieldset className="space-y-2">
            <legend className="type-label">Effect</legend>
            <div className="flex gap-2">
              {(['allow', 'deny'] as const).map((option) => (
                <label key={option} className="flex items-center gap-1.5 type-body">
                  <input
                    type="radio"
                    name="effect"
                    value={option}
                    checked={effect === option}
                    onChange={() => setEffect(option)}
                  />
                  {option === 'allow' ? 'Allow' : 'Deny'}
                </label>
              ))}
            </div>
          </fieldset>

          <fieldset className="space-y-2">
            <legend className="type-label">Actions</legend>
            <div className="grid gap-1.5 sm:grid-cols-2">
              {ACTIONS.map((action) => (
                <label key={action} className="flex items-center gap-2 text-xs text-ink">
                  <input
                    type="checkbox"
                    checked={actions.includes(action)}
                    onChange={(event) =>
                      setActions((current) =>
                        event.target.checked
                          ? [...current, action]
                          : current.filter((item) => item !== action),
                      )
                    }
                  />
                  <span className="font-mono">{action}</span>
                </label>
              ))}
            </div>
          </fieldset>

          <fieldset className="space-y-2">
            <legend className="type-label">Resources</legend>
            <ul className="space-y-1">
              {resources.map((resource) => (
                <li key={resource} className="flex items-center gap-2">
                  <span className="flex-1 font-mono text-xs text-ink">{resource}</span>
                  {isBroad(resource) && effect === 'allow' ? (
                    <Badge tone="warn">
                      <TriangleAlert aria-hidden />
                      Broad
                    </Badge>
                  ) : null}
                  <Button
                    size="icon"
                    variant="ghost"
                    aria-label={`Remove ${resource}`}
                    onClick={() =>
                      setResources((current) => current.filter((item) => item !== resource))
                    }
                  >
                    <X aria-hidden />
                  </Button>
                </li>
              ))}
            </ul>
            <div className="flex gap-2">
              <Input
                value={resourceDraft}
                placeholder="bucket:uploads/*"
                aria-label="Resource pattern"
                aria-invalid={resourceError !== null}
                aria-describedby={resourceError ? 'resource-error' : undefined}
                onChange={(event) => {
                  setResourceDraft(event.target.value);
                  setResourceError(null);
                }}
              />
              <Button
                onClick={() => {
                  const value = resourceDraft.trim();
                  if (value.length === 0 || resources.includes(value)) return;
                  const problem = resourceProblem(value);
                  if (problem) {
                    setResourceError(problem);
                    return;
                  }
                  setResources((current) => [...current, value]);
                  setResourceDraft('');
                  setResourceError(null);
                }}
              >
                Add
              </Button>
            </div>
            {resourceError ? (
              <p id="resource-error" role="alert" className="text-xs text-danger">
                {resourceError}
              </p>
            ) : null}
          </fieldset>

          {grantsBroadAccess ? (
            <div className="flex items-start gap-2 rounded-control border border-warn/40 bg-warn-soft px-3 py-2">
              <TriangleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-warn" />
              <p className="text-xs text-ink">
                This policy reaches every bucket that matches, not one. Confirm that is intended
                before attaching it to an account.
              </p>
            </div>
          ) : null}
          {error ? <ErrorDetails error={error} /> : null}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={!valid || mutation.isPending}>
            {mutation.isPending ? 'Creating…' : 'Create policy'}
          </Button>
        </DialogFooter>
      </form>
    </>
  );
}
