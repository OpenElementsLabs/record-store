'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Code2, Link2, MoreHorizontal, Plus, Share2 } from 'lucide-react';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Skeleton } from '@/components/ui/skeleton';
import {
  CapabilityStatusBadge,
  ExpiryLabel,
  OriginBadge,
  PasswordBadge,
  VersionModeBadge,
} from '@/features/sharing/capability-status';
import { CreateEmbedDialog } from '@/features/sharing/create-embed-dialog';
import { CreateShareDialog } from '@/features/sharing/create-share-dialog';
import { embedSnippet } from '@/features/sharing/embed-snippet';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import {
  absoluteCapabilityUrl,
  deleteEmbed,
  deleteShare,
  fetchEmbedUrl,
  fetchObjectEmbeds,
  fetchObjectShares,
  fetchShareUrl,
  fetchSharingSettings,
  revokeEmbed,
  revokeShare,
} from '@/lib/api/sharing';
import { formatCount, formatDateTime } from '@/lib/format';
import type { EmbedLink, ShareLink } from '@/types/api';

/**
 * Everything currently pointing at one object from outside Record Store.
 *
 * Shares and embeds are listed apart because they are different things: one is a
 * page a person opens, the other is a URL an application fetches. Merging them
 * into one table would make "revoke everything the marketing site uses" a
 * reading exercise.
 */
export function SharingTab({
  bucket,
  objectKey,
  contentType,
  versionId,
}: {
  readonly bucket: string;
  readonly objectKey: string;
  readonly contentType: string | null;
  readonly versionId?: string | undefined;
}) {
  const permissions = usePermissions();
  const [creatingShare, setCreatingShare] = React.useState(false);
  const [creatingEmbed, setCreatingEmbed] = React.useState(false);

  const settings = useQuery({
    queryKey: queryKeys.sharingSettings,
    queryFn: ({ signal }) => fetchSharingSettings(signal),
    staleTime: 300_000,
  });

  const shares = useQuery({
    queryKey: queryKeys.objectShares(bucket, objectKey),
    queryFn: ({ signal }) => fetchObjectShares(bucket, objectKey, signal),
  });

  const embeds = useQuery({
    queryKey: queryKeys.objectEmbeds(bucket, objectKey),
    queryFn: ({ signal }) => fetchObjectEmbeds(bucket, objectKey, signal),
  });

  if (settings.isError) {
    return (
      <Card>
        <ErrorState error={settings.error} onRetry={() => void settings.refetch()} />
      </Card>
    );
  }

  const canManage = permissions.manage_sharing;
  const sharesEnabled = settings.data?.shares_enabled ?? false;
  const embedsEnabled = settings.data?.embeds_enabled ?? false;

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="flex-col items-start gap-3 sm:flex-row sm:items-center">
          <div className="min-w-0">
            <CardTitle>Share links</CardTitle>
            <CardDescription>
              For people. Each link opens a Record Store page showing this object and nothing else.
            </CardDescription>
          </div>
          {canManage && sharesEnabled && settings.data ? (
            <Button
              size="sm"
              variant="primary"
              className="sm:ml-auto"
              onClick={() => setCreatingShare(true)}
            >
              <Plus aria-hidden />
              Create share link
            </Button>
          ) : null}
        </CardHeader>
        <CardContent>
          {shares.isError ? (
            <ErrorState error={shares.error} onRetry={() => void shares.refetch()} />
          ) : shares.isPending ? (
            <Skeleton className="h-20 w-full" />
          ) : shares.data.length === 0 ? (
            <EmptyState
              icon={Share2}
              title="No share links"
              description={
                sharesEnabled
                  ? 'Create one to let someone read this object without a Record Store account.'
                  : 'Share links are disabled for this deployment.'
              }
            />
          ) : (
            <ul className="divide-y divide-border">
              {shares.data.map((share) => (
                <ShareRow
                  key={share.id}
                  share={share}
                  bucket={bucket}
                  objectKey={objectKey}
                  canManage={canManage}
                />
              ))}
            </ul>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="flex-col items-start gap-3 sm:flex-row sm:items-center">
          <div className="min-w-0">
            <CardTitle>Embeds</CardTitle>
            <CardDescription>
              For websites and applications. A read-only URL that serves these bytes directly.
            </CardDescription>
          </div>
          {canManage && embedsEnabled && settings.data ? (
            <Button
              size="sm"
              variant="primary"
              className="sm:ml-auto"
              onClick={() => setCreatingEmbed(true)}
            >
              <Plus aria-hidden />
              Create embed
            </Button>
          ) : null}
        </CardHeader>
        <CardContent>
          {embeds.isError ? (
            <ErrorState error={embeds.error} onRetry={() => void embeds.refetch()} />
          ) : embeds.isPending ? (
            <Skeleton className="h-20 w-full" />
          ) : embeds.data.length === 0 ? (
            <EmptyState
              icon={Code2}
              title="No embeds"
              description={
                embedsEnabled
                  ? 'Create one to use this object on a site you control.'
                  : 'Embeds are disabled for this deployment.'
              }
            />
          ) : (
            <ul className="divide-y divide-border">
              {embeds.data.map((embed) => (
                <EmbedRow
                  key={embed.id}
                  embed={embed}
                  bucket={bucket}
                  objectKey={objectKey}
                  canManage={canManage}
                />
              ))}
            </ul>
          )}
        </CardContent>
      </Card>

      {settings.data ? (
        <>
          <CreateShareDialog
            bucket={bucket}
            objectKey={objectKey}
            versionId={versionId}
            settings={settings.data}
            open={creatingShare}
            onOpenChange={setCreatingShare}
          />
          <CreateEmbedDialog
            bucket={bucket}
            objectKey={objectKey}
            contentType={contentType}
            versionId={versionId}
            settings={settings.data}
            open={creatingEmbed}
            onOpenChange={setCreatingEmbed}
          />
        </>
      ) : null}
    </div>
  );
}

/**
 * Copies a capability URL, fetched at the moment it is needed.
 *
 * The clipboard write has to happen in the same user gesture that started it, so
 * the URL is fetched first and written second rather than the other way around;
 * a browser that has already lost the gesture silently refuses the write, and
 * the fallback tells the operator what the link is instead of failing quietly.
 */
function useCopyCapabilityUrl(
  fetcher: (id: string) => Promise<{ readonly url: string | null; readonly available: boolean }>,
  what: string,
) {
  return useMutation({
    mutationFn: async (id: string) => {
      const result = await fetcher(id);
      if (!result.available || !result.url) {
        throw new Error(
          `This ${what} can no longer be displayed. It still works; revoke and recreate it if you need the URL.`,
        );
      }
      const url = absoluteCapabilityUrl(result.url);
      try {
        await navigator.clipboard.writeText(url);
        return { url, copied: true };
      } catch {
        return { url, copied: false };
      }
    },
    onSuccess: (result) => {
      toast[result.copied ? 'success' : 'message'](
        result.copied ? `Copied the ${what}` : `Could not use the clipboard`,
        result.copied ? undefined : { description: result.url },
      );
    },
    onError: (error: unknown) => {
      toast.error(error instanceof Error ? error.message : `Could not read the ${what}`);
    },
  });
}

function ShareRow({
  share,
  bucket,
  objectKey,
  canManage,
}: {
  readonly share: ShareLink;
  readonly bucket: string;
  readonly objectKey: string;
  readonly canManage: boolean;
}) {
  const client = useQueryClient();
  const [confirming, setConfirming] = React.useState<'revoke' | 'delete' | null>(null);
  const copy = useCopyCapabilityUrl(fetchShareUrl, 'share link');

  const withdrawal = useMutation({
    mutationFn: () => revokeShare(share.id),
    onSuccess: async () => {
      toast.success(`Revoked ${share.label}`);
      setConfirming(null);
      await client.invalidateQueries({ queryKey: queryKeys.objectShares(bucket, objectKey) });
    },
  });

  const removal = useMutation({
    mutationFn: () => deleteShare(share.id),
    onSuccess: async () => {
      toast.success(`Deleted the record for ${share.label}`);
      setConfirming(null);
      await client.invalidateQueries({ queryKey: queryKeys.objectShares(bucket, objectKey) });
    },
  });

  const usable = share.status === 'active';
  return (
    <li className="flex flex-wrap items-start gap-x-4 gap-y-2 py-3">
      <div className="min-w-0 flex-1 space-y-1.5">
        <div className="flex flex-wrap items-center gap-2">
          <span className="type-body font-medium">{share.label}</span>
          <CapabilityStatusBadge status={share.status} />
          <VersionModeBadge mode={share.version_mode} />
          {share.password_protected ? <PasswordBadge /> : null}
        </div>
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <ExpiryLabel expiresAt={share.expires_at} />
          <span className="type-meta">
            {formatCount(share.access_count)}
            {share.maximum_access_count === null
              ? ' opens'
              : ` of ${formatCount(share.maximum_access_count)} opens`}
          </span>
          {share.last_accessed_at ? (
            <span className="type-meta-subtle">
              Last opened {formatDateTime(share.last_accessed_at)}
            </span>
          ) : null}
        </div>
      </div>
      <div className="flex items-center gap-1">
        {usable ? (
          <Button
            size="sm"
            variant="secondary"
            disabled={copy.isPending}
            onClick={() => copy.mutate(share.id)}
          >
            <Link2 aria-hidden />
            {copy.isPending ? 'Copying…' : 'Copy'}
          </Button>
        ) : null}
        {canManage ? (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" size="icon" aria-label={`Actions for ${share.label}`}>
                <MoreHorizontal aria-hidden />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent>
              {usable ? (
                <DropdownMenuItem destructive onSelect={() => setConfirming('revoke')}>
                  Revoke link
                </DropdownMenuItem>
              ) : (
                <DropdownMenuItem destructive onSelect={() => setConfirming('delete')}>
                  Delete record
                </DropdownMenuItem>
              )}
            </DropdownMenuContent>
          </DropdownMenu>
        ) : null}
      </div>

      <ConfirmDialog
        open={confirming === 'revoke'}
        onOpenChange={(open) => (open ? undefined : setConfirming(null))}
        title={`Revoke ${share.label}?`}
        description="The next request against this link fails. Anyone who already downloaded the object keeps their copy — Record Store controls future access, not copies already made."
        confirmLabel="Revoke link"
        pending={withdrawal.isPending}
        error={withdrawal.error}
        onConfirm={() => withdrawal.mutate()}
      />
      <ConfirmDialog
        open={confirming === 'delete'}
        onOpenChange={(open) => (open ? undefined : setConfirming(null))}
        title={`Delete the record for ${share.label}?`}
        description="The link is already inert. This removes it from this list; the audit trail keeps what happened."
        confirmLabel="Delete record"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => removal.mutate()}
      />
    </li>
  );
}

function EmbedRow({
  embed,
  bucket,
  objectKey,
  canManage,
}: {
  readonly embed: EmbedLink;
  readonly bucket: string;
  readonly objectKey: string;
  readonly canManage: boolean;
}) {
  const client = useQueryClient();
  const [confirming, setConfirming] = React.useState<'revoke' | 'delete' | null>(null);
  const copy = useCopyCapabilityUrl(fetchEmbedUrl, 'embed URL');

  const copySnippet = useMutation({
    mutationFn: async () => {
      const result = await fetchEmbedUrl(embed.id);
      if (!result.available || !result.url) {
        throw new Error('This embed URL can no longer be displayed.');
      }
      const snippet = embedSnippet(absoluteCapabilityUrl(result.url), embed.content_type);
      if (!snippet) throw new Error('This media type has no HTML snippet.');
      await navigator.clipboard.writeText(snippet.code);
      return snippet.code;
    },
    onSuccess: () => toast.success('Copied the embed HTML'),
    onError: (error: unknown) =>
      toast.error(error instanceof Error ? error.message : 'Could not copy the snippet'),
  });

  const withdrawal = useMutation({
    mutationFn: () => revokeEmbed(embed.id),
    onSuccess: async () => {
      toast.success(`Revoked ${embed.label}`);
      setConfirming(null);
      await client.invalidateQueries({ queryKey: queryKeys.objectEmbeds(bucket, objectKey) });
    },
  });

  const removal = useMutation({
    mutationFn: () => deleteEmbed(embed.id),
    onSuccess: async () => {
      toast.success(`Deleted the record for ${embed.label}`);
      setConfirming(null);
      await client.invalidateQueries({ queryKey: queryKeys.objectEmbeds(bucket, objectKey) });
    },
  });

  const usable = embed.status === 'active';
  const hasSnippet =
    embed.disposition === 'inline' &&
    embedSnippet('https://example.invalid', embed.content_type) !== null;

  return (
    <li className="flex flex-wrap items-start gap-x-4 gap-y-2 py-3">
      <div className="min-w-0 flex-1 space-y-1.5">
        <div className="flex flex-wrap items-center gap-2">
          <span className="type-body font-medium">{embed.label}</span>
          <CapabilityStatusBadge status={embed.status} />
          <VersionModeBadge mode={embed.version_mode} />
          <OriginBadge count={embed.allowed_origins.length} />
        </div>
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <ExpiryLabel expiresAt={embed.expires_at} />
          <span className="type-meta">{embed.content_type}</span>
          {embed.allowed_origins.length > 0 ? (
            <span className="truncate font-mono type-meta-subtle">
              {embed.allowed_origins.join(' · ')}
            </span>
          ) : null}
        </div>
      </div>
      <div className="flex items-center gap-1">
        {usable ? (
          <Button
            size="sm"
            variant="secondary"
            disabled={copy.isPending}
            onClick={() => copy.mutate(embed.id)}
          >
            <Link2 aria-hidden />
            {copy.isPending ? 'Copying…' : 'Copy URL'}
          </Button>
        ) : null}
        {usable && hasSnippet ? (
          <Button
            size="sm"
            variant="secondary"
            disabled={copySnippet.isPending}
            onClick={() => copySnippet.mutate()}
          >
            <Code2 aria-hidden />
            Copy HTML
          </Button>
        ) : null}
        {canManage ? (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" size="icon" aria-label={`Actions for ${embed.label}`}>
                <MoreHorizontal aria-hidden />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent>
              {usable ? (
                <DropdownMenuItem destructive onSelect={() => setConfirming('revoke')}>
                  Revoke embed
                </DropdownMenuItem>
              ) : (
                <DropdownMenuItem destructive onSelect={() => setConfirming('delete')}>
                  Delete record
                </DropdownMenuItem>
              )}
            </DropdownMenuContent>
          </DropdownMenu>
        ) : null}
      </div>

      <ConfirmDialog
        open={confirming === 'revoke'}
        onOpenChange={(open) => (open ? undefined : setConfirming(null))}
        title={`Revoke ${embed.label}?`}
        description="Pages using this embed stop loading the object."
        consequence="Browsers and caches may keep a copy for up to a minute. Bytes already fetched cannot be recalled."
        confirmLabel="Revoke embed"
        pending={withdrawal.isPending}
        error={withdrawal.error}
        onConfirm={() => withdrawal.mutate()}
      />
      <ConfirmDialog
        open={confirming === 'delete'}
        onOpenChange={(open) => (open ? undefined : setConfirming(null))}
        title={`Delete the record for ${embed.label}?`}
        description="The embed is already inert. This removes it from this list; the audit trail keeps what happened."
        confirmLabel="Delete record"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => removal.mutate()}
      />
    </li>
  );
}
