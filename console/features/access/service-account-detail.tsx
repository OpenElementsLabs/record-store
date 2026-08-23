'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { KeyRound } from 'lucide-react';
import * as React from 'react';
import { toast } from 'sonner';

import { Breadcrumbs } from '@/components/breadcrumbs';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableShell,
} from '@/components/ui/table';
import { CredentialDialog } from '@/features/access/credential-dialog';
import { TemporaryCredentialDialog } from '@/features/access/temporary-credential-dialog';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import {
  fetchPolicies,
  fetchServiceAccount,
  issueTemporaryCredential,
  rotateCredential,
  setCredentialEnabled,
} from '@/lib/api/access';
import { fetchAuditEvents } from '@/lib/api/observability';
import { ApiError } from '@/lib/api/error';
import { secondsRemaining } from '@/lib/credential-lifetime';
import { formatCount, formatDateTime, formatDuration } from '@/lib/format';
import type { Credential, IssuedCredential, ServiceAccountInfo } from '@/types/api';

/**
 * One service account.
 *
 * The account is the identity; credentials are how it authenticates and
 * policies are what it may do. Keeping those on separate tabs stops the three
 * being read as one thing, which is where access mistakes come from.
 */
export function ServiceAccountDetail({ accountId }: { readonly accountId: string }) {
  const permissions = usePermissions();
  const [issued, setIssued] = React.useState<IssuedCredential | null>(null);
  const [issuedKind, setIssuedKind] = React.useState<'rotated' | 'temporary'>('rotated');
  const [temporaryOpen, setTemporaryOpen] = React.useState(false);

  const account = useQuery({
    queryKey: queryKeys.serviceAccount(accountId),
    queryFn: ({ signal }) => fetchServiceAccount(accountId, signal),
  });

  const info = account.data ?? null;

  return (
    <>
      <Breadcrumbs
        items={[
          { label: 'Service accounts', href: '/service-accounts' },
          { label: info?.account.name ?? '…' },
        ]}
      />
      <PageHeader
        title={info?.account.name ?? 'Service account'}
        description={info?.account.description || 'A workload identity for the S3 API.'}
        actions={
          permissions.manage_service_accounts && info ? (
            <Button size="sm" variant="secondary" onClick={() => setTemporaryOpen(true)}>
              <KeyRound aria-hidden />
              Temporary credential
            </Button>
          ) : null
        }
      />

      {account.isError ? (
        <Card>
          <ErrorState error={account.error} onRetry={() => void account.refetch()} />
        </Card>
      ) : (
        <Tabs defaultValue="overview">
          <TabsList>
            <TabsTrigger value="overview">Overview</TabsTrigger>
            <TabsTrigger value="credentials">Credentials</TabsTrigger>
            <TabsTrigger value="policies">Policies</TabsTrigger>
            {permissions.read_audit ? <TabsTrigger value="activity">Activity</TabsTrigger> : null}
          </TabsList>

          <TabsContent value="overview">
            <Overview info={info} />
          </TabsContent>

          <TabsContent value="credentials">
            <Credentials
              info={info}
              onIssued={(result) => {
                setIssuedKind('rotated');
                setIssued(result);
              }}
            />
          </TabsContent>

          <TabsContent value="policies">
            <Policies info={info} />
          </TabsContent>

          {permissions.read_audit ? (
            <TabsContent value="activity">
              <Activity name={info?.account.name ?? null} />
            </TabsContent>
          ) : null}
        </Tabs>
      )}

      <CredentialDialog
        issued={issued}
        onClose={() => setIssued(null)}
        title={issuedKind === 'temporary' ? 'Temporary credential issued' : 'New credential issued'}
        description={
          issuedKind === 'temporary'
            ? 'This credential expires on its own and carries the account’s policies. The secret cannot be retrieved later.'
            : 'The previous credential is still active. Update your application, then disable the old credential.'
        }
      />

      <TemporaryCredentialDialog
        account={temporaryOpen ? info : null}
        pending={false}
        onCancel={() => setTemporaryOpen(false)}
        onIssue={(seconds) => {
          setTemporaryOpen(false);
          void issueTemporaryCredential(accountId, seconds)
            .then((result) => {
              setIssuedKind('temporary');
              setIssued(result);
            })
            .catch((error: unknown) =>
              toast.error(
                error instanceof ApiError ? error.message : 'Could not issue a credential',
              ),
            );
        }}
      />
    </>
  );
}

function Overview({ info }: { readonly info: ServiceAccountInfo | null }) {
  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Identity</CardTitle>
        <CardDescription>
          What this account is. A disabled account cannot authenticate at all, whatever its
          credentials say.
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-x-8 gap-y-3 sm:grid-cols-2">
        <Detail label="Name" value={info?.account.name ?? null} />
        <Detail
          label="Status"
          node={
            info ? (
              <StatusBadge
                level={info.account.disabled ? 'disabled' : 'healthy'}
                label={info.account.disabled ? 'Disabled' : 'Active'}
              />
            ) : null
          }
        />
        <Detail label="Created" value={info ? formatDateTime(info.account.created_at) : null} />
        <Detail label="Credentials" value={info ? formatCount(info.credentials.length) : null} />
        <Detail
          label="Policies attached"
          value={info ? formatCount(info.policy_bindings.length) : null}
        />
      </CardContent>
    </Card>
  );
}

function Credentials({
  info,
  onIssued,
}: {
  readonly info: ServiceAccountInfo | null;
  readonly onIssued: (issued: IssuedCredential) => void;
}) {
  const client = useQueryClient();
  const permissions = usePermissions();

  const invalidate = () =>
    client.invalidateQueries({ queryKey: ['service-accounts'], exact: false });

  const rotate = useMutation({
    mutationFn: () => rotateCredential(info?.account.id ?? ''),
    onSuccess: async (result) => {
      onIssued(result);
      await invalidate();
    },
    onError: (error) => toast.error(error instanceof ApiError ? error.message : 'Rotation failed'),
  });

  const toggle = useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      setCredentialEnabled(info?.account.id ?? '', id, enabled),
    onSuccess: async (_result, variables) => {
      toast.success(variables.enabled ? 'Credential enabled' : 'Credential disabled');
      await invalidate();
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : 'Could not change the credential'),
  });

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <div className="flex w-full items-start justify-between gap-3">
          <div>
            <CardTitle>Credentials</CardTitle>
            <CardDescription>
              Rotation issues a new credential and leaves the old one working, so an application can
              be moved across before anything is revoked.
            </CardDescription>
          </div>
          {permissions.manage_service_accounts && info ? (
            <Button size="sm" disabled={rotate.isPending} onClick={() => rotate.mutate()}>
              {rotate.isPending ? 'Rotating…' : 'Rotate'}
            </Button>
          ) : null}
        </div>
      </CardHeader>
      <CardContent>
        {info === null ? (
          <Skeleton className="h-24 w-full" />
        ) : info.credentials.length === 0 ? (
          <EmptyState
            title="No credentials"
            description="Rotate to issue a credential for this account."
          />
        ) : (
          <TableShell>
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead>Access key</TableHead>
                  <TableHead>Kind</TableHead>
                  <TableHead>Created</TableHead>
                  <TableHead>Status</TableHead>
                  {permissions.manage_service_accounts ? (
                    <TableHead>
                      <span className="sr-only">Actions</span>
                    </TableHead>
                  ) : null}
                </TableRow>
              </TableHeader>
              <TableBody>
                {info.credentials.map((credential) => (
                  <TableRow key={credential.id}>
                    <TableCell className="font-mono text-xs">{credential.key_id}</TableCell>
                    <TableCell>
                      <Lifetime credential={credential} />
                    </TableCell>
                    <TableCell className="type-meta">
                      <time dateTime={credential.created_at}>
                        {formatDateTime(credential.created_at)}
                      </time>
                    </TableCell>
                    <TableCell>
                      <StatusBadge
                        level={credential.disabled ? 'disabled' : 'healthy'}
                        label={credential.disabled ? 'Disabled' : 'Active'}
                      />
                    </TableCell>
                    {permissions.manage_service_accounts ? (
                      <TableCell>
                        <div className="flex justify-end">
                          <Button
                            size="sm"
                            variant="secondary"
                            aria-label={`${credential.disabled ? 'Enable' : 'Disable'} credential ${credential.key_id}`}
                            disabled={toggle.isPending}
                            onClick={() =>
                              toggle.mutate({
                                id: credential.id,
                                enabled: credential.disabled,
                              })
                            }
                          >
                            {credential.disabled ? 'Enable' : 'Disable'}
                          </Button>
                        </div>
                      </TableCell>
                    ) : null}
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </TableShell>
        )}
      </CardContent>
    </Card>
  );
}

/** Whether a credential is permanent, and if not, how long it has left. */
function Lifetime({ credential }: { readonly credential: Credential }) {
  const remaining = secondsRemaining(credential.expires_at, new Date());
  if (remaining === null) return <Badge tone="neutral">Permanent</Badge>;
  return (
    <Badge tone={remaining === 0 ? 'neutral' : 'warn'}>
      {remaining === 0 ? 'Expired' : `Expires in ${formatDuration(remaining)}`}
    </Badge>
  );
}

function Policies({ info }: { readonly info: ServiceAccountInfo | null }) {
  const policies = useQuery({
    queryKey: queryKeys.policies,
    queryFn: ({ signal }) => fetchPolicies(signal),
  });

  const attached = (policies.data ?? []).filter((policy) =>
    (info?.policy_bindings ?? []).includes(policy.id),
  );

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Policies</CardTitle>
        <CardDescription>
          What this account may do. Attach and detach are managed from the policy itself, where the
          full set of accounts using it is visible.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {policies.isPending || info === null ? (
          <Skeleton className="h-16 w-full" />
        ) : attached.length === 0 ? (
          <EmptyState
            title="No policies attached"
            description="This account can authenticate but is authorised for nothing."
          />
        ) : (
          <ul className="space-y-2">
            {attached.map((policy) => (
              <li
                key={policy.id}
                className="rounded-[--radius-control] border border-border px-3 py-2"
              >
                <p className="text-sm font-medium text-ink">{policy.name}</p>
                <ul className="mt-1 flex flex-wrap gap-1.5">
                  {policy.statements
                    .flatMap((statement) => statement.resources)
                    .map((resource) => (
                      <li key={resource}>
                        <Badge tone="neutral" className="font-mono">
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

/** Audit entries recorded for this account. */
function Activity({ name }: { readonly name: string | null }) {
  const events = useQuery({
    queryKey: queryKeys.audit(`principal:${name ?? ''}`),
    queryFn: ({ signal }) => fetchAuditEvents({ principal: name, limit: 25 }, signal),
    enabled: name !== null,
  });

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Activity</CardTitle>
        <CardDescription>
          Audit entries recorded for this principal. This is the security trail, not storage events.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {events.isError ? (
          <ErrorState error={events.error} onRetry={() => void events.refetch()} />
        ) : events.isPending || name === null ? (
          <Skeleton className="h-20 w-full" />
        ) : events.data.events.length === 0 ? (
          <EmptyState
            title="No recorded activity"
            description="No audited operation has been attributed to this account."
          />
        ) : (
          <ul className="divide-y divide-border">
            {events.data.events.map((event) => (
              <li key={event.event_id} className="flex flex-wrap items-baseline gap-x-3 py-2">
                <span className="font-mono text-xs text-ink">{event.operation}</span>
                <span className="min-w-0 flex-1 truncate type-meta">{event.resource}</span>
                <Badge tone={event.result === 'success' ? 'neutral' : 'warn'}>{event.result}</Badge>
                <time dateTime={event.timestamp} className="type-meta-subtle">
                  {formatDateTime(event.timestamp)}
                </time>
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

function Detail({
  label,
  value,
  node,
}: {
  readonly label: string;
  readonly value?: string | null;
  readonly node?: React.ReactNode;
}) {
  return (
    <div className="min-w-0 space-y-0.5">
      <p className="text-xs font-medium text-ink-muted">{label}</p>
      {node ??
        (value === null || value === undefined ? (
          <Skeleton className="h-5 w-24" />
        ) : (
          <p className="break-all type-body">{value}</p>
        ))}
    </div>
  );
}
