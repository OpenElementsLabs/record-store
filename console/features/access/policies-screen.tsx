'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Plus, TriangleAlert, X } from 'lucide-react';
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
import { TableSkeleton } from '@/components/ui/skeleton';
import { queryKeys } from '@/hooks/use-system';
import { createPolicy, fetchPolicies } from '@/lib/api/access';
import { ApiError } from '@/lib/api/error';
import { formatDate } from '@/lib/format';
import type { PolicyAction, PolicyEffect, PolicyStatement } from '@/types/api';

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
  const [creating, setCreating] = React.useState(false);
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
                      <p className="mt-0.5 text-xs text-ink-muted">{policy.description}</p>
                    ) : null}
                  </div>
                  <span className="shrink-0 text-xs text-ink-subtle">
                    Created {formatDate(policy.created_at)}
                  </span>
                </div>
              </CardHeader>
              <CardContent className="space-y-3">
                {policy.statements.map((statement, index) => (
                  <StatementView key={index} statement={statement} />
                ))}
              </CardContent>
            </Card>
          ))}
        </div>
      )}

      <CreatePolicyDialog open={creating} onOpenChange={setCreating} />
    </>
  );
}

function StatementView({ statement }: { readonly statement: PolicyStatement }) {
  const broad = statement.resources.some(isBroad);
  return (
    <div className="space-y-2 rounded-[--radius-control] border border-border p-3">
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
            className="rounded border border-border bg-surface-muted px-1.5 py-0.5 font-mono text-[0.6875rem] text-ink-muted"
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
            <legend className="text-xs font-medium text-ink">Effect</legend>
            <div className="flex gap-2">
              {(['allow', 'deny'] as const).map((option) => (
                <label key={option} className="flex items-center gap-1.5 text-sm text-ink">
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
            <legend className="text-xs font-medium text-ink">Actions</legend>
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
            <legend className="text-xs font-medium text-ink">Resources</legend>
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
            <div className="flex items-start gap-2 rounded-[--radius-control] border border-warn/40 bg-warn-soft px-3 py-2">
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
