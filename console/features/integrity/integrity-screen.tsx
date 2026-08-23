'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { CircleCheck, ShieldAlert, TriangleAlert } from 'lucide-react';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Field } from '@/components/ui/label';
import { Skeleton } from '@/components/ui/skeleton';
import { useDeployment, usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { fetchBuckets } from '@/lib/api/buckets';
import { inspectStorage, repairStorage, verifyBucket } from '@/lib/api/integrity';
import { formatCount } from '@/lib/format';
import { shortenIdentifier } from '@/lib/format';
import type { StorageInspection } from '@/types/api';

/** How many entries one scan walks before reporting that it stopped early. */
const SCAN_LIMIT = 10_000;

type Severity = 'healthy' | 'reclaimable' | 'data-loss';

/**
 * Classifies a scan result.
 *
 * Only missing payloads are data loss. Orphans and stray temporary entries are
 * space that can be reclaimed, which is a different and much less urgent thing.
 */
function severityOf(inspection: StorageInspection): Severity {
  if (inspection.metadata_without_data > 0) return 'data-loss';
  const reclaimable =
    inspection.data_without_metadata +
    inspection.unknown_data_entries +
    inspection.unknown_temporary_entries;
  return reclaimable > 0 ? 'reclaimable' : 'healthy';
}

export function IntegrityScreen() {
  const { info } = useDeployment();
  const permissions = usePermissions();
  const clustered = info.mode !== 'standalone';

  const inspection = useQuery({
    queryKey: queryKeys.storageInspection(SCAN_LIMIT),
    queryFn: ({ signal }) => inspectStorage(SCAN_LIMIT, signal),
    // A consistency scan reads the whole catalog, so it is not refetched on
    // every focus change; the operator asks for it.
    staleTime: Number.POSITIVE_INFINITY,
    refetchOnWindowFocus: false,
  });

  return (
    <>
      <PageHeader
        title="Integrity"
        description="Cross-checks what OES records against the bytes it actually holds."
        actions={
          <Button
            size="sm"
            disabled={inspection.isFetching}
            onClick={() => void inspection.refetch()}
          >
            {inspection.isFetching ? 'Scanning…' : 'Run scan'}
          </Button>
        }
      />

      {inspection.isError ? (
        <ErrorState error={inspection.error} onRetry={() => void inspection.refetch()} />
      ) : (
        <div className="space-y-4">
          <StatusCard inspection={inspection.data ?? null} clustered={clustered} />
          <FindingsCard inspection={inspection.data ?? null} />
          <VerifyCard />
          {permissions.manage_storage ? <ReclaimCard clustered={clustered} /> : null}
        </div>
      )}
    </>
  );
}

function StatusCard({
  inspection,
  clustered,
}: {
  readonly inspection: StorageInspection | null;
  readonly clustered: boolean;
}) {
  if (!inspection) {
    return (
      <Card>
        <CardContent>
          <Skeleton className="h-16 w-full" />
        </CardContent>
      </Card>
    );
  }
  const severity = severityOf(inspection);
  const headline = {
    healthy: 'No inconsistencies found',
    reclaimable: 'Reclaimable storage found',
    'data-loss': 'Objects are missing their payloads',
  }[severity];

  return (
    <Card>
      <CardContent className="flex flex-col gap-3 sm:flex-row sm:items-start">
        <StatusIcon severity={severity} />
        <div className="space-y-1.5">
          <p className="text-sm font-medium text-ink">{headline}</p>
          <p className="max-w-2xl text-sm text-ink-muted">
            {severity === 'healthy'
              ? `Every one of the ${formatCount(inspection.metadata_payloads_scanned)} objects scanned has the bytes OES recorded for it.`
              : severity === 'reclaimable'
                ? 'Some payloads on disk are not referenced by any object. They occupy space but no object depends on them.'
                : 'These objects cannot be read. A checksum proves bytes are wrong or missing; it cannot rebuild them.'}
          </p>
          {severity === 'data-loss' ? (
            <p className="max-w-2xl text-sm text-ink-muted">
              {clustered
                ? 'This deployment stores redundant copies, so affected objects may be recoverable from another replica. Check replication and repair status.'
                : 'This deployment keeps a single copy of each object, so there is no redundant copy to rebuild from. Recovery requires restoring from a backup outside OES.'}
            </p>
          ) : null}
          {inspection.truncated ? (
            <p className="text-xs text-warn">
              The scan stopped at {formatCount(SCAN_LIMIT)} entries, so these counts are a sample
              rather than a total.
            </p>
          ) : null}
        </div>
      </CardContent>
    </Card>
  );
}

function StatusIcon({ severity }: { readonly severity: Severity }) {
  const className = 'mt-0.5 size-5 shrink-0';
  if (severity === 'healthy') {
    return <CircleCheck aria-hidden className={`${className} text-ok`} />;
  }
  if (severity === 'reclaimable') {
    return <TriangleAlert aria-hidden className={`${className} text-warn`} />;
  }
  return <ShieldAlert aria-hidden className={`${className} text-danger`} />;
}

function FindingsCard({ inspection }: { readonly inspection: StorageInspection | null }) {
  const rows: readonly {
    readonly label: string;
    readonly value: number;
    readonly tone: 'neutral' | 'warn' | 'danger';
    readonly hint: string;
  }[] = inspection
    ? [
        {
          label: 'Objects scanned',
          value: inspection.metadata_payloads_scanned,
          tone: 'neutral',
          hint: 'Object versions whose payload was checked.',
        },
        {
          label: 'Payloads on disk',
          value: inspection.data_payloads_scanned,
          tone: 'neutral',
          hint: 'Stored payload files examined.',
        },
        {
          label: 'Missing payloads',
          value: inspection.metadata_without_data,
          tone: 'danger',
          hint: 'OES records the object but its bytes are not on disk.',
        },
        {
          label: 'Orphaned payloads',
          value: inspection.data_without_metadata,
          tone: 'warn',
          hint: 'Bytes on disk that no object references. Safe to reclaim.',
        },
        {
          label: 'Unrecognised data entries',
          value: inspection.unknown_data_entries,
          tone: 'warn',
          hint: 'Files in the data directory OES did not write.',
        },
        {
          label: 'Abandoned uploads',
          value: inspection.unknown_temporary_entries,
          tone: 'warn',
          hint: 'Temporary files left by uploads that never completed.',
        },
      ]
    : [];

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Scan findings</CardTitle>
        <CardDescription>
          Counts from the most recent scan. Missing payloads are data loss; the rest is space.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {inspection ? (
          <>
            <dl className="grid gap-x-6 gap-y-4 sm:grid-cols-2 lg:grid-cols-3">
              {rows.map((row) => (
                <div key={row.label} className="space-y-0.5">
                  <dt className="text-xs font-medium text-ink-muted">{row.label}</dt>
                  <dd
                    className={`text-lg tabular-nums ${
                      row.value > 0 && row.tone === 'danger'
                        ? 'text-danger'
                        : row.value > 0 && row.tone === 'warn'
                          ? 'text-warn'
                          : 'text-ink'
                    }`}
                  >
                    {formatCount(row.value)}
                  </dd>
                  <p className="type-meta-subtle">{row.hint}</p>
                </div>
              ))}
            </dl>
            <Samples
              missing={inspection.missing_payload_samples}
              orphans={inspection.orphan_payload_samples}
            />
          </>
        ) : (
          <Skeleton className="h-24 w-full" />
        )}
      </CardContent>
    </Card>
  );
}

function Samples({
  missing,
  orphans,
}: {
  readonly missing: readonly string[];
  readonly orphans: readonly string[];
}) {
  if (missing.length === 0 && orphans.length === 0) return null;
  return (
    <div className="mt-5 space-y-3 border-t border-border pt-4">
      {missing.length > 0 ? (
        <SampleList
          title="Objects with missing payloads"
          // Payload identifiers, never filesystem paths.
          ids={missing}
        />
      ) : null}
      {orphans.length > 0 ? <SampleList title="Orphaned payloads" ids={orphans} /> : null}
    </div>
  );
}

function SampleList({ title, ids }: { readonly title: string; readonly ids: readonly string[] }) {
  return (
    <div className="space-y-1.5">
      <p className="text-xs font-medium text-ink-muted">
        {title} <span className="text-ink-subtle">(sample of {formatCount(ids.length)})</span>
      </p>
      <ul className="flex flex-wrap gap-1.5">
        {ids.map((id) => (
          <li key={id}>
            <Badge tone="neutral" className="font-mono" title={id}>
              {shortenIdentifier(id, 8)}
            </Badge>
          </li>
        ))}
      </ul>
    </div>
  );
}

function VerifyCard() {
  const buckets = useQuery({
    queryKey: queryKeys.buckets,
    queryFn: ({ signal }) => fetchBuckets(signal),
  });
  const [bucket, setBucket] = React.useState('');
  const verification = useMutation({
    mutationFn: (name: string) => verifyBucket(name),
    onSuccess: (result, name) => {
      if (result.failures === 0) {
        toast.success(
          `${formatCount(result.verified_objects)} objects in ${name} verified with no failures`,
        );
      } else {
        toast.error(`${formatCount(result.failures)} objects in ${name} failed verification`);
      }
    },
  });

  const available = buckets.data ?? [];

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Verify a bucket</CardTitle>
        <CardDescription>
          Re-reads every object in the bucket and re-hashes it. This reads all of the bucket&apos;s
          bytes, so it costs real I/O on a large bucket.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {available.length === 0 && !buckets.isPending ? (
          <EmptyState
            title="No buckets to verify"
            description="Create a bucket and store an object first."
          />
        ) : (
          <form
            className="flex flex-wrap items-end gap-2"
            onSubmit={(event) => {
              event.preventDefault();
              if (bucket) verification.mutate(bucket);
            }}
          >
            <Field label="Bucket" htmlFor="verify-bucket">
              <select
                id="verify-bucket"
                value={bucket}
                onChange={(event) => setBucket(event.target.value)}
                className="h-9 min-w-48 rounded-control border border-border-strong bg-surface px-2 type-body"
              >
                <option value="">Select a bucket…</option>
                {available.map((entry) => (
                  <option key={entry.id} value={entry.name}>
                    {entry.name}
                  </option>
                ))}
              </select>
            </Field>
            <Button
              type="submit"
              size="sm"
              variant="primary"
              disabled={bucket === '' || verification.isPending}
            >
              {verification.isPending ? 'Verifying…' : 'Verify bucket'}
            </Button>
          </form>
        )}
        {verification.error ? <ErrorState error={verification.error} /> : null}
        {verification.data ? (
          <p className="text-sm text-ink-muted" role="status">
            Verified {formatCount(verification.data.verified_objects)} objects,{' '}
            {formatCount(verification.data.failures)} failures.
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}

function ReclaimCard({ clustered }: { readonly clustered: boolean }) {
  const client = useQueryClient();
  const [confirming, setConfirming] = React.useState(false);

  const preview = useMutation({
    mutationFn: () => repairStorage(SCAN_LIMIT, true),
  });
  const apply = useMutation({
    mutationFn: () => repairStorage(SCAN_LIMIT, false),
    onSuccess: async (result) => {
      toast.success(`Reclaimed ${formatCount(result.removed_orphan_payloads)} orphaned payloads`);
      setConfirming(false);
      await client.invalidateQueries({ queryKey: ['storage'] });
    },
  });

  const previewed = preview.data;

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Reclaim orphaned payloads</CardTitle>
        <CardDescription>
          Deletes payload files that no object references. This frees space; it does not repair or
          recover anything.
          {clustered
            ? ' Replica repair is a separate operation driven by the cluster.'
            : ' An object whose payload is already missing cannot be restored by this operation.'}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="flex flex-wrap gap-2">
          <Button size="sm" disabled={preview.isPending} onClick={() => preview.mutate()}>
            {preview.isPending ? 'Checking…' : 'Preview'}
          </Button>
          <Button
            size="sm"
            variant="danger"
            disabled={!previewed || previewed.inspection.data_without_metadata === 0}
            onClick={() => setConfirming(true)}
          >
            Reclaim
          </Button>
        </div>
        {preview.error ? <ErrorState error={preview.error} /> : null}
        {apply.error ? <ErrorState error={apply.error} /> : null}
        {previewed ? (
          <p className="text-sm text-ink-muted" role="status">
            {previewed.inspection.data_without_metadata === 0
              ? 'Nothing to reclaim.'
              : `${formatCount(previewed.inspection.data_without_metadata)} orphaned payloads would be removed.`}
          </p>
        ) : (
          <p className="type-meta-subtle">
            Preview first: it reports what would be removed without removing it.
          </p>
        )}
      </CardContent>

      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title="Reclaim orphaned payloads"
        description={
          previewed
            ? `${formatCount(previewed.inspection.data_without_metadata)} payload files that no object references will be deleted permanently. Objects and versions are not affected.`
            : ''
        }
        confirmLabel="Reclaim"
        strength="acknowledge"
        pending={apply.isPending}
        error={apply.error}
        onConfirm={() => apply.mutate()}
      />
    </Card>
  );
}
