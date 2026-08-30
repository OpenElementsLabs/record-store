import { MANAGEMENT_TOKEN, expect, test } from './fixtures';

type NodeStatus = { node_id: string; state: string };

test.describe('real cluster deployment', () => {
  test('shows the healthy cluster overview and all three storage nodes', async ({ signedIn }) => {
    await expect(signedIn.getByRole('link', { name: 'Cluster overview' })).toBeVisible();
    await signedIn.goto('/cluster');
    await expect(signedIn.getByRole('heading', { name: 'Cluster' })).toBeVisible();
    await expect(signedIn.getByText('3 healthy')).toBeVisible();
    await expect(signedIn.getByText('3 of 3 members')).toBeVisible();
    await expect(signedIn.getByText('Accepted')).toBeVisible();

    await signedIn.getByRole('link', { name: 'View nodes' }).click();
    await expect(signedIn.getByRole('heading', { name: 'Nodes' })).toBeVisible();
    await expect(signedIn.getByRole('row')).toHaveCount(4);
    await expect(signedIn.getByText('Healthy', { exact: true })).toHaveCount(3);
  });

  test('inspects a node and performs maintenance then resume through the console', async ({
    signedIn,
  }) => {
    const managementUrl = process.env.RECORD_STORE_E2E_MANAGEMENT_URL as string;
    const nodesResponse = await fetch(`${managementUrl}/api/v1/nodes`, {
      headers: { authorization: `Bearer ${MANAGEMENT_TOKEN}` },
    });
    expect(nodesResponse.ok).toBe(true);
    const nodes = (await nodesResponse.json()) as NodeStatus[];
    expect(nodes).toHaveLength(3);
    const node = nodes[2];

    await signedIn.goto(`/cluster/nodes/${node.node_id}`);
    await expect(signedIn.getByRole('heading', { level: 1, name: /Node / })).toBeVisible();
    await expect(signedIn.getByText(node.node_id)).toBeVisible();
    await expect(signedIn.getByText('Voting member')).toBeVisible();

    await signedIn.goto('/cluster/nodes');
    await signedIn.getByRole('button', { name: `Actions for node ${node.node_id}` }).click();
    await signedIn.getByRole('menuitem', { name: 'Enter maintenance' }).click();
    await signedIn.getByRole('button', { name: 'Enter maintenance' }).click();
    await expect(signedIn.getByText('Maintenance', { exact: true })).toBeVisible();

    await signedIn.getByRole('button', { name: `Actions for node ${node.node_id}` }).click();
    await signedIn.getByRole('menuitem', { name: 'Resume node' }).click();
    await expect(signedIn.getByText('Healthy', { exact: true })).toHaveCount(3);
  });

  test('a node serving several drives registers all of them', async ({ signedIn }) => {
    // The first node is configured with two drives beyond its data directory.
    // Nothing before this exercised a real server declaring, opening, and
    // advertising more than one device.
    await signedIn.goto('/cluster/drives');
    await expect(signedIn.getByRole('heading', { level: 1, name: 'Drives' })).toBeVisible();

    // Three one-drive nodes would give three rows; the multi-drive node adds two.
    const rows = signedIn.getByRole('row').filter({ hasText: /standard/ });
    await expect(rows).toHaveCount(5, { timeout: 30_000 });
  });

  test('the topology view places drives under the node serving them', async ({ signedIn }) => {
    await signedIn.goto('/cluster/topology');
    await expect(signedIn.getByRole('heading', { level: 1, name: 'Topology' })).toBeVisible();

    // The cluster labels region, zone, and rack, so all three levels are drawn.
    await expect(signedIn.getByText('region e2e')).toBeVisible();
    await expect(signedIn.getByText('rack r1')).toBeVisible();
    // Nothing is unlabelled, so nothing may claim separation it cannot prove.
    await expect(signedIn.getByText('not proven separate')).toHaveCount(0);
  });
});
