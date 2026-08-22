import { NodeDetails } from '@/features/cluster/node-details';

export default async function ClusterNodePage({
  params,
}: {
  readonly params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return <NodeDetails nodeId={id} />;
}
