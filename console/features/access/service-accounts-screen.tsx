'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { createColumnHelper } from '@tanstack/react-table';
import { KeyRound, MoreHorizontal, Plus } from 'lucide-react';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTable, type DataTableFeatures } from '@/components/data-table';
import { EmptyState } from '@/components/empty-state';
import { ErrorDetails, ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
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
import { CredentialDialog } from '@/features/access/credential-dialog';
import { queryKeys } from '@/hooks/use-system';
import {
  createServiceAccount,
  deleteServiceAccount,
  fetchServiceAccounts,
  rotateCredential,
  setServiceAccountEnabled,
} from '@/lib/api/access';
import { ApiError } from '@/lib/api/error';
import { formatDate, formatDateTime } from '@/lib/format';
import type { IssuedCredential, ServiceAccountInfo } from '@/types/api';

const column = createColumnHelper<DataTableFeatures, ServiceAccountInfo>();

export function ServiceAccountsScreen() {
  const client = useQueryClient();
  const [creating, setCreating] = React.useState(false);
  const [issued, setIssued] = React.useState<IssuedCredential | null>(null);
  const [issuedKind, setIssuedKind] = React.useState<'created' | 'rotated'>('created');
  const [pendingDelete, setPendingDelete] = React.useState<ServiceAccountInfo | null>(null);

  const accounts = useQuery({
    queryKey: queryKeys.serviceAccounts,
    queryFn: ({ signal }) => fetchServiceAccounts(signal),
  });

  const invalidate = () => client.invalidateQueries({ queryKey: queryKeys.serviceAccounts });

  const rotation = useMutation({
    mutationFn: (id: string) => rotateCredential(id),
    onSuccess: async (result) => {
      setIssuedKind('rotated');
      setIssued(result);
      await invalidate();
    },
    onError: (error) => toast.error(message(error, 'Credential rotation failed')),
  });

  const status = useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      setServiceAccountEnabled(id, enabled),
    onSuccess: async (_result, variables) => {
      toast.success(variables.enabled ? 'Account enabled' : 'Account disabled');
      await invalidate();
    },
    onError: (error) => toast.error(message(error, 'Could not change account status')),
  });

  const removal = useMutation({
    mutationFn: (id: string) => deleteServiceAccount(id),
    onSuccess: async () => {
      toast.success('Service account deleted');
      setPendingDelete(null);
      await invalidate();
    },
  });

  const columns = React.useMemo(
    () =>
      column.columns([
        column.accessor((row) => row.account.name, {
          id: 'name',
          header: 'Name',
          cell: ({ row }) => (
            <div className="space-y-0.5">
              <p className="font-medium text-ink">{row.original.account.name}</p>
              {row.original.account.description ? (
                <p className="text-xs text-ink-muted">{row.original.account.description}</p>
              ) : null}
            </div>
          ),
        }),
        column.accessor((row) => (row.account.disabled ? 'disabled' : 'active'), {
          id: 'status',
          header: 'Status',
          cell: ({ row }) =>
            row.original.account.disabled ? (
              <StatusBadge level="disabled" label="Disabled" />
            ) : (
              <StatusBadge level="healthy" label="Active" />
            ),
        }),
        column.accessor((row) => row.credentials.length, {
          id: 'credentials',
          header: 'Credentials',
          cell: ({ row }) => {
            const active = row.original.credentials.filter((item) => !item.disabled).length;
            return (
              <span className="text-xs text-ink-muted">
                {active} active
                {row.original.credentials.length > active
                  ? ` · ${row.original.credentials.length - active} disabled`
                  : ''}
              </span>
            );
          },
        }),
        column.accessor((row) => row.policy_bindings.length, {
          id: 'policies',
          header: 'Policies',
          cell: ({ getValue }) => (
            <span className="tabular-nums text-xs text-ink-muted">{getValue()}</span>
          ),
        }),
        column.accessor((row) => row.account.created_at, {
          id: 'created',
          header: 'Created',
          cell: ({ getValue }) => (
            <time
              dateTime={getValue()}
              title={formatDateTime(getValue())}
              className="text-xs text-ink-muted"
            >
              {formatDate(getValue())}
            </time>
          ),
        }),
        column.display({
          id: 'actions',
          header: () => <span className="sr-only">Actions</span>,
          cell: ({ row }) => (
            <div className="flex justify-end">
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label={`Actions for ${row.original.account.name}`}
                  >
                    <MoreHorizontal aria-hidden />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent>
                  <DropdownMenuItem
                    onSelect={() => rotation.mutate(row.original.account.id)}
                    disabled={rotation.isPending}
                  >
                    <KeyRound aria-hidden /> Rotate credential
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() =>
                      status.mutate({
                        id: row.original.account.id,
                        enabled: row.original.account.disabled,
                      })
                    }
                  >
                    {row.original.account.disabled ? 'Enable account' : 'Disable account'}
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem destructive onSelect={() => setPendingDelete(row.original)}>
                    Delete account
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          ),
        }),
      ]),
    [rotation, status],
  );

  return (
    <>
      <PageHeader
        title="Service accounts"
        description="Workload identities that applications use to reach the S3 API."
        actions={
          <Button variant="primary" onClick={() => setCreating(true)}>
            <Plus aria-hidden />
            Create account
          </Button>
        }
      />

      <Card>
        {accounts.isError ? (
          <ErrorState error={accounts.error} onRetry={() => void accounts.refetch()} />
        ) : (
          <DataTable
            data={accounts.data ?? []}
            columns={columns}
            rowId={(row) => row.account.id}
            loading={accounts.isPending}
            initialSorting={[{ id: 'name', desc: false }]}
            empty={
              <EmptyState
                title="No service accounts"
                description="Create a service account to give an application S3 credentials."
                action={
                  <Button variant="primary" onClick={() => setCreating(true)}>
                    <Plus aria-hidden />
                    Create account
                  </Button>
                }
              />
            }
          />
        )}
      </Card>

      <CreateAccountDialog
        open={creating}
        onOpenChange={setCreating}
        onCreated={(result) => {
          setIssuedKind('created');
          setIssued(result);
          setCreating(false);
          void invalidate();
        }}
      />

      <CredentialDialog
        issued={issued}
        onClose={() => setIssued(null)}
        title={issuedKind === 'created' ? 'Service account created' : 'New credential issued'}
        description={
          issuedKind === 'created'
            ? 'Store these credentials now. The secret cannot be retrieved later.'
            : 'The previous credential is still active. Update your application, then disable the old credential.'
        }
      />

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPendingDelete(null);
            removal.reset();
          }
        }}
        strength="type-to-confirm"
        expectedText={pendingDelete?.account.name}
        title={`Delete ${pendingDelete?.account.name ?? ''}?`}
        description="The account and all of its credentials are removed."
        consequence="Applications using these credentials will immediately lose access."
        confirmLabel="Delete account"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => {
          if (pendingDelete) removal.mutate(pendingDelete.account.id);
        }}
      />
    </>
  );
}

function CreateAccountDialog({
  open,
  onOpenChange,
  onCreated,
}: {
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
  readonly onCreated: (issued: IssuedCredential) => void;
}) {
  const [name, setName] = React.useState('');
  const [description, setDescription] = React.useState('');

  const mutation = useMutation({
    mutationFn: () => createServiceAccount({ name, description }),
    onSuccess: (result) => {
      setName('');
      setDescription('');
      onCreated(result);
    },
  });

  React.useEffect(() => {
    if (!open) mutation.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const error = mutation.error instanceof ApiError ? mutation.error : null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create service account</DialogTitle>
          <DialogDescription>
            A credential is issued immediately and its secret shown once.
          </DialogDescription>
        </DialogHeader>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (name.trim().length > 0) mutation.mutate();
          }}
        >
          <DialogBody>
            <Field label="Name" htmlFor="account-name" error={error?.message ?? null}>
              <Input
                value={name}
                autoComplete="off"
                required
                onChange={(event) => setName(event.target.value)}
              />
            </Field>
            <Field
              label="Description"
              htmlFor="account-description"
              hint="Optional. Describe what uses this account."
            >
              <Input
                value={description}
                autoComplete="off"
                onChange={(event) => setDescription(event.target.value)}
              />
            </Field>
            {error ? <ErrorDetails error={error} /> : null}
          </DialogBody>
          <DialogFooter>
            <Button variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending || name.trim().length === 0}
            >
              {mutation.isPending ? 'Creating…' : 'Create account'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function message(error: unknown, fallback: string): string {
  return error instanceof ApiError ? error.message : fallback;
}
