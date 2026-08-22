'use client';

import { useMutation, useQueryClient } from '@tanstack/react-query';
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
import { validateBucketName } from '@/features/buckets/bucket-name';
import { queryKeys } from '@/hooks/use-system';
import { createBucket } from '@/lib/api/buckets';
import { ApiError } from '@/lib/api/error';

export function CreateBucketDialog({
  open,
  onOpenChange,
}: {
  readonly open: boolean;
  readonly onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        {/* Mounted only while open, so the form starts empty every time. */}
        <CreateBucketForm onOpenChange={onOpenChange} />
      </DialogContent>
    </Dialog>
  );
}

function CreateBucketForm({ onOpenChange }: { readonly onOpenChange: (open: boolean) => void }) {
  const client = useQueryClient();
  const [name, setName] = React.useState('');
  const [touched, setTouched] = React.useState(false);

  const mutation = useMutation({
    mutationFn: (bucketName: string) => createBucket(bucketName),
    onSuccess: async (bucket) => {
      toast.success(`Bucket ${bucket.name} created`);
      await client.invalidateQueries({ queryKey: queryKeys.buckets });
      onOpenChange(false);
    },
  });

  const localError = touched ? validateBucketName(name) : null;
  const serverError = mutation.error instanceof ApiError ? mutation.error : null;

  return (
    <>
      <DialogHeader>
        <DialogTitle>Create bucket</DialogTitle>
        <DialogDescription>
          Buckets hold objects. The name cannot be changed afterwards.
        </DialogDescription>
      </DialogHeader>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          setTouched(true);
          if (validateBucketName(name)) return;
          mutation.mutate(name);
        }}
      >
        <DialogBody>
          <Field
            label="Bucket name"
            htmlFor="bucket-name"
            hint="Lowercase letters, digits, hyphens, and dots."
            error={localError ?? serverError?.message ?? null}
          >
            <Input
              value={name}
              autoComplete="off"
              spellCheck={false}
              onChange={(event) => setName(event.target.value.trim())}
              onBlur={() => setTouched(true)}
            />
          </Field>
          {serverError ? <ErrorDetails error={serverError} /> : null}
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" disabled={mutation.isPending}>
            {mutation.isPending ? 'Creating…' : 'Create bucket'}
          </Button>
        </DialogFooter>
      </form>
    </>
  );
}
