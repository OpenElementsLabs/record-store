'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Download } from 'lucide-react';
import { useRouter } from 'next/navigation';
import * as React from 'react';
import { toast } from 'sonner';

import { Breadcrumbs, type Crumb } from '@/components/breadcrumbs';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { CopyButton } from '@/components/copy-button';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { EmptyState } from '@/components/empty-state';
import { ObjectVersions } from '@/features/objects/object-versions';
import { ObjectPreview } from '@/features/objects/object-preview';
import { verifyObject } from '@/lib/api/integrity';
import { fetchStorageEvents } from '@/lib/api/observability';
import { Skeleton } from '@/components/ui/skeleton';
import { useCapabilities, usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { deleteObject, fetchObject, objectContentUrl } from '@/lib/api/objects';
import { formatBytes, formatDateTime, keyBasename, keySegments } from '@/lib/format';
import { mergeSearch } from '@/lib/search-params';
import type { ObjectSummary } from '@/types/api';

/**
 * One object's metadata.
 *
 * Everything shown here is protocol-visible metadata. Where the bytes physically
 * live is an internal concern and is not part of this view.
 */
export function ObjectDetails({
  bucket,
  objectKey,
}: {
  readonly bucket: string;
  readonly objectKey: string;
}) {
  const router = useRouter();
  const client = useQueryClient();
  const permissions = usePermissions();
  const capabilities = useCapabilities();
  const [confirming, setConfirming] = React.useState(false);

  const object = useQuery({
    queryKey: queryKeys.object(bucket, objectKey),
    queryFn: ({ signal }) => fetchObject(bucket, objectKey, signal),
  });

  const removal = useMutation({
    mutationFn: () => deleteObject(bucket, objectKey),
    onSuccess: async () => {
      toast.success(`Deleted ${keyBasename(objectKey)}`);
      await client.invalidateQueries({ queryKey: ['buckets', bucket] });
      router.push(`/buckets/${encodeURIComponent(bucket)}`);
    },
  });

  const segments = keySegments(objectKey);
  const crumbs: Crumb[] = [
    { label: 'Buckets', href: '/buckets' },
    { label: bucket, href: `/buckets/${encodeURIComponent(bucket)}` },
    ...segments.slice(0, -1).map((segment, index) => ({
      label: segment,
      href: `/buckets/${encodeURIComponent(bucket)}${mergeSearch(new URLSearchParams(), {
        prefix: `${segments.slice(0, index + 1).join('/')}/`,
      })}`,
    })),
    { label: keyBasename(objectKey) },
  ];

  return (
    <>
      <Breadcrumbs items={crumbs} />
      <PageHeader
        eyebrow="Object"
        title={keyBasename(objectKey)}
        description={objectKey}
        actions={
          <div className="flex items-center gap-2">
            <Button asChild variant="secondary">
              <a href={objectContentUrl(bucket, objectKey)} download>
                <Download aria-hidden />
                Download
              </a>
            </Button>
            {permissions.manage_objects ? (
              <Button variant="danger" onClick={() => setConfirming(true)}>
                Delete
              </Button>
            ) : null}
          </div>
        }
      />

      {object.isError ? (
        <Card>
          <ErrorState error={object.error} onRetry={() => void object.refetch()} />
        </Card>
      ) : (
        <Tabs defaultValue={object.data && object.data.content_type ? 'preview' : 'overview'}>
          <TabsList>
            {object.data && object.data.content_type ? (
              <TabsTrigger value="preview">Preview</TabsTrigger>
            ) : null}
            <TabsTrigger value="overview">Overview</TabsTrigger>
            {capabilities.versioning ? <TabsTrigger value="versions">Versions</TabsTrigger> : null}
            <TabsTrigger value="metadata">Metadata</TabsTrigger>
            <TabsTrigger value="integrity">Integrity</TabsTrigger>
            {capabilities.events ? <TabsTrigger value="activity">Activity</TabsTrigger> : null}
          </TabsList>

          {object.data && object.data.content_type ? (
            <TabsContent value="preview">
              <ObjectPreview bucket={bucket} record={object.data} />
            </TabsContent>
          ) : null}

          <TabsContent value="overview">
            <OverviewTab bucket={bucket} record={object.data ?? null} />
          </TabsContent>

          {capabilities.versioning ? (
            <TabsContent value="versions">
              <ObjectVersions bucket={bucket} prefixOverride={objectKey} />
            </TabsContent>
          ) : null}

          <TabsContent value="metadata">
            <MetadataTab record={object.data ?? null} />
          </TabsContent>

          <TabsContent value="integrity">
            <IntegrityTab bucket={bucket} objectKey={objectKey} record={object.data ?? null} />
          </TabsContent>

          {capabilities.events ? (
            <TabsContent value="activity">
              <ActivityTab bucket={bucket} objectKey={objectKey} />
            </TabsContent>
          ) : null}
        </Tabs>
      )}

      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title="Delete object"
        description={`${keyBasename(objectKey)} and its current version will be removed. This cannot be undone.`}
        confirmLabel="Delete object"
        strength="acknowledge"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => removal.mutate()}
      />
    </>
  );
}

/** Everything an operator needs to identify and trust one object version. */
function OverviewTab({
  bucket,
  record,
}: {
  readonly bucket: string;
  readonly record: ObjectSummary | null;
}) {
  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Object</CardTitle>
        <CardDescription>
          Identifiers OES publishes for this version. Physical storage details are deliberately not
          exposed here.
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-x-8 gap-y-3 sm:grid-cols-2">
        <Row label="Bucket" value={bucket} />
        <Row label="Key" value={record?.key ?? null} mono />
        <Row
          label="Size"
          value={record ? formatBytes(record.size) : null}
          extra={record ? `${record.size} bytes` : undefined}
        />
        <Row label="Content type" value={record?.content_type ?? '—'} />
        <Row label="Version" value={record?.version_id ?? null} mono />
        <Row label="ETag" value={record?.etag ?? null} mono />
        <Row label="Checksum" value={record?.checksum ?? null} mono />
        <Row label="Created" value={record ? formatDateTime(record.created_at) : null} />
        <Row label="Last modified" value={record ? formatDateTime(record.modified_at) : null} />
      </CardContent>
    </Card>
  );
}

function MetadataTab({ record }: { readonly record: ObjectSummary | null }) {
  const entries = Object.entries(record?.custom_metadata ?? {});
  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Custom metadata</CardTitle>
        <CardDescription>Keys the client supplied when the object was stored.</CardDescription>
      </CardHeader>
      <CardContent>
        {record === null ? (
          <Skeleton className="h-16 w-full" />
        ) : entries.length === 0 ? (
          <EmptyState
            title="No custom metadata"
            description="This object was stored without any x-amz-meta-* headers."
          />
        ) : (
          <dl className="grid gap-x-8 gap-y-3 sm:grid-cols-2">
            {entries.map(([key, value]) => (
              <div key={key} className="min-w-0 space-y-0.5">
                <dt className="font-mono type-meta">{key}</dt>
                <dd className="break-all type-body">{value}</dd>
              </div>
            ))}
          </dl>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Re-reads and re-hashes this one object.
 *
 * Verification proves whether the stored bytes still match the recorded
 * checksum. It cannot repair them, and the copy says so rather than offering a
 * fix that does not exist at this level.
 */
function IntegrityTab({
  bucket,
  objectKey,
  record,
}: {
  readonly bucket: string;
  readonly objectKey: string;
  readonly record: ObjectSummary | null;
}) {
  const permissions = usePermissions();
  const verification = useMutation({
    mutationFn: () => verifyObject(bucket, objectKey),
    onSuccess: () => toast.success('Object verified against its recorded checksum'),
  });

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Integrity</CardTitle>
        <CardDescription>
          Reads every byte of this object and compares the result with the checksum recorded when it
          was stored.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <Row label="Recorded checksum" value={record?.checksum ?? null} mono />
        {permissions.manage_storage ? (
          <Button
            size="sm"
            variant="secondary"
            disabled={verification.isPending}
            onClick={() => verification.mutate()}
          >
            {verification.isPending ? 'Verifying…' : 'Verify object'}
          </Button>
        ) : (
          <p className="type-meta">Your role does not permit running verification.</p>
        )}
        {verification.error ? <ErrorState error={verification.error} /> : null}
        {verification.data ? (
          <p className="text-sm text-ok" role="status">
            The stored bytes match the recorded checksum.
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}

/** Storage events for this object, newest first. */
function ActivityTab({
  bucket,
  objectKey,
}: {
  readonly bucket: string;
  readonly objectKey: string;
}) {
  const events = useQuery({
    queryKey: queryKeys.events(`object:${bucket}:${objectKey}`),
    queryFn: ({ signal }) => fetchStorageEvents({ bucket, prefix: objectKey, limit: 25 }, signal),
  });

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Activity</CardTitle>
        <CardDescription>
          Storage events recorded for this key. This is data activity, not the security audit trail.
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
            description="No storage events have been recorded for this object."
          />
        ) : (
          <ul className="divide-y divide-border">
            {events.data.events.map((event) => (
              <li key={event.id} className="flex flex-wrap items-baseline gap-x-3 py-2">
                <Badge tone="neutral">{event.type}</Badge>
                <span className="type-meta">
                  <time dateTime={event.time}>{formatDateTime(event.time)}</time>
                </span>
                {event.size === null ? null : (
                  <span className="text-xs tabular-nums text-ink-subtle">
                    {formatBytes(event.size)}
                  </span>
                )}
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

function Row({
  label,
  value,
  extra,
  mono = false,
  copy = false,
}: {
  readonly label: string;
  readonly value: string | null;
  readonly extra?: string;
  readonly mono?: boolean;
  readonly copy?: boolean;
}) {
  return (
    <div className="min-w-0 space-y-1">
      <dt className="type-meta">{label}</dt>
      <dd className="flex min-w-0 items-center gap-2">
        {value === null ? (
          <Skeleton className="h-5 w-32" />
        ) : (
          <>
            <span className={mono ? 'break-all font-mono text-xs text-ink' : 'type-body'}>
              {value}
            </span>
            {copy ? <CopyButton value={value} label={label} variant="ghost" /> : null}
          </>
        )}
      </dd>
      {extra ? <p className="type-meta-subtle">{extra}</p> : null}
    </div>
  );
}
