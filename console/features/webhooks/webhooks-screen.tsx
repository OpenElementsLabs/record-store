'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { MoreHorizontal, Plus } from 'lucide-react';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { EmptyState } from '@/components/empty-state';
import { ErrorDetails, ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { SecretOnceWarning, SecretReveal } from '@/components/secret-reveal';
import { StatusBadge } from '@/components/status-badge';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Input } from '@/components/ui/input';
import { Field } from '@/components/ui/label';
import { Skeleton, TableSkeleton } from '@/components/ui/skeleton';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableShell,
} from '@/components/ui/table';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { summariseDeliveries } from '@/features/webhooks/delivery-health';
import { queryKeys } from '@/hooks/use-system';
import {
  createWebhook,
  deleteWebhook,
  fetchWebhookDeliveries,
  fetchWebhooks,
  setWebhookEnabled,
} from '@/lib/api/observability';
import { ApiError } from '@/lib/api/error';
import { formatCount, formatDateTime, shortenIdentifier } from '@/lib/format';
import type {
  CreatedWebhook,
  StorageEventType,
  WebhookDeliveryLog,
  WebhookSubscription,
} from '@/types/api';

const EVENT_TYPES: readonly StorageEventType[] = [
  'object.created',
  'object.updated',
  'object.deleted',
  'object.restored',
  'bucket.created',
  'bucket.deleted',
  'multipart.completed',
  'multipart.aborted',
];

export function WebhooksScreen() {
  const client = useQueryClient();
  const [creating, setCreating] = React.useState(false);
  const [created, setCreated] = React.useState<CreatedWebhook | null>(null);
  const [pendingDelete, setPendingDelete] = React.useState<WebhookSubscription | null>(null);

  const deliveries = useQuery({
    queryKey: queryKeys.webhookDeliveries,
    queryFn: ({ signal }) => fetchWebhookDeliveries(100, signal),
  });

  const webhooks = useQuery({
    queryKey: queryKeys.webhooks,
    queryFn: ({ signal }) => fetchWebhooks(signal),
  });

  const invalidate = () => client.invalidateQueries({ queryKey: queryKeys.webhooks });

  const status = useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      setWebhookEnabled(id, enabled),
    onSuccess: async (_result, variables) => {
      toast.success(variables.enabled ? 'Webhook enabled' : 'Webhook disabled');
      await invalidate();
    },
  });

  const removal = useMutation({
    mutationFn: (id: string) => deleteWebhook(id),
    onSuccess: async () => {
      toast.success('Webhook deleted');
      setPendingDelete(null);
      await invalidate();
    },
  });

  return (
    <>
      <PageHeader
        title="Webhooks"
        description="Signed HTTP deliveries of storage events to an external endpoint."
        actions={
          <Button variant="primary" onClick={() => setCreating(true)}>
            <Plus aria-hidden />
            Create webhook
          </Button>
        }
      />

      <Tabs defaultValue="subscriptions">
        <TabsList>
          <TabsTrigger value="subscriptions">Subscriptions</TabsTrigger>
          <TabsTrigger value="deliveries">Delivery history</TabsTrigger>
        </TabsList>

        <TabsContent value="subscriptions">
          <Card>
            {webhooks.isError ? (
              <ErrorState error={webhooks.error} onRetry={() => void webhooks.refetch()} />
            ) : webhooks.isPending ? (
              <TableSkeleton columns={4} />
            ) : webhooks.data.length === 0 ? (
              <EmptyState
                title="No webhooks"
                description="Create a webhook to deliver storage events to an external service."
                action={
                  <Button variant="primary" onClick={() => setCreating(true)}>
                    <Plus aria-hidden />
                    Create webhook
                  </Button>
                }
              />
            ) : (
              <TableShell>
                <Table>
                  <TableHeader>
                    <TableRow className="hover:bg-transparent">
                      <TableHead>Endpoint</TableHead>
                      <TableHead>Events</TableHead>
                      <TableHead>Filters</TableHead>
                      <TableHead>Status</TableHead>
                      <TableHead>Recent deliveries</TableHead>
                      <TableHead>
                        <span className="sr-only">Actions</span>
                      </TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {webhooks.data.map((webhook) => (
                      <TableRow key={webhook.id}>
                        <TableCell className="max-w-xs truncate text-xs" title={webhook.target_url}>
                          {webhook.target_url}
                        </TableCell>
                        <TableCell>
                          <div className="flex flex-wrap gap-1">
                            {webhook.event_types.map((type) => (
                              <Badge key={type} tone="neutral" className="font-mono">
                                {type}
                              </Badge>
                            ))}
                          </div>
                        </TableCell>
                        <TableCell className="text-xs text-ink-muted">
                          {webhook.bucket_filter ?? 'any bucket'}
                          {webhook.object_prefix_filter ? ` · ${webhook.object_prefix_filter}` : ''}
                        </TableCell>
                        <TableCell>
                          <StatusBadge
                            level={webhook.enabled ? 'healthy' : 'disabled'}
                            label={webhook.enabled ? 'Enabled' : 'Disabled'}
                          />
                        </TableCell>
                        <TableCell>
                          <DeliveryBadge
                            deliveries={deliveries.data ?? null}
                            webhookId={webhook.id}
                          />
                        </TableCell>
                        <TableCell>
                          <div className="flex justify-end">
                            <DropdownMenu>
                              <DropdownMenuTrigger asChild>
                                <Button variant="ghost" size="icon" aria-label="Webhook actions">
                                  <MoreHorizontal aria-hidden />
                                </Button>
                              </DropdownMenuTrigger>
                              <DropdownMenuContent>
                                <DropdownMenuItem
                                  onSelect={() =>
                                    status.mutate({ id: webhook.id, enabled: !webhook.enabled })
                                  }
                                >
                                  {webhook.enabled ? 'Disable' : 'Enable'}
                                </DropdownMenuItem>
                                <DropdownMenuSeparator />
                                <DropdownMenuItem
                                  destructive
                                  onSelect={() => setPendingDelete(webhook)}
                                >
                                  Delete webhook
                                </DropdownMenuItem>
                              </DropdownMenuContent>
                            </DropdownMenu>
                          </div>
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </TableShell>
            )}
          </Card>
        </TabsContent>

        <TabsContent value="deliveries">
          <DeliveryHistory />
        </TabsContent>
      </Tabs>

      <CreateWebhookDialog
        open={creating}
        onOpenChange={setCreating}
        onCreated={(result) => {
          setCreated(result);
          setCreating(false);
          void invalidate();
        }}
      />

      {created ? (
        <Dialog open onOpenChange={(open) => !open && setCreated(null)}>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>Webhook created</DialogTitle>
              <DialogDescription>
                Use this secret to verify the signature on delivered events.
              </DialogDescription>
            </DialogHeader>
            <DialogBody>
              <SecretOnceWarning what="signing secret" />
              <SecretReveal label="Signing secret" value={created.signing_secret} />
            </DialogBody>
            <DialogFooter>
              <Button variant="primary" onClick={() => setCreated(null)}>
                Done
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      ) : null}

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPendingDelete(null);
            removal.reset();
          }
        }}
        title="Delete this webhook?"
        description={pendingDelete?.target_url ?? ''}
        consequence="Events will stop being delivered to this endpoint. Its signing secret cannot be recovered."
        confirmLabel="Delete webhook"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => {
          if (pendingDelete) removal.mutate(pendingDelete.id);
        }}
      />
    </>
  );
}

/** Recent delivery attempts, with the status and attempt count. */
/**
 * Delivery health for one webhook, within the fetched window.
 *
 * OES returns a bounded delivery log that cannot be filtered per webhook, so
 * this describes recent deliveries only. An empty result means nothing recent,
 * not nothing ever, and the label says so rather than implying a clean record.
 */
function DeliveryBadge({
  deliveries,
  webhookId,
}: {
  readonly deliveries: readonly WebhookDeliveryLog[] | null;
  readonly webhookId: string;
}) {
  if (deliveries === null) return <Skeleton className="h-5 w-20" />;
  const health = summariseDeliveries(deliveries, webhookId);
  if (health.total === 0) {
    return <span className="text-xs text-ink-subtle">none recently</span>;
  }
  return (
    <div className="space-y-0.5">
      <span
        className={health.failed > 0 ? 'text-xs text-danger' : 'text-xs text-ok'}
        title={health.lastError ?? undefined}
      >
        {health.failed === 0
          ? `${formatCount(health.total)} delivered`
          : `${formatCount(health.failed)} of ${formatCount(health.total)} failed`}
      </span>
      {health.lastAttemptAt ? (
        <p className="text-xs text-ink-subtle">
          last <time dateTime={health.lastAttemptAt}>{formatDateTime(health.lastAttemptAt)}</time>
        </p>
      ) : null}
    </div>
  );
}

function DeliveryHistory() {
  const deliveries = useQuery({
    queryKey: queryKeys.webhookDeliveries,
    queryFn: ({ signal }) => fetchWebhookDeliveries(100, signal),
    refetchInterval: 30_000,
  });

  return (
    <Card>
      <CardHeader>
        <CardTitle>Recent deliveries</CardTitle>
      </CardHeader>
      {deliveries.isError ? (
        <ErrorState error={deliveries.error} onRetry={() => void deliveries.refetch()} />
      ) : deliveries.isPending ? (
        <TableSkeleton columns={4} />
      ) : deliveries.data.length === 0 ? (
        <EmptyState
          title="No deliveries yet"
          description="Delivery attempts appear here once events start matching a webhook."
        />
      ) : (
        <TableShell>
          <Table>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead>Attempted</TableHead>
                <TableHead>Webhook</TableHead>
                <TableHead>Attempts</TableHead>
                <TableHead>HTTP status</TableHead>
                <TableHead>Result</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {deliveries.data.map((log) => (
                <TableRow key={`${log.webhook_id}:${log.event_id}:${log.delivered_at}`}>
                  <TableCell className="whitespace-nowrap text-xs text-ink-muted">
                    <time dateTime={log.delivered_at}>{formatDateTime(log.delivered_at)}</time>
                  </TableCell>
                  <TableCell className="font-mono text-xs" title={log.webhook_id}>
                    {shortenIdentifier(log.webhook_id, 6)}
                  </TableCell>
                  <TableCell className="tabular-nums text-xs">{log.attempts}</TableCell>
                  <TableCell className="tabular-nums text-xs">{log.status_code ?? '—'}</TableCell>
                  <TableCell>
                    {log.success ? (
                      <StatusBadge level="healthy" label="Delivered" />
                    ) : (
                      <StatusBadge level="critical" label="Failed" />
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </TableShell>
      )}
    </Card>
  );
}

function CreateWebhookDialog({
  open,
  onOpenChange,
  onCreated,
}: {
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
  readonly onCreated: (created: CreatedWebhook) => void;
}) {
  const [url, setUrl] = React.useState('');
  const [types, setTypes] = React.useState<readonly StorageEventType[]>(['object.created']);
  const [bucket, setBucket] = React.useState('');
  const [prefix, setPrefix] = React.useState('');

  const mutation = useMutation({
    mutationFn: () =>
      createWebhook({
        target_url: url,
        event_types: types,
        bucket_filter: bucket.trim() === '' ? null : bucket.trim(),
        object_prefix_filter: prefix.trim() === '' ? null : prefix.trim(),
        enabled: true,
      }),
    onSuccess: (result) => {
      setUrl('');
      setTypes(['object.created']);
      setBucket('');
      setPrefix('');
      onCreated(result);
    },
  });

  React.useEffect(() => {
    if (!open) mutation.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const error = mutation.error instanceof ApiError ? mutation.error : null;
  const valid = url.trim().length > 0 && types.length > 0;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create webhook</DialogTitle>
          <DialogDescription>
            OES requires HTTPS endpoints by default and refuses private network targets unless the
            server was configured to allow them.
          </DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (valid) mutation.mutate();
          }}
        >
          <DialogBody>
            <Field label="Endpoint URL" htmlFor="webhook-url" error={error?.message ?? null}>
              <Input
                type="url"
                value={url}
                placeholder="https://example.com/oes-events"
                required
                onChange={(event) => setUrl(event.target.value)}
              />
            </Field>
            <fieldset className="space-y-2">
              <legend className="text-xs font-medium text-ink">Event types</legend>
              <div className="grid gap-1.5 sm:grid-cols-2">
                {EVENT_TYPES.map((type) => (
                  <label key={type} className="flex items-center gap-2 text-xs text-ink">
                    <input
                      type="checkbox"
                      checked={types.includes(type)}
                      onChange={(event) =>
                        setTypes((current) =>
                          event.target.checked
                            ? [...current, type]
                            : current.filter((item) => item !== type),
                        )
                      }
                    />
                    <span className="font-mono">{type}</span>
                  </label>
                ))}
              </div>
            </fieldset>
            <Field label="Bucket filter" htmlFor="webhook-bucket" hint="Optional.">
              <Input value={bucket} onChange={(event) => setBucket(event.target.value)} />
            </Field>
            <Field label="Key prefix filter" htmlFor="webhook-prefix" hint="Optional.">
              <Input value={prefix} onChange={(event) => setPrefix(event.target.value)} />
            </Field>
            {error ? <ErrorDetails error={error} /> : null}
          </DialogBody>
          <DialogFooter>
            <Button variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="primary" disabled={!valid || mutation.isPending}>
              {mutation.isPending ? 'Creating…' : 'Create webhook'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
