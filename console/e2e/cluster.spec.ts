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
});
