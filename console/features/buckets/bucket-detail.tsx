'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { ErrorState } from '@/components/error-state';
import { MetricCard } from '@/components/metric-card';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
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
import { Skeleton } from '@/components/ui/skeleton';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { ObjectBrowser } from '@/features/objects/object-browser';
import { ObjectVersions } from '@/features/objects/object-versions';
import { useCapabilities, usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import {
  createLifecycleRule,
  deleteLifecycleRule,
  fetchBuckets,
  fetchLifecycleRules,
  setBucketVersioning,
} from '@/lib/api/buckets';
import { formatBytes, formatCount, formatDateTime } from '@/lib/format';
import type { LifecycleRule, VersioningState } from '@/types/api';

/**
 * One bucket, with only the sections this deployment actually supports.
 *
 * Tabs are omitted rather than shown empty, so the presence of a tab is a
 * reliable signal that there is something behind it.
 */
export function BucketDetail({ bucket }: { readonly bucket: string }) {
  const capabilities = useCapabilities();
  const buckets = useQuery({
    queryKey: queryKeys.buckets,
    queryFn: ({ signal }) => fetchBuckets(signal),
  });

  const record = buckets.data?.find((candidate) => candidate.name === bucket);
  const versioned = record?.versioning === 'enabled' || record?.versioning === 'suspended';

  return (
    <>
      <PageHeader
        title={bucket}
        description="Objects, version history, and settings for this bucket."
      />

      {buckets.isError ? (
        <Card>
          <ErrorState error={buckets.error} onRetry={() => void buckets.refetch()} />
        </Card>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <MetricCard
            label="Objects"
            value={record ? formatCount(record.object_count) : <Skeleton className="h-7 w-16" />}
          />
          <MetricCard
            label="Size"
            value={record ? formatBytes(record.logical_bytes) : <Skeleton className="h-7 w-20" />}
          />
          <MetricCard
            label="Versions"
            value={record ? formatCount(record.version_count) : <Skeleton className="h-7 w-16" />}
            detail={record ? formatBytes(record.version_bytes) : undefined}
          />
          <MetricCard
            label="Created"
            value={
              record ? (
                <span className="text-base">{formatDateTime(record.created_at)}</span>
              ) : (
                <Skeleton className="h-7 w-32" />
              )
            }
          />
        </div>
      )}

      <Tabs defaultValue="objects">
        <TabsList>
          <TabsTrigger value="objects">Objects</TabsTrigger>
          {capabilities.versioning && versioned ? (
            <TabsTrigger value="versions">Versions</TabsTrigger>
          ) : null}
          {capabilities.lifecycle ? <TabsTrigger value="lifecycle">Lifecycle</TabsTrigger> : null}
          <TabsTrigger value="settings">Settings</TabsTrigger>
        </TabsList>

        <TabsContent value="objects">
          <ObjectBrowser bucket={bucket} />
        </TabsContent>

        {capabilities.versioning && versioned ? (
          <TabsContent value="versions">
            <ObjectVersions bucket={bucket} />
          </TabsContent>
        ) : null}

        {capabilities.lifecycle ? (
          <TabsContent value="lifecycle">
            <LifecycleSection bucket={bucket} />
          </TabsContent>
        ) : null}

        <TabsContent value="settings">
          <VersioningSection bucket={bucket} current={record?.versioning ?? null} />
        </TabsContent>
      </Tabs>
    </>
  );
}

function VersioningSection({
  bucket,
  current,
}: {
  readonly bucket: string;
  readonly current: VersioningState | null;
}) {
  const client = useQueryClient();
  const permissions = usePermissions();
  const mutation = useMutation({
    mutationFn: (state: VersioningState) => setBucketVersioning(bucket, state),
    onSuccess: async (updated) => {
      toast.success(`Versioning ${updated.versioning}`);
      await client.invalidateQueries({ queryKey: queryKeys.buckets });
    },
  });

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Object versioning</CardTitle>
        <CardDescription>
          When enabled, overwrites and deletes keep the previous version instead of replacing it.
          Versioning cannot be turned off again once enabled; it can only be suspended.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex items-center gap-2">
          <span className="text-xs text-ink-muted">Current state</span>
          {current ? (
            <StatusBadge
              level={
                current === 'enabled' ? 'healthy' : current === 'suspended' ? 'paused' : 'disabled'
              }
              label={current.charAt(0).toUpperCase() + current.slice(1)}
            />
          ) : (
            <Skeleton className="h-5 w-20" />
          )}
        </div>
        {mutation.error ? <ErrorState error={mutation.error} /> : null}
        {permissions.manage_buckets ? (
          <div className="flex flex-wrap gap-2">
            <Button
              variant="primary"
              size="sm"
              disabled={current === 'enabled' || mutation.isPending}
              onClick={() => mutation.mutate('enabled')}
            >
              Enable versioning
            </Button>
            <Button
              size="sm"
              disabled={current !== 'enabled' || mutation.isPending}
              onClick={() => mutation.mutate('suspended')}
            >
              Suspend versioning
            </Button>
          </div>
        ) : (
          <p className="text-xs text-ink-muted">
            Your role does not permit changing bucket settings.
          </p>
        )}
      </CardContent>
    </Card>
  );
}

function LifecycleSection({ bucket }: { readonly bucket: string }) {
  const client = useQueryClient();
  const permissions = usePermissions();
  const [creating, setCreating] = React.useState(false);
  const [pendingDelete, setPendingDelete] = React.useState<LifecycleRule | null>(null);
  const rules = useQuery({
    queryKey: queryKeys.bucketLifecycle(bucket),
    queryFn: ({ signal }) => fetchLifecycleRules(bucket, signal),
  });
  const removal = useMutation({
    mutationFn: (id: string) => deleteLifecycleRule(id),
    onSuccess: async () => {
      toast.success('Lifecycle rule deleted');
      setPendingDelete(null);
      await client.invalidateQueries({ queryKey: queryKeys.bucketLifecycle(bucket) });
    },
  });

  return (
    <>
      <Card>
        <CardHeader>
          <div>
            <CardTitle>Lifecycle rules</CardTitle>
            <CardDescription>
              Rules expire objects and non-current versions by age. They are evaluated by a
              background worker on the server.
            </CardDescription>
          </div>
          {permissions.manage_buckets ? (
            <Button size="sm" variant="primary" onClick={() => setCreating(true)}>
              Create rule
            </Button>
          ) : null}
        </CardHeader>
        <CardContent>
          {rules.isError ? (
            <ErrorState error={rules.error} onRetry={() => void rules.refetch()} />
          ) : rules.isPending ? (
            <Skeleton className="h-16 w-full" />
          ) : rules.data.length === 0 ? (
            <p className="text-sm text-ink-muted">
              No lifecycle rules are configured for this bucket.
            </p>
          ) : (
            <ul className="space-y-2">
              {rules.data.map((rule) => (
                <li
                  key={rule.id}
                  className="flex flex-wrap items-center gap-3 rounded-[--radius-control] border border-border px-3 py-2"
                >
                  <StatusBadge
                    level={rule.enabled ? 'healthy' : 'disabled'}
                    label={rule.enabled ? 'Enabled' : 'Disabled'}
                  />
                  <span className="font-mono text-xs text-ink">{rule.prefix || '(all keys)'}</span>
                  <span className="min-w-0 flex-1 text-xs text-ink-muted">
                    {rule.expiration ? `Expire after ${rule.expiration} day(s)` : null}
                    {rule.expiration && rule.noncurrent_version_expiration ? ' · ' : null}
                    {rule.noncurrent_version_expiration
                      ? `Non-current versions after ${rule.noncurrent_version_expiration} day(s)`
                      : null}
                  </span>
                  {permissions.manage_buckets ? (
                    <Button
                      size="sm"
                      variant="danger"
                      aria-label={`Delete lifecycle rule ${rule.prefix || 'all keys'}`}
                      onClick={() => setPendingDelete(rule)}
                    >
                      Delete
                    </Button>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>

      <CreateLifecycleRuleDialog
        bucket={bucket}
        open={creating}
        onOpenChange={setCreating}
        onCreated={async () => {
          setCreating(false);
          await client.invalidateQueries({ queryKey: queryKeys.bucketLifecycle(bucket) });
        }}
      />
      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPendingDelete(null);
            removal.reset();
          }
        }}
        title="Delete lifecycle rule?"
        description={pendingDelete?.prefix || 'All keys'}
        consequence="Objects will no longer expire under this rule. Already expired objects are not restored."
        confirmLabel="Delete rule"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => {
          if (pendingDelete) removal.mutate(pendingDelete.id);
        }}
      />
    </>
  );
}

function CreateLifecycleRuleDialog({
  bucket,
  open,
  onOpenChange,
  onCreated,
}: {
  bucket: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: () => Promise<void>;
}) {
  const [prefix, setPrefix] = React.useState('');
  const [expiration, setExpiration] = React.useState('');
  const [noncurrentExpiration, setNoncurrentExpiration] = React.useState('');
  const valid = positive(expiration) !== null || positive(noncurrentExpiration) !== null;
  const mutation = useMutation({
    mutationFn: () =>
      createLifecycleRule(bucket, {
        prefix: prefix.trim(),
        enabled: true,
        expiration: positive(expiration),
        noncurrent_version_expiration: positive(noncurrentExpiration),
      }),
    onSuccess: async () => {
      toast.success('Lifecycle rule created');
      setPrefix('');
      setExpiration('');
      setNoncurrentExpiration('');
      await onCreated();
    },
  });

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create lifecycle rule</DialogTitle>
          <DialogDescription>
            Set at least one positive expiration age. An empty prefix applies to all keys.
          </DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (valid) mutation.mutate();
          }}
        >
          <DialogBody>
            <Field label="Key prefix" htmlFor="lifecycle-prefix" hint="Optional.">
              <Input value={prefix} onChange={(event) => setPrefix(event.target.value)} />
            </Field>
            <Field label="Expire current objects after days" htmlFor="lifecycle-expiration">
              <Input
                type="number"
                min="1"
                value={expiration}
                onChange={(event) => setExpiration(event.target.value)}
              />
            </Field>
            <Field
              label="Expire non-current versions after days"
              htmlFor="lifecycle-noncurrent-expiration"
            >
              <Input
                type="number"
                min="1"
                value={noncurrentExpiration}
                onChange={(event) => setNoncurrentExpiration(event.target.value)}
              />
            </Field>
            {mutation.error ? <ErrorState error={mutation.error} /> : null}
          </DialogBody>
          <DialogFooter>
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="primary" disabled={!valid || mutation.isPending}>
              {mutation.isPending ? 'Creating…' : 'Create rule'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function positive(value: string): number | null {
  const number = Number(value);
  return Number.isInteger(number) && number > 0 ? number : null;
}
