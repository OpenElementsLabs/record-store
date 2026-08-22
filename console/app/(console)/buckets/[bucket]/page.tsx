import type { Metadata } from 'next';

import { BucketDetail } from '@/features/buckets/bucket-detail';

type Params = { params: Promise<{ bucket: string }> };

export async function generateMetadata({ params }: Params): Promise<Metadata> {
  const { bucket } = await params;
  return { title: decodeURIComponent(bucket) };
}

export default async function Page({ params }: Params) {
  const { bucket } = await params;
  return <BucketDetail bucket={decodeURIComponent(bucket)} />;
}
