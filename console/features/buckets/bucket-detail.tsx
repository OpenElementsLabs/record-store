'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { MetricCard } from '@/components/metric-card';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Badge } from '@/components/ui/badge';
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
import { isBroad } from '@/features/access/policy-resource';
import { QuotaSection } from '@/features/buckets/quota-section';
import { ObjectBrowser } from '@/features/objects/object-browser';
import { ObjectVersions } from '@/features/objects/object-versions';
import { useCapabilities, usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { ApiError } from '@/lib/api/error';
import {
  createLifecycleRule,
  deleteLifecycleRule,
  fetchBuckets,
  fetchLifecycleRules,
  setBucketVersioning,
  updateLifecycleRule,
} from '@/lib/api/buckets';
import { formatBytes, formatCount, formatDateTime } from '@/lib/format';
import { fetchPolicies } from '@/lib/api/access';
import { verifyBucket } from '@/lib/api/integrity';
import { fetchStorageEvents } from '@/lib/api/observability';
import { describeLifecycleRule } from '@/lib/lifecycle-summary';
import type { Bucket, LifecycleRule, VersioningState } from '@/types/api';

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
        eyebrow="Bucket"
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

      {/*
        Overview leads the tab strip because it describes the bucket, but Objects
        is the landing tab: opening a bucket almost always means wanting to see
        what is in it, not its configuration summary.
      */}
      <Tabs defaultValue="objects">
        <TabsList>
          <TabsTrigger value="overview">Overview</TabsTrigger>
          <TabsTrigger value="objects">Objects</TabsTrigger>
          {capabilities.versioning ? (
            <TabsTrigger value="versioning">Versioning</TabsTrigger>
          ) : null}
          <TabsTrigger value="quota">Quota</TabsTrigger>
          {capabilities.lifecycle ? <TabsTrigger value="lifecycle">Lifecycle</TabsTrigger> : null}
          <TabsTrigger value="access">Access</TabsTrigger>
          {capabilities.events ? <TabsTrigger value="activity">Activity</TabsTrigger> : null}
          <TabsTrigger value="integrity">Integrity</TabsTrigger>
        </TabsList>

        <TabsContent value="overview">
          <BucketOverview bucket={bucket} record={record ?? null} />
        </TabsContent>

        <TabsContent value="objects">
          <ObjectBrowser bucket={bucket} />
        </TabsContent>

        {capabilities.versioning ? (
          <TabsContent value="versioning">
            <div className="space-y-4">
              <VersioningSection bucket={bucket} current={record?.versioning ?? null} />
              {versioned ? <ObjectVersions bucket={bucket} /> : null}
            </div>
          </TabsContent>
        ) : null}

        <TabsContent value="quota">
          <QuotaSection record={record ?? null} />
        </TabsContent>

        {capabilities.lifecycle ? (
          <TabsContent value="lifecycle">
            <LifecycleSection bucket={bucket} />
          </TabsContent>
        ) : null}

        <TabsContent value="access">
          <BucketAccess bucket={bucket} />
        </TabsContent>

        {capabilities.events ? (
          <TabsContent value="activity">
            <BucketActivity bucket={bucket} />
          </TabsContent>
        ) : null}

        <TabsContent value="integrity">
          <BucketIntegrity bucket={bucket} />
        </TabsContent>
      </Tabs>
    </>
  );
}

/** What this bucket holds and how it is configured, on one screen. */
function BucketOverview({
  bucket,
  record,
}: {
  readonly bucket: string;
  readonly record: Bucket | null;
}) {
  const capabilities = useCapabilities();
  const rules = useQuery({
    queryKey: queryKeys.bucketLifecycle(bucket),
    queryFn: ({ signal }) => fetchLifecycleRules(bucket, signal),
    enabled: capabilities.lifecycle,
  });

  const quota = record?.quota;
  const active = (rules.data ?? []).filter((rule) => rule.enabled);

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Bucket</CardTitle>
        <CardDescription>
          Configuration and accounting for {bucket}. Figures come with the bucket listing, so
          reading this page costs one request.
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-x-8 gap-y-3 sm:grid-cols-2">
        <Detail label="Name" value={bucket} />
        <Detail label="Created" value={record ? formatDateTime(record.created_at) : null} />
        <Detail label="Objects" value={record ? formatCount(record.object_count) : null} />
        <Detail label="Logical size" value={record ? formatBytes(record.logical_bytes) : null} />
        <Detail
          label="Versions retained"
          value={
            record
              ? `${formatCount(record.version_count)} (${formatBytes(record.version_bytes)})`
              : null
          }
        />
        <Detail
          label="Incomplete uploads"
          value={record ? formatBytes(record.multipart_bytes) : null}
        />
        <Detail label="Versioning" value={record ? capitalise(record.versioning) : null} />
        <Detail
          label="Storage quota"
          value={
            quota
              ? quota.bytes.mode === 'limit'
                ? formatBytes(quota.bytes.bytes)
                : 'Unlimited'
              : null
          }
        />
        <Detail
          label="Object quota"
          value={
            quota
              ? quota.objects.mode === 'limit'
                ? formatCount(quota.objects.objects)
                : 'Unlimited'
              : null
          }
        />
        {capabilities.lifecycle ? (
          <Detail
            label="Lifecycle"
            value={
              rules.isPending
                ? null
                : active.length === 0
                  ? 'No active rules'
                  : `${formatCount(active.length)} active rule${active.length === 1 ? '' : 's'}`
            }
          />
        ) : null}
      </CardContent>
    </Card>
  );
}

/**
 * Policies that can reach this bucket.
 *
 * OES scopes authorization by resource pattern, not by an access list attached
 * to the bucket. This therefore shows which policies match this bucket rather
 * than pretending the bucket owns a permission list of its own.
 */
function BucketAccess({ bucket }: { readonly bucket: string }) {
  const policies = useQuery({
    queryKey: queryKeys.policies,
    queryFn: ({ signal }) => fetchPolicies(signal),
  });

  const matching = (policies.data ?? [])
    .map((policy) => ({
      policy,
      resources: policy.statements
        .flatMap((statement) => statement.resources)
        .filter((resource) => resourceReachesBucket(resource, bucket)),
    }))
    .filter((entry) => entry.resources.length > 0);

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Access</CardTitle>
        <CardDescription>
          Policies whose resource patterns match this bucket. Authorization is decided by the
          backend on every request; this is a view of the rules, not a cache of decisions.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {policies.isError ? (
          <ErrorState error={policies.error} onRetry={() => void policies.refetch()} />
        ) : policies.isPending ? (
          <Skeleton className="h-16 w-full" />
        ) : matching.length === 0 ? (
          <EmptyState
            title="No policy reaches this bucket"
            description="No policy resource pattern matches this bucket, so only the root credential can access it."
          />
        ) : (
          <ul className="space-y-2">
            {matching.map(({ policy, resources }) => (
              <li
                key={policy.id}
                className="space-y-1 rounded-control border border-border px-3 py-2"
              >
                <p className="text-sm font-medium text-ink">{policy.name}</p>
                <ul className="flex flex-wrap gap-1.5">
                  {resources.map((resource) => (
                    <li key={resource}>
                      <Badge tone={isBroad(resource) ? 'warn' : 'neutral'} className="font-mono">
                        {resource}
                      </Badge>
                    </li>
                  ))}
                </ul>
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

/** Whether a policy resource pattern can reach this bucket. */
export function resourceReachesBucket(resource: string, bucket: string): boolean {
  if (!resource.startsWith('bucket:')) return false;
  const target = resource.slice('bucket:'.length);
  if (target.endsWith('*')) {
    const stem = target.slice(0, -1);
    // `bucket:up*` reaches every bucket whose name starts with "up".
    return bucket.startsWith(stem) || `${bucket}/`.startsWith(stem);
  }
  // An exact pattern names either the bucket or one key inside it.
  return target === bucket || target.startsWith(`${bucket}/`);
}

/** Storage events for this bucket, newest first. */
function BucketActivity({ bucket }: { readonly bucket: string }) {
  const events = useQuery({
    queryKey: queryKeys.events(`bucket:${bucket}`),
    queryFn: ({ signal }) => fetchStorageEvents({ bucket, limit: 25 }, signal),
  });

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Activity</CardTitle>
        <CardDescription>
          Recent storage events for this bucket. This is data activity, not the security audit
          trail.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {events.isError ? (
          <ErrorState error={events.error} onRetry={() => void events.refetch()} />
        ) : events.isPending ? (
          <Skeleton className="h-20 w-full" />
        ) : events.data.events.length === 0 ? (
          <EmptyState
            title="No recorded activity"
            description="No storage events have been recorded for this bucket yet."
          />
        ) : (
          <ul className="divide-y divide-border">
            {events.data.events.map((event) => (
              <li key={event.id} className="flex flex-wrap items-baseline gap-x-3 py-2">
                <Badge tone="neutral">{event.type}</Badge>
                <span className="min-w-0 flex-1 truncate font-mono type-meta">
                  {event.object ?? '—'}
                </span>
                <time dateTime={event.time} className="type-meta-subtle">
                  {formatDateTime(event.time)}
                </time>
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Verifies every object in the bucket.
 *
 * This reads all of the bucket's bytes, so the cost is stated up front rather
 * than discovered. Verification detects a mismatch; it cannot repair one.
 */
function BucketIntegrity({ bucket }: { readonly bucket: string }) {
  const permissions = usePermissions();
  const verification = useMutation({
    mutationFn: () => verifyBucket(bucket),
    onSuccess: (result) => {
      if (result.failures === 0) {
        toast.success(`${formatCount(result.verified_objects)} objects verified`);
      } else {
        toast.error(`${formatCount(result.failures)} objects failed verification`);
      }
    },
  });

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Integrity</CardTitle>
        <CardDescription>
          Re-reads and re-hashes every object in this bucket, comparing each against the checksum
          recorded when it was stored.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {permissions.manage_storage ? (
          <Button
            size="sm"
            variant="secondary"
            disabled={verification.isPending}
            onClick={() => verification.mutate()}
          >
            {verification.isPending ? 'Verifying…' : 'Verify bucket'}
          </Button>
        ) : (
          <p className="type-meta">Your role does not permit running verification.</p>
        )}
        {verification.error ? <ErrorState error={verification.error} /> : null}
        {verification.data ? (
          <p
            className={verification.data.failures === 0 ? 'text-sm text-ok' : 'text-sm text-danger'}
            role="status"
          >
            {verification.data.failures === 0
              ? `All ${formatCount(verification.data.verified_objects)} objects match their recorded checksums.`
              : `${formatCount(verification.data.failures)} of ${formatCount(verification.data.verified_objects)} objects did not match. A checksum detects damage; it cannot repair it.`}
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}

function Detail({ label, value }: { readonly label: string; readonly value: string | null }) {
  return (
    <div className="min-w-0 space-y-0.5">
      <p className="text-xs font-medium text-ink-muted">{label}</p>
      {value === null ? (
        <Skeleton className="h-5 w-24" />
      ) : (
        <p className="break-all type-body">{value}</p>
      )}
    </div>
  );
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
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
          <span className="type-meta">Current state</span>
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
          <p className="type-meta">Your role does not permit changing bucket settings.</p>
        )}
      </CardContent>
    </Card>
  );
}

function LifecycleSection({ bucket }: { readonly bucket: string }) {
  const client = useQueryClient();
  const permissions = usePermissions();
  const [creating, setCreating] = React.useState(false);
  const [editing, setEditing] = React.useState<LifecycleRule | null>(null);

  const toggle = useMutation({
    mutationFn: (rule: LifecycleRule) =>
      updateLifecycleRule(bucket, rule.id, {
        prefix: rule.prefix,
        enabled: !rule.enabled,
        expiration: rule.expiration,
        noncurrent_version_expiration: rule.noncurrent_version_expiration,
      }),
    onSuccess: async (updated) => {
      toast.success(updated.enabled ? 'Lifecycle rule enabled' : 'Lifecycle rule disabled');
      await client.invalidateQueries({ queryKey: queryKeys.bucketLifecycle(bucket) });
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : 'Could not change the rule'),
  });
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
                  className="flex flex-wrap items-center gap-3 rounded-control border border-border px-3 py-2"
                >
                  <StatusBadge
                    level={rule.enabled ? 'healthy' : 'disabled'}
                    label={rule.enabled ? 'Enabled' : 'Disabled'}
                  />
                  <span className="min-w-0 flex-1 type-meta">{describeLifecycleRule(rule)}</span>
                  {permissions.manage_buckets ? (
                    <div className="flex items-center gap-2">
                      <Button
                        size="sm"
                        variant="secondary"
                        aria-label={`${rule.enabled ? 'Disable' : 'Enable'} lifecycle rule ${rule.prefix || 'all keys'}`}
                        disabled={toggle.isPending}
                        onClick={() => toggle.mutate(rule)}
                      >
                        {rule.enabled ? 'Disable' : 'Enable'}
                      </Button>
                      <Button
                        size="sm"
                        variant="secondary"
                        aria-label={`Edit lifecycle rule ${rule.prefix || 'all keys'}`}
                        onClick={() => setEditing(rule)}
                      >
                        Edit
                      </Button>
                      <Button
                        size="sm"
                        variant="danger"
                        aria-label={`Delete lifecycle rule ${rule.prefix || 'all keys'}`}
                        onClick={() => setPendingDelete(rule)}
                      >
                        Delete
                      </Button>
                    </div>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>

      <LifecycleRuleDialog
        bucket={bucket}
        rule={editing}
        open={creating || editing !== null}
        onOpenChange={(open) => {
          if (!open) {
            setCreating(false);
            setEditing(null);
          }
        }}
        onSaved={async () => {
          setCreating(false);
          setEditing(null);
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

/**
 * Creates or edits one lifecycle rule.
 *
 * Both use the same form because they set the same fields. The dialog's state
 * is seeded from the rule and lives inside `DialogContent`, which only mounts
 * while open, so switching rules starts from the right values without an effect
 * copying props into state.
 */
function LifecycleRuleDialog({
  bucket,
  rule,
  open,
  onOpenChange,
  onSaved,
}: {
  readonly bucket: string;
  readonly rule: LifecycleRule | null;
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
  readonly onSaved: () => Promise<void>;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <LifecycleRuleForm
          key={rule?.id ?? 'new'}
          bucket={bucket}
          rule={rule}
          onOpenChange={onOpenChange}
          onSaved={onSaved}
        />
      </DialogContent>
    </Dialog>
  );
}

function LifecycleRuleForm({
  bucket,
  rule,
  onOpenChange,
  onSaved,
}: {
  readonly bucket: string;
  readonly rule: LifecycleRule | null;
  readonly onOpenChange: (open: boolean) => void;
  readonly onSaved: () => Promise<void>;
}) {
  const [prefix, setPrefix] = React.useState(rule?.prefix ?? '');
  const [expiration, setExpiration] = React.useState(
    rule?.expiration === null || rule?.expiration === undefined ? '' : String(rule.expiration),
  );
  const [noncurrentExpiration, setNoncurrentExpiration] = React.useState(
    rule?.noncurrent_version_expiration === null ||
      rule?.noncurrent_version_expiration === undefined
      ? ''
      : String(rule.noncurrent_version_expiration),
  );
  const valid = positive(expiration) !== null || positive(noncurrentExpiration) !== null;
  const mutation = useMutation({
    mutationFn: () => {
      const input = {
        prefix: prefix.trim(),
        enabled: rule?.enabled ?? true,
        expiration: positive(expiration),
        noncurrent_version_expiration: positive(noncurrentExpiration),
      };
      return rule
        ? updateLifecycleRule(bucket, rule.id, input)
        : createLifecycleRule(bucket, input);
    },
    onSuccess: async () => {
      toast.success(rule ? 'Lifecycle rule updated' : 'Lifecycle rule created');
      await onSaved();
    },
  });

  return (
    <>
      <DialogHeader>
        <DialogTitle>{rule ? 'Edit lifecycle rule' : 'Create lifecycle rule'}</DialogTitle>
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
            <Input
              id="lifecycle-prefix"
              value={prefix}
              onChange={(event) => setPrefix(event.target.value)}
            />
          </Field>
          <Field label="Expire current objects after days" htmlFor="lifecycle-expiration">
            <Input
              id="lifecycle-expiration"
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
              id="lifecycle-noncurrent-expiration"
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
            {mutation.isPending
              ? rule
                ? 'Saving…'
                : 'Creating…'
              : rule
                ? 'Save rule'
                : 'Create rule'}
          </Button>
        </DialogFooter>
      </form>
    </>
  );
}

function positive(value: string): number | null {
  const number = Number(value);
  return Number.isInteger(number) && number > 0 ? number : null;
}
