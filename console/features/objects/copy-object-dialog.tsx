'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import * as React from 'react';
import { toast } from 'sonner';

import { ErrorDetails } from '@/components/error-state';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Field } from '@/components/ui/label';
import { queryKeys } from '@/hooks/use-system';
import { fetchBuckets } from '@/lib/api/buckets';
import { copyObject } from '@/lib/api/objects';
import { ApiError } from '@/lib/api/error';
import { keyBasename } from '@/lib/format';

/**
 * Copies one object to a chosen bucket and key.
 *
 * OES streams the bytes internally, so the browser never carries them. The
 * dialog therefore stays responsive for a multi-gigabyte object, and the only
 * thing being waited on is the server's own copy.
 */
export function CopyObjectDialog({
  bucket,
  objectKey,
  open,
  onOpenChange,
}: {
  readonly bucket: string;
  readonly objectKey: string | null;
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        {objectKey === null ? null : (
          <CopyForm
            bucket={bucket}
            objectKey={objectKey}
            onDone={() => onOpenChange(false)}
            onCancel={() => onOpenChange(false)}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}

function CopyForm({
  bucket,
  objectKey,
  onDone,
  onCancel,
}: {
  readonly bucket: string;
  readonly objectKey: string;
  readonly onDone: () => void;
  readonly onCancel: () => void;
}) {
  const client = useQueryClient();
  const buckets = useQuery({
    queryKey: queryKeys.buckets,
    queryFn: ({ signal }) => fetchBuckets(signal),
  });
  const [destinationBucket, setDestinationBucket] = React.useState(bucket);
  const [destinationKey, setDestinationKey] = React.useState(() => suggestCopyName(objectKey));

  const mutation = useMutation({
    mutationFn: () =>
      copyObject({
        sourceBucket: bucket,
        sourceKey: objectKey,
        destinationBucket,
        destinationKey,
      }),
    onSuccess: async (created) => {
      toast.success(`Copied to ${destinationBucket}/${created.key}`);
      await client.invalidateQueries({ queryKey: ['buckets', destinationBucket, 'objects'] });
      await client.invalidateQueries({ queryKey: queryKeys.buckets });
      onDone();
    },
  });

  const sameLocation = destinationBucket === bucket && destinationKey === objectKey;
  const valid = destinationKey.trim().length > 0 && !sameLocation;

  return (
    <>
      <DialogHeader>
        <DialogTitle>Copy object</DialogTitle>
        <DialogDescription>
          OES copies the bytes itself. Nothing is downloaded to this browser, and the source is left
          unchanged.
        </DialogDescription>
      </DialogHeader>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (valid) mutation.mutate();
        }}
      >
        <DialogBody className="space-y-4">
          <div className="space-y-1">
            <p className="type-label">Source</p>
            <p className="break-all font-mono type-meta">
              {bucket}/{objectKey}
            </p>
          </div>
          <Field label="Destination bucket" htmlFor="copy-bucket">
            <select
              id="copy-bucket"
              value={destinationBucket}
              onChange={(event) => setDestinationBucket(event.target.value)}
              className="h-9 w-full rounded-control border border-border-strong bg-surface px-2 type-body"
            >
              {(buckets.data ?? [{ id: bucket, name: bucket }]).map((entry) => (
                <option key={entry.id} value={entry.name}>
                  {entry.name}
                </option>
              ))}
            </select>
          </Field>
          <Field
            label="Destination key"
            htmlFor="copy-key"
            hint="A key containing slashes places the copy under those prefixes."
          >
            <Input
              id="copy-key"
              value={destinationKey}
              onChange={(event) => setDestinationKey(event.target.value)}
            />
          </Field>
          {sameLocation ? (
            <p role="alert" className="text-xs text-danger">
              Choose a different bucket or key: an object cannot be copied over itself.
            </p>
          ) : null}
          {mutation.error instanceof ApiError ? <ErrorDetails error={mutation.error} /> : null}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={!valid || mutation.isPending}>
            {mutation.isPending ? 'Copying…' : 'Copy object'}
          </Button>
        </DialogFooter>
      </form>
    </>
  );
}

/**
 * Suggests a destination key that will not collide with the source.
 *
 * The suffix goes before the extension so the copy keeps its file type, which
 * is what determines how anything downstream treats it.
 */
export function suggestCopyName(key: string): string {
  const basename = keyBasename(key);
  const prefix = key.slice(0, key.length - basename.length);
  const dot = basename.lastIndexOf('.');
  if (dot <= 0) return `${prefix}${basename}-copy`;
  return `${prefix}${basename.slice(0, dot)}-copy${basename.slice(dot)}`;
}
