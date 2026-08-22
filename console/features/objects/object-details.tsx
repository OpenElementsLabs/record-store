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
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { deleteObject, fetchObject, objectContentUrl } from '@/lib/api/objects';
import { formatBytes, formatDateTime, keyBasename, keySegments } from '@/lib/format';
import { mergeSearch } from '@/lib/search-params';

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
        <>
          <Card>
            <CardHeader>
              <CardTitle>Metadata</CardTitle>
            </CardHeader>
            <CardContent>
              <dl className="grid gap-x-8 gap-y-3 sm:grid-cols-2">
                <Row label="Key" value={objectKey} mono copy />
                <Row
                  label="Size"
                  value={object.data ? formatBytes(object.data.size) : null}
                  extra={object.data ? `${object.data.size} bytes` : undefined}
                />
                <Row label="Content type" value={object.data?.content_type ?? '—'} />
                <Row label="ETag" value={object.data?.etag ?? null} mono copy />
                <Row label="Checksum" value={object.data?.checksum ?? null} mono copy />
                <Row label="Version" value={object.data?.version_id ?? null} mono copy />
                <Row
                  label="Created"
                  value={object.data ? formatDateTime(object.data.created_at) : null}
                />
                <Row
                  label="Modified"
                  value={object.data ? formatDateTime(object.data.modified_at) : null}
                />
              </dl>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>Custom metadata</CardTitle>
            </CardHeader>
            <CardContent>
              {object.isPending ? (
                <Skeleton className="h-10 w-full" />
              ) : Object.keys(object.data.custom_metadata).length === 0 ? (
                <p className="text-sm text-ink-muted">
                  No custom metadata was supplied with this object.
                </p>
              ) : (
                <dl className="space-y-2">
                  {Object.entries(object.data.custom_metadata).map(([key, value]) => (
                    <div key={key} className="flex flex-wrap items-baseline gap-2">
                      <dt className="font-mono text-xs text-ink-muted">{key}</dt>
                      <dd className="font-mono text-xs text-ink">{value}</dd>
                    </div>
                  ))}
                </dl>
              )}
            </CardContent>
          </Card>
        </>
      )}

      <ConfirmDialog
        open={confirming}
        onOpenChange={(open) => {
          setConfirming(open);
          if (!open) removal.reset();
        }}
        title={`Delete ${keyBasename(objectKey)}?`}
        description="The current version of this object is deleted."
        consequence="In a versioning-enabled bucket this adds a delete marker; otherwise the object is removed permanently."
        confirmLabel="Delete object"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => removal.mutate()}
      />
    </>
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
      <dt className="text-xs text-ink-muted">{label}</dt>
      <dd className="flex min-w-0 items-center gap-2">
        {value === null ? (
          <Skeleton className="h-5 w-32" />
        ) : (
          <>
            <span className={mono ? 'break-all font-mono text-xs text-ink' : 'text-sm text-ink'}>
              {value}
            </span>
            {copy ? <CopyButton value={value} label={label} variant="ghost" /> : null}
          </>
        )}
      </dd>
      {extra ? <p className="text-xs text-ink-subtle">{extra}</p> : null}
    </div>
  );
}
