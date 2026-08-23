'use client';

import { useQuery } from '@tanstack/react-query';
import { RefreshCw } from 'lucide-react';

import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableShell,
} from '@/components/ui/table';
import { queryKeys } from '@/hooks/use-system';
import { fetchClusterStatus } from '@/lib/api/cluster';
import { formatCount, shortenIdentifier } from '@/lib/format';
import type { MetadataQuorum } from '@/types/cluster';

/**
 * Metadata consensus, in operational terms.
 *
 * Administrators need to know whether metadata can be written, whether losing
 * one more member would stop that, and which member is behind. They do not need
 * a Raft debugger, so the log positions are present but secondary to the plain
 * statement of what works.
 */
export function ConsensusScreen() {
  const status = useQuery({
    queryKey: queryKeys.clusterStatus,
    queryFn: ({ signal }) => fetchClusterStatus(signal),
    refetchInterval: 15_000,
  });

  const quorum = status.data?.metadata;

  return (
    <>
      <PageHeader
        title="Consensus"
        description="Metadata is agreed between members before it is committed. This is what that agreement looks like right now."
        actions={
          <Button
            size="sm"
            aria-label="Refresh consensus"
            disabled={status.isFetching}
            onClick={() => void status.refetch()}
          >
            <RefreshCw aria-hidden className={status.isFetching ? 'animate-spin' : ''} />
            <span aria-hidden>{status.isFetching ? 'Reading…' : 'Refresh'}</span>
          </Button>
        }
      />

      {status.isError ? (
        <Card>
          <ErrorState error={status.error} onRetry={() => void status.refetch()} />
        </Card>
      ) : (
        <div className="space-y-4">
          <Card>
            <CardContent className="space-y-3 py-4">
              {quorum ? <Summary quorum={quorum} /> : <Skeleton className="h-16 w-full" />}
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="flex-col items-start">
              <CardTitle>Members</CardTitle>
              <CardDescription>
                Voting members decide whether a write is committed. A member that is present but not
                voting cannot help reach agreement.
              </CardDescription>
            </CardHeader>
            <CardContent>
              {quorum ? (
                <TableShell>
                  <Table>
                    <TableHeader>
                      <TableRow className="hover:bg-transparent">
                        <TableHead>Member</TableHead>
                        <TableHead>Address</TableHead>
                        <TableHead>Role</TableHead>
                        <TableHead>Reachable</TableHead>
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {quorum.members.map((member) => (
                        <TableRow key={member.member_id}>
                          <TableCell className="tabular-nums">
                            {member.member_id}
                            {member.member_id === quorum.member_id ? (
                              <span className="ml-2 text-xs text-ink-subtle">this node</span>
                            ) : null}
                          </TableCell>
                          <TableCell className="font-mono text-xs">{member.address}</TableCell>
                          <TableCell>
                            <Badge tone={member.voter ? 'accent' : 'neutral'}>
                              {member.voter ? 'Voter' : 'Non-voting'}
                            </Badge>
                          </TableCell>
                          <TableCell>
                            <StatusBadge
                              level={member.reachable ? 'healthy' : 'critical'}
                              label={member.reachable ? 'Reachable' : 'Unreachable'}
                            />
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                </TableShell>
              ) : (
                <Skeleton className="h-24 w-full" />
              )}
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="flex-col items-start">
              <CardTitle>Replication position</CardTitle>
              <CardDescription>
                How far this node has applied the agreed history. A gap between the last agreed
                entry and the applied one means this node is still catching up.
              </CardDescription>
            </CardHeader>
            <CardContent className="grid gap-x-8 gap-y-3 sm:grid-cols-3">
              <Position label="Agreed up to" value={quorum?.last_log_index ?? null} />
              <Position label="Applied here" value={quorum?.applied_index ?? null} />
              <Position label="Snapshot at" value={quorum?.snapshot_index ?? null} />
            </CardContent>
          </Card>
        </div>
      )}
    </>
  );
}

function Summary({ quorum }: { readonly quorum: MetadataQuorum }) {
  const { status } = quorum;
  // The three questions an operator actually has, answered in words before any
  // number is shown.
  const writes = status.writable
    ? 'Metadata changes are being accepted.'
    : 'Metadata changes are refused: there is no writable majority.';
  const tolerance = status.fault_tolerant
    ? 'The group would survive losing one more member.'
    : 'Losing one more member would stop metadata changes.';

  return (
    <>
      <div className="flex flex-wrap items-center gap-6">
        <div className="space-y-1">
          <p className="text-xs font-medium text-ink-muted">Consensus</p>
          <StatusBadge
            level={status.health}
            label={status.writable ? 'Healthy' : status.readable ? 'Read-only' : 'No quorum'}
          />
        </div>
        <div className="space-y-1">
          <p className="text-xs font-medium text-ink-muted">Members agreeing</p>
          <p className="text-sm tabular-nums text-ink">
            {formatCount(status.healthy_members)} of {formatCount(status.members)}, needs{' '}
            {formatCount(status.quorum)}
          </p>
        </div>
        <div className="space-y-1">
          <p className="text-xs font-medium text-ink-muted">Leader</p>
          <p className="font-mono text-xs text-ink">
            {status.leader === null ? 'none elected' : shortenIdentifier(status.leader, 8)}
          </p>
        </div>
        <div className="space-y-1">
          <p className="text-xs font-medium text-ink-muted">This node</p>
          <p className="text-sm text-ink">{capitalise(quorum.role)}</p>
        </div>
      </div>
      <p className="text-sm text-ink-muted">{writes}</p>
      <p className={status.fault_tolerant ? 'text-sm text-ink-muted' : 'text-sm text-warn'}>
        {tolerance}
      </p>
      {status.notes.length > 0 ? (
        <ul className="space-y-1 border-t border-border pt-3">
          {status.notes.map((note) => (
            <li key={note} className="text-xs text-ink-muted">
              {note}
            </li>
          ))}
        </ul>
      ) : null}
    </>
  );
}

function Position({ label, value }: { readonly label: string; readonly value: number | null }) {
  return (
    <div className="space-y-0.5">
      <p className="text-xs font-medium text-ink-muted">{label}</p>
      {value === null ? (
        // A null position is genuinely "nothing yet", not zero.
        <p className="text-sm text-ink-subtle">not yet established</p>
      ) : (
        <p className="text-sm tabular-nums text-ink">{formatCount(value)}</p>
      )}
    </div>
  );
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
