'use client';

import { useQuery } from '@tanstack/react-query';
import { ChevronLeft, ChevronRight, Search } from 'lucide-react';
import { usePathname, useRouter, useSearchParams } from 'next/navigation';
import * as React from 'react';

import { CopyButton } from '@/components/copy-button';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { TableSkeleton } from '@/components/ui/skeleton';
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
import { fetchAuditEvents } from '@/lib/api/observability';
import { formatDateTime } from '@/lib/format';
import { mergeSearch, readEnum, readOptionalString, readTimestamp } from '@/lib/search-params';
import type { AuditEvent, AuditResult } from '@/types/api';

const RESULTS = ['success', 'denied', 'failure'] as const;

/**
 * The security audit trail: who asked for what, and what Record Store decided.
 *
 * The history is unbounded, so every query is server side with a cursor. The
 * console never holds more than one page.
 */
export function AuditScreen() {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();
  const [selected, setSelected] = React.useState<AuditEvent | null>(null);

  const filters = {
    principal: readOptionalString(params, 'principal'),
    operation: readOptionalString(params, 'operation'),
    resource: readOptionalString(params, 'resource'),
    result: readEnum(params, 'result', RESULTS),
    sourceIp: readOptionalString(params, 'source_ip'),
    requestId: readOptionalString(params, 'request_id'),
    since: readTimestamp(params, 'since'),
    until: readTimestamp(params, 'until'),
    afterTime: readOptionalString(params, 'after_time'),
    afterId: readOptionalString(params, 'after_id'),
  };

  const key = JSON.stringify(filters);
  const audit = useQuery({
    queryKey: queryKeys.audit(key),
    queryFn: ({ signal }) => fetchAuditEvents({ ...filters, limit: 50 }, signal),
  });

  function update(updates: Record<string, string | null>) {
    router.push(`${pathname}${mergeSearch(params, updates)}`);
  }

  return (
    <>
      <PageHeader
        title="Audit log"
        description="Authenticated management and storage operations, with the decision Record Store made."
      />

      <Card>
        <form
          className="grid gap-3 border-b border-border p-3 sm:grid-cols-2 lg:grid-cols-4"
          onSubmit={(event) => {
            event.preventDefault();
            const form = new FormData(event.currentTarget);
            update({
              principal: (form.get('principal') as string) || null,
              operation: (form.get('operation') as string) || null,
              resource: (form.get('resource') as string) || null,
              result: (form.get('result') as string) || null,
              source_ip: (form.get('source_ip') as string) || null,
              request_id: (form.get('request_id') as string) || null,
              since: (form.get('since') as string) || null,
              // A new filter invalidates the old cursor.
              after_time: null,
              after_id: null,
            });
          }}
        >
          <LabelledInput name="principal" label="Principal" defaultValue={filters.principal} />
          <LabelledInput name="operation" label="Operation" defaultValue={filters.operation} />
          <LabelledInput name="resource" label="Resource prefix" defaultValue={filters.resource} />
          <LabelledInput name="source_ip" label="Source IP" defaultValue={filters.sourceIp} />
          <LabelledInput name="request_id" label="Request ID" defaultValue={filters.requestId} />
          <div className="space-y-1.5">
            <label htmlFor="audit-result" className="type-label">
              Result
            </label>
            <select
              id="audit-result"
              name="result"
              defaultValue={filters.result ?? ''}
              className="h-9 w-full rounded-control border border-border-strong bg-surface px-2 type-body"
            >
              <option value="">Any</option>
              {RESULTS.map((result) => (
                <option key={result} value={result}>
                  {result}
                </option>
              ))}
            </select>
          </div>
          <div className="flex items-end gap-2">
            <Button type="submit" variant="primary" className="flex-1">
              Apply
            </Button>
            <Button
              variant="ghost"
              onClick={() =>
                update({
                  principal: null,
                  operation: null,
                  resource: null,
                  result: null,
                  since: null,
                  until: null,
                  after_time: null,
                  after_id: null,
                })
              }
            >
              Clear
            </Button>
          </div>
        </form>

        {audit.isError ? (
          <ErrorState error={audit.error} onRetry={() => void audit.refetch()} />
        ) : audit.isPending ? (
          <TableSkeleton columns={5} />
        ) : audit.data.events.length === 0 ? (
          <EmptyState
            title="No audit events"
            description="No events match these filters. Clear them to see recent activity."
          />
        ) : (
          <TableShell>
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead>Time</TableHead>
                  <TableHead>Principal</TableHead>
                  <TableHead>Operation</TableHead>
                  <TableHead>Resource</TableHead>
                  <TableHead>Result</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {audit.data.events.map((event) => (
                  <TableRow
                    key={event.event_id}
                    className="cursor-pointer"
                    onClick={() => setSelected(event)}
                  >
                    <TableCell className="whitespace-nowrap type-meta">
                      <time dateTime={event.timestamp}>{formatDateTime(event.timestamp)}</time>
                    </TableCell>
                    <TableCell className="text-xs">{event.principal}</TableCell>
                    <TableCell className="font-mono text-xs">{event.operation}</TableCell>
                    <TableCell className="max-w-xs truncate text-xs" title={event.resource}>
                      {event.resource}
                    </TableCell>
                    <TableCell>
                      <ResultBadge result={event.result} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </TableShell>
        )}
      </Card>

      <div className="flex items-center justify-end gap-2">
        <Button
          size="sm"
          variant="secondary"
          disabled={filters.afterTime === null}
          onClick={() => update({ after_time: null, after_id: null })}
        >
          <ChevronLeft aria-hidden />
          First page
        </Button>
        <Button
          size="sm"
          variant="secondary"
          disabled={!audit.data?.next_time}
          onClick={() =>
            update({
              after_time: audit.data?.next_time ?? null,
              after_id: audit.data?.next_id ?? null,
            })
          }
        >
          Next page
          <ChevronRight aria-hidden />
        </Button>
      </div>

      <AuditDetail event={selected} onClose={() => setSelected(null)} />
    </>
  );
}

function LabelledInput({
  name,
  label,
  defaultValue,
}: {
  readonly name: string;
  readonly label: string;
  readonly defaultValue: string | null;
}) {
  return (
    <div className="space-y-1.5">
      <label htmlFor={`audit-${name}`} className="type-label">
        {label}
      </label>
      <Input
        id={`audit-${name}`}
        name={name}
        defaultValue={defaultValue ?? ''}
        autoComplete="off"
      />
    </div>
  );
}

function ResultBadge({ result }: { readonly result: AuditResult }) {
  if (result === 'success') return <StatusBadge level="healthy" label="Success" />;
  if (result === 'denied') return <StatusBadge level="warning" label="Denied" />;
  return <StatusBadge level="critical" label="Failure" />;
}

/**
 * One audit event in full.
 *
 * Credential identifiers are shown because they identify which key was used;
 * secrets, tokens, and authorisation headers are never part of an audit record.
 */
function AuditDetail({
  event,
  onClose,
}: {
  readonly event: AuditEvent | null;
  readonly onClose: () => void;
}) {
  if (!event) return null;
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Audit event</DialogTitle>
        </DialogHeader>
        <DialogBody>
          <dl className="space-y-2 text-xs">
            <Row label="Event ID" value={event.event_id} mono copy />
            <Row
              label="Request ID"
              value={event.request_id ?? '—'}
              mono
              copy={event.request_id !== null}
              filter={
                event.request_id === null
                  ? undefined
                  : { label: 'Show every event for this request', request_id: event.request_id }
              }
            />
            <Row label="Time" value={formatDateTime(event.timestamp)} />
            <Row label="Principal" value={event.principal} />
            <Row label="Credential" value={event.credential_id ?? '—'} mono />
            <Row
              label="Source IP"
              value={event.source_ip ?? '—'}
              mono
              filter={
                event.source_ip === null
                  ? undefined
                  : { label: 'Show every event from this address', source_ip: event.source_ip }
              }
            />
            <Row label="Operation" value={event.operation} mono />
            <Row label="Resource" value={event.resource} mono />
            <Row label="Result" value={event.result} />
          </dl>
          {Object.keys(event.metadata).length > 0 ? (
            <div className="space-y-1 border-t border-border pt-3">
              <p className="type-label">Metadata</p>
              <dl className="space-y-1">
                {Object.entries(event.metadata).map(([key, value]) => (
                  <div key={key} className="flex gap-2">
                    <dt className="font-mono type-meta">{key}</dt>
                    <dd className="font-mono text-xs text-ink">{value}</dd>
                  </div>
                ))}
              </dl>
            </div>
          ) : null}
        </DialogBody>
      </DialogContent>
    </Dialog>
  );
}

function Row({
  label,
  value,
  mono = false,
  copy = false,
  filter,
}: {
  readonly label: string;
  readonly value: string;
  readonly mono?: boolean;
  readonly copy?: boolean;
  /**
   * Turns the value into a filter for the log behind this drawer.
   *
   * Tracing a support request means finding every event that shares its request
   * id or address, so the identifier is the control rather than something to
   * select and retype.
   */
  readonly filter?: { readonly label: string; readonly [key: string]: string };
}) {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();

  return (
    <div className="flex flex-wrap items-baseline justify-between gap-2">
      <dt className="text-ink-muted">{label}</dt>
      <dd className="flex min-w-0 items-center gap-1.5">
        <span className={mono ? 'break-all font-mono text-ink' : 'text-ink'}>{value}</span>
        {copy ? (
          <CopyButton value={value} label={`Copy ${label}`} size="icon" variant="ghost" />
        ) : null}
        {filter ? (
          <button
            type="button"
            aria-label={filter.label}
            title={filter.label}
            className="shrink-0 text-accent hover:underline"
            onClick={() => {
              const { label: _ignored, ...updates } = filter;
              router.push(
                `${pathname}${mergeSearch(params, { ...updates, after_time: null, after_id: null })}`,
              );
            }}
          >
            <Search aria-hidden className="size-3.5" />
          </button>
        ) : null}
      </dd>
    </div>
  );
}
