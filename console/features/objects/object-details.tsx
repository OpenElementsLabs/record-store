'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Code2, Download, Share2, Trash2 } from 'lucide-react';
import { usePathname, useRouter, useSearchParams } from 'next/navigation';
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
import { CreateEmbedDialog } from '@/features/sharing/create-embed-dialog';
import { CreateShareDialog } from '@/features/sharing/create-share-dialog';
import { SharingTab } from '@/features/sharing/sharing-tab';
import { ApiError } from '@/lib/api/error';
import { verifyObject } from '@/lib/api/integrity';
import { fetchStorageEvents } from '@/lib/api/observability';
import { fetchSharingSettings } from '@/lib/api/sharing';
import { Skeleton } from '@/components/ui/skeleton';
import { useCapabilities, usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { deleteObject, fetchObject, objectContentUrl } from '@/lib/api/objects';
import { formatBytes, formatDateTime, keyBasename, keySegments } from '@/lib/format';
import { isPreviewable, previewKind } from '@/lib/preview-kind';
import { mergeSearch, readString } from '@/lib/search-params';
import type { ObjectSummary } from '@/types/api';

/**
 * One object, and everything OES can do with it.
 *
 * The tabs are ordered by what an operator opening this page usually wants:
 * seeing the thing, then identifying it, then its history, then who outside OES
 * can reach it. Preview leads when the object has one, because a screen that
 * opens on a checksum table when the object is a photograph is a screen that
 * makes its reader do the work.
 *
 * A `version` search parameter pins the whole screen to one immutable version.
 * Every request made here then names that version, so viewing history never
 * shows the current bytes by accident.
 */
export function ObjectDetails({
  bucket,
  objectKey,
}: {
  readonly bucket: string;
  readonly objectKey: string;
}) {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();
  const client = useQueryClient();
  const permissions = usePermissions();
  const capabilities = useCapabilities();
  const [confirming, setConfirming] = React.useState(false);
  const [sharing, setSharing] = React.useState<'share' | 'embed' | null>(null);

  const versionId = readString(params, 'version', '') || undefined;
  // The tab lives in the URL so a row action can deep-link into Sharing or
  // Versions, and so a reader can send someone the exact view they are looking
  // at rather than "open the object and click the third tab".
  const requestedTab = readString(params, 'tab', '');

  const object = useQuery({
    queryKey: [...queryKeys.object(bucket, objectKey), versionId ?? 'current'],
    queryFn: ({ signal }) => fetchObject(bucket, objectKey, versionId, signal),
  });

  const settings = useQuery({
    queryKey: queryKeys.sharingSettings,
    queryFn: ({ signal }) => fetchSharingSettings(signal),
    staleTime: 300_000,
    // Sharing may be switched off for a deployment; the screen must still work.
    retry: false,
  });

  const removal = useMutation({
    mutationFn: () => deleteObject(bucket, objectKey),
    onSuccess: async () => {
      toast.success(`Deleted ${keyBasename(objectKey)}`);
      await client.invalidateQueries({ queryKey: ['buckets', bucket] });
      router.push(`/buckets/${encodeURIComponent(bucket)}`);
    },
  });

  // A delete marker and a purged key are both "there are no bytes here", and
  // both leave the rest of the key's history intact.
  const missingCurrentVersion =
    object.error instanceof ApiError &&
    (object.error.code === 'OBJECT_NOT_FOUND' || object.error.code === 'OBJECT_DELETED');

  const kind = previewKind(object.data?.content_type);
  const previewable = object.data !== undefined && isPreviewable(kind);
  const sharingAvailable =
    settings.data !== undefined && (settings.data.shares_enabled || settings.data.embeds_enabled);

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
          <div className="flex flex-wrap items-center gap-2">
            <Button asChild variant="secondary">
              <a href={objectContentUrl(bucket, objectKey, versionId)} download>
                <Download aria-hidden />
                Download
              </a>
            </Button>
            {permissions.manage_sharing && settings.data?.shares_enabled ? (
              <Button variant="secondary" onClick={() => setSharing('share')}>
                <Share2 aria-hidden />
                Share
              </Button>
            ) : null}
            {permissions.manage_sharing && settings.data?.embeds_enabled ? (
              <Button variant="secondary" onClick={() => setSharing('embed')}>
                <Code2 aria-hidden />
                Embed
              </Button>
            ) : null}
            {permissions.manage_objects ? (
              <Button variant="danger" onClick={() => setConfirming(true)}>
                Delete
              </Button>
            ) : null}
          </div>
        }
      />

      {versionId ? (
        <Card>
          <div className="flex flex-wrap items-center gap-3 px-4 py-3">
            <Badge tone="info">Historical version</Badge>
            <span className="break-all font-mono type-meta">{versionId}</span>
            <Button
              size="sm"
              variant="ghost"
              className="ml-auto"
              onClick={() => router.push(`${pathname}${mergeSearch(params, { version: null })}`)}
            >
              Show current version
            </Button>
          </div>
        </Card>
      ) : null}

      {object.isError && missingCurrentVersion ? (
        // A key whose current version is a delete marker still has history, and
        // that history is exactly what the reader came for. Showing only an
        // error would hide versions that are still stored and still readable.
        <Tabs key="deleted" defaultValue={capabilities.versioning ? 'versions' : 'overview'}>
          <TabsList>
            <TabsTrigger value="overview">Overview</TabsTrigger>
            {capabilities.versioning ? <TabsTrigger value="versions">Versions</TabsTrigger> : null}
            {capabilities.events ? <TabsTrigger value="activity">Activity</TabsTrigger> : null}
          </TabsList>
          <TabsContent value="overview">
            <Card>
              <EmptyState
                icon={Trash2}
                title="This object has been deleted"
                description={
                  capabilities.versioning
                    ? 'The current version of this key is a delete marker, so there are no bytes to show. Earlier versions are still stored and can be previewed, downloaded, or restored from the Versions tab.'
                    : 'There is no object stored under this key.'
                }
              />
            </Card>
          </TabsContent>
          {capabilities.versioning ? (
            <TabsContent value="versions">
              <ObjectVersions bucket={bucket} prefixOverride={objectKey} />
            </TabsContent>
          ) : null}
          {capabilities.events ? (
            <TabsContent value="activity">
              <ActivityTab bucket={bucket} objectKey={objectKey} />
            </TabsContent>
          ) : null}
        </Tabs>
      ) : object.isError ? (
        <Card>
          <ErrorState error={object.error} onRetry={() => void object.refetch()} />
        </Card>
      ) : object.isPending ? (
        // The tabs are not rendered until the object's media type is known.
        // Which tab leads depends on that answer, and a tab strip that reshuffles
        // itself a moment after it appears is worse than one that waits.
        <Card>
          <CardContent className="py-8">
            <Skeleton className="h-64 w-full" />
          </CardContent>
        </Card>
      ) : (
        <Tabs
          // Keyed on the pinned version, so opening a historical version starts
          // the tab strip afresh rather than leaving the reader on whichever tab
          // they used to get there.
          key={versionId ?? 'current'}
          defaultValue={
            requestedTab.length > 0 ? requestedTab : previewable ? 'preview' : 'overview'
          }
          onValueChange={(value) =>
            router.replace(`${pathname}${mergeSearch(params, { tab: value })}`, { scroll: false })
          }
        >
          <TabsList>
            {previewable ? <TabsTrigger value="preview">Preview</TabsTrigger> : null}
            <TabsTrigger value="overview">Overview</TabsTrigger>
            {capabilities.versioning ? <TabsTrigger value="versions">Versions</TabsTrigger> : null}
            <TabsTrigger value="metadata">Metadata</TabsTrigger>
            {sharingAvailable ? <TabsTrigger value="sharing">Sharing</TabsTrigger> : null}
            {capabilities.events ? <TabsTrigger value="activity">Activity</TabsTrigger> : null}
            <TabsTrigger value="integrity">Integrity</TabsTrigger>
          </TabsList>

          {previewable ? (
            <TabsContent value="preview">
              <ObjectPreview
                bucket={bucket}
                record={object.data ?? null}
                versionId={versionId}
                textLimitBytes={settings.data?.preview_text_limit_bytes}
              />
            </TabsContent>
          ) : null}

          <TabsContent value="overview">
            <OverviewTab
              bucket={bucket}
              record={object.data ?? null}
              versionId={versionId}
              previewable={previewable}
              downloadUrl={objectContentUrl(bucket, objectKey, versionId)}
            />
          </TabsContent>

          {capabilities.versioning ? (
            <TabsContent value="versions">
              <ObjectVersions bucket={bucket} prefixOverride={objectKey} />
            </TabsContent>
          ) : null}

          <TabsContent value="metadata">
            <MetadataTab record={object.data ?? null} />
          </TabsContent>

          {sharingAvailable ? (
            <TabsContent value="sharing">
              <SharingTab
                bucket={bucket}
                objectKey={objectKey}
                contentType={object.data?.content_type ?? null}
                versionId={versionId}
              />
            </TabsContent>
          ) : null}

          {capabilities.events ? (
            <TabsContent value="activity">
              <ActivityTab bucket={bucket} objectKey={objectKey} />
            </TabsContent>
          ) : null}

          <TabsContent value="integrity">
            <IntegrityTab bucket={bucket} objectKey={objectKey} record={object.data ?? null} />
          </TabsContent>
        </Tabs>
      )}

      {settings.data ? (
        <>
          <CreateShareDialog
            bucket={bucket}
            objectKey={objectKey}
            versionId={versionId}
            settings={settings.data}
            open={sharing === 'share'}
            onOpenChange={(open) => setSharing(open ? 'share' : null)}
          />
          <CreateEmbedDialog
            bucket={bucket}
            objectKey={objectKey}
            contentType={object.data?.content_type ?? null}
            versionId={versionId}
            settings={settings.data}
            open={sharing === 'embed'}
            onOpenChange={(open) => setSharing(open ? 'embed' : null)}
          />
        </>
      ) : null}

      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title="Delete object"
        description={`${keyBasename(objectKey)} and its current version will be removed. This cannot be undone.`}
        consequence="Share and embed links pointing at this object stop working. Copies already downloaded are unaffected."
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
  versionId,
  previewable,
  downloadUrl,
}: {
  readonly bucket: string;
  readonly record: ObjectSummary | null;
  readonly versionId: string | undefined;
  readonly previewable: boolean;
  readonly downloadUrl: string;
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
      <CardContent className="space-y-4">
        <dl className="grid gap-x-8 gap-y-3 sm:grid-cols-2">
          <Row label="Bucket" value={bucket} />
          <Row label="Key" value={record?.key ?? null} mono />
          <Row
            label="Size"
            value={record ? formatBytes(record.size) : null}
            extra={record ? `${record.size} bytes` : undefined}
          />
          <Row label="Content type" value={record?.content_type ?? '—'} />
          <Row label="Version" value={record?.version_id ?? null} mono copy />
          <Row label="ETag" value={record?.etag ?? null} mono />
          <Row label="Checksum" value={record?.checksum ?? null} mono />
          <Row label="Created" value={record ? formatDateTime(record.created_at) : null} />
          <Row label="Last modified" value={record ? formatDateTime(record.modified_at) : null} />
        </dl>
        {record && !previewable ? (
          <p className="type-meta">
            This object type cannot be shown in the browser safely.{' '}
            <a href={downloadUrl} download className="text-accent underline underline-offset-4">
              Download it
            </a>{' '}
            to inspect it.
          </p>
        ) : null}
        {versionId ? (
          <p className="type-meta-subtle">
            Showing an immutable historical version. It cannot be changed or replaced.
          </p>
        ) : null}
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
                {/*
                  Rendered as text, never as markup. Custom metadata is
                  caller-supplied and is exactly the kind of value an attacker
                  would fill with a script tag.
                */}
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
          Storage events recorded for this key. This is data activity, not the security audit trail:
          who created or revoked a share link is recorded there instead.
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
