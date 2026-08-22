import type { Metadata } from 'next';

import { ObjectDetails } from '@/features/objects/object-details';

type Params = { params: Promise<{ bucket: string; key: string[] }> };

/**
 * Object keys arrive as path segments.
 *
 * Each segment is decoded individually and rejoined with `/`, which restores the
 * original key even when it contains characters that look like path structure.
 */
function decodeKey(segments: readonly string[]): string {
  return segments.map((segment) => decodeURIComponent(segment)).join('/');
}

export async function generateMetadata({ params }: Params): Promise<Metadata> {
  const { key } = await params;
  const decoded = decodeKey(key);
  return { title: decoded.split('/').pop() ?? decoded };
}

export default async function Page({ params }: Params) {
  const { bucket, key } = await params;
  return <ObjectDetails bucket={decodeURIComponent(bucket)} objectKey={decodeKey(key)} />;
}
