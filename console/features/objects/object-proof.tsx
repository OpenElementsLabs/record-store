'use client';

import { useMutation } from '@tanstack/react-query';

import { ErrorState } from '@/components/error-state';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { encodeObjectKey, request } from '@/lib/api/client';
import { objectContentUrl } from '@/lib/api/objects';
import type { ObjectSummary } from '@/types/api';

/** Generation is not verification: download the signed evidence without awarding a verdict. */
export function ObjectProof({
  bucket,
  record,
}: {
  readonly bucket: string;
  readonly record: ObjectSummary;
}) {
  const proof = useMutation({
    mutationFn: () =>
      request<unknown>(
        `/v1/buckets/${encodeURIComponent(bucket)}/proof/${encodeObjectKey(record.key)}`,
        {
          query: { version_id: record.version_id },
        },
      ),
    onSuccess: (bundle) => {
      const url = URL.createObjectURL(
        new Blob([JSON.stringify(bundle, null, 2)], { type: 'application/json' }),
      );
      const link = document.createElement('a');
      link.href = url;
      link.download = 'proof.json';
      document.body.append(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1_000);
    },
  });
  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Offline proof</CardTitle>
        <CardDescription>
          A signed statement about this exact version. Downloading it does not verify the file or
          establish trust in the signer.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="break-all type-meta">Version: {record.version_id}</p>
        <div className="flex flex-wrap gap-2">
          <Button variant="secondary" disabled={proof.isPending} onClick={() => proof.mutate()}>
            {proof.isPending ? 'Preparing proof…' : 'Download proof bundle'}
          </Button>
          <Button variant="secondary" asChild>
            <a href={objectContentUrl(bucket, record.key, record.version_id)} download>
              Download matching version
            </a>
          </Button>
        </div>
        {proof.error ? <ErrorState error={proof.error} /> : null}
        <p className="type-meta">Save the matching file as object.bin and verify locally:</p>
        <pre className="overflow-x-auto rounded-control bg-surface-muted p-3 text-xs">
          record-store verify proof proof.json --object object.bin
        </pre>
        <p className="type-meta">
          The verifier checks file integrity and signature validity separately. Establish signer
          identity using a public key obtained through a trusted channel and the verifier’s
          --public-key option. Read every check: unavailable history or external anchoring is not
          verified history.
        </p>
      </CardContent>
    </Card>
  );
}
