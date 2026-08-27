'use client';

import { useMutation, useQueryClient } from '@tanstack/react-query';
import * as React from 'react';
import { toast } from 'sonner';

import { ErrorState } from '@/components/error-state';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Field } from '@/components/ui/label';
import { Skeleton } from '@/components/ui/skeleton';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { setBucketQuota } from '@/lib/api/buckets';
import { BYTE_UNITS, type ByteUnit, splitBytes, toBytes } from '@/lib/byte-size';
import { formatBytes, formatCount, formatPercent } from '@/lib/format';
import type { Bucket, BucketQuota } from '@/types/api';

/** How one limit is being edited. */
type LimitDraft = {
  readonly limited: boolean;
  readonly value: string;
  readonly unit: ByteUnit;
};

function byteDraft(quota: BucketQuota): LimitDraft {
  if (quota.bytes.mode === 'unlimited') return { limited: false, value: '', unit: 'GB' };
  const { value, unit } = splitBytes(quota.bytes.bytes);
  return { limited: true, value: String(value), unit };
}

function objectDraft(quota: BucketQuota): { readonly limited: boolean; readonly value: string } {
  return quota.objects.mode === 'unlimited'
    ? { limited: false, value: '' }
    : { limited: true, value: String(quota.objects.objects) };
}

/**
 * Shows and edits a bucket's quota.
 *
 * The backend stores both limits as one value, so the form always submits both.
 * Amounts are whole numbers in a chosen unit, which keeps an untouched limit
 * byte-identical on save instead of drifting through a fractional conversion.
 */
export function QuotaSection({ record }: { readonly record: Bucket | null }) {
  if (!record) {
    return (
      <Card>
        <CardHeader className="flex-col items-start">
          <CardTitle>Quota</CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-24 w-full" />
        </CardContent>
      </Card>
    );
  }
  // Keyed on the bucket so switching buckets resets the draft by remounting
  // rather than by synchronising state in an effect.
  return <QuotaForm key={record.id} record={record} />;
}

function QuotaForm({ record }: { readonly record: Bucket }) {
  const client = useQueryClient();
  const permissions = usePermissions();
  const editable = permissions.manage_buckets;

  const [bytes, setBytes] = React.useState<LimitDraft>(() => byteDraft(record.quota));
  const [objects, setObjects] = React.useState(() => objectDraft(record.quota));
  const [problem, setProblem] = React.useState<string | null>(null);

  const mutation = useMutation({
    mutationFn: (quota: BucketQuota) => setBucketQuota(record.name, quota),
    onSuccess: async () => {
      toast.success('Quota updated');
      await client.invalidateQueries({ queryKey: queryKeys.buckets });
    },
  });

  function submit(event: React.FormEvent) {
    event.preventDefault();
    setProblem(null);
    let byteLimit: BucketQuota['bytes'] = { mode: 'unlimited' };
    if (bytes.limited) {
      const exact = toBytes(bytes.value, bytes.unit);
      if (exact === null) {
        setProblem('Enter the storage limit as a whole number of the selected unit.');
        return;
      }
      byteLimit = { mode: 'limit', bytes: exact };
    }
    let objectLimit: BucketQuota['objects'] = { mode: 'unlimited' };
    if (objects.limited) {
      if (!/^\d+$/.test(objects.value.trim())) {
        setProblem('Enter the object limit as a whole number.');
        return;
      }
      objectLimit = { mode: 'limit', objects: Number(objects.value.trim()) };
    }
    mutation.mutate({ bytes: byteLimit, objects: objectLimit });
  }

  const storedBytes = record.quota.bytes;
  const storedObjects = record.quota.objects;

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Quota</CardTitle>
        <CardDescription>
          Limits are enforced by Record Store when an object version is published, so an upload that
          would exceed a limit is refused rather than stored and cleaned up later.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-5">
        <div className="grid gap-4 sm:grid-cols-2">
          <Usage
            label="Storage"
            used={formatBytes(record.logical_bytes)}
            limit={storedBytes.mode === 'limit' ? formatBytes(storedBytes.bytes) : 'Unlimited'}
            fraction={
              storedBytes.mode === 'limit' && storedBytes.bytes > 0
                ? record.logical_bytes / storedBytes.bytes
                : null
            }
          />
          <Usage
            label="Objects"
            used={formatCount(record.object_count)}
            limit={
              storedObjects.mode === 'limit' ? formatCount(storedObjects.objects) : 'Unlimited'
            }
            fraction={
              storedObjects.mode === 'limit' && storedObjects.objects > 0
                ? record.object_count / storedObjects.objects
                : null
            }
          />
        </div>

        {editable ? (
          <form onSubmit={submit} className="space-y-4 border-t border-border pt-4">
            <fieldset className="space-y-2">
              <legend className="type-label">Storage limit</legend>
              <Choice
                name="storage"
                limited={bytes.limited}
                onChange={(limited) => setBytes({ ...bytes, limited })}
              />
              {bytes.limited ? (
                <div className="flex items-end gap-2">
                  <Field label="Amount" htmlFor="quota-bytes">
                    <Input
                      id="quota-bytes"
                      inputMode="numeric"
                      value={bytes.value}
                      onChange={(event) => setBytes({ ...bytes, value: event.target.value })}
                    />
                  </Field>
                  <label className="flex flex-col gap-1 type-meta">
                    Unit
                    <select
                      aria-label="Storage limit unit"
                      value={bytes.unit}
                      onChange={(event) =>
                        setBytes({ ...bytes, unit: event.target.value as ByteUnit })
                      }
                      className="h-9 rounded-control border border-border-strong bg-surface px-2 type-body"
                    >
                      {BYTE_UNITS.map((unit) => (
                        <option key={unit} value={unit}>
                          {unit}
                        </option>
                      ))}
                    </select>
                  </label>
                </div>
              ) : null}
            </fieldset>

            <fieldset className="space-y-2">
              <legend className="type-label">Object limit</legend>
              <Choice
                name="objects"
                limited={objects.limited}
                onChange={(limited) => setObjects({ ...objects, limited })}
              />
              {objects.limited ? (
                <Field label="Maximum objects" htmlFor="quota-objects">
                  <Input
                    id="quota-objects"
                    inputMode="numeric"
                    value={objects.value}
                    onChange={(event) => setObjects({ ...objects, value: event.target.value })}
                  />
                </Field>
              ) : null}
            </fieldset>

            {problem ? (
              <p role="alert" className="text-xs text-danger">
                {problem}
              </p>
            ) : null}
            {mutation.error ? <ErrorState error={mutation.error} /> : null}
            <Button type="submit" variant="primary" size="sm" disabled={mutation.isPending}>
              {mutation.isPending ? 'Saving…' : 'Save quota'}
            </Button>
          </form>
        ) : (
          <p className="type-meta">Your role does not permit changing quotas.</p>
        )}
      </CardContent>
    </Card>
  );
}

function Choice({
  name,
  limited,
  onChange,
}: {
  readonly name: string;
  readonly limited: boolean;
  readonly onChange: (limited: boolean) => void;
}) {
  return (
    <div className="flex gap-4">
      {[
        { label: 'Unlimited', value: false },
        { label: 'Set a limit', value: true },
      ].map((option) => (
        <label key={option.label} className="flex items-center gap-1.5 type-body">
          <input
            type="radio"
            name={`${name}-quota-mode`}
            checked={limited === option.value}
            onChange={() => onChange(option.value)}
          />
          {option.label}
        </label>
      ))}
    </div>
  );
}

function Usage({
  label,
  used,
  limit,
  fraction,
}: {
  readonly label: string;
  readonly used: string;
  readonly limit: string;
  readonly fraction: number | null;
}) {
  const percent = fraction === null ? null : Math.min(100, Math.round(fraction * 100));
  return (
    <div className="space-y-1.5">
      <p className="text-xs font-medium text-ink-muted">{label}</p>
      <p className="text-sm tabular-nums text-ink">
        {used} <span className="text-ink-subtle">of {limit}</span>
      </p>
      {percent === null ? (
        // No limit means there is no fraction to draw; a full or empty bar
        // would both imply a threshold that does not exist.
        <p className="type-meta-subtle">No limit configured</p>
      ) : (
        <div
          role="progressbar"
          aria-label={`${label} quota usage`}
          aria-valuenow={percent}
          aria-valuemin={0}
          aria-valuemax={100}
          className="h-1.5 w-full overflow-hidden rounded-full bg-surface-muted"
        >
          <div
            className={percent >= 90 ? 'h-full bg-danger' : 'h-full bg-accent'}
            style={{ width: `${percent}%` }}
          />
        </div>
      )}
      {percent === null ? null : (
        <p className="type-meta-subtle">{formatPercent((fraction ?? 0) * 100)} used</p>
      )}
    </div>
  );
}
