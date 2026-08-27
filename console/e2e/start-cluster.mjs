/** Starts and verifies a disposable three-storage-node Record Store cluster. */
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { createServer as createHttpServer } from 'node:http';
import { createServer as createTcpServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = fileURLToPath(new URL('../..', import.meta.url));
const runDirectory = mkdtempSync(join(tmpdir(), 'record-store-e2e-cluster-'));
const host = '127.0.0.1';
const consolePort = port('RECORD_STORE_CLUSTER_E2E_CONSOLE_PORT', 18_602);
const harnessPort = port('RECORD_STORE_CLUSTER_E2E_HARNESS_PORT', 18_604);
const managementToken =
  process.env.RECORD_STORE_E2E_TOKEN ?? 'e2e-management-system-token-32-bytes-long';
const nodes = [
  nodePorts(1, 18_600, 18_601, 18_603),
  nodePorts(2, 18_700, 18_701, 18_703),
  nodePorts(3, 18_800, 18_801, 18_803),
];
const processes = [];
let shuttingDown = false;
let readinessServer;

await Promise.all([
  assertPortFree(consolePort, 'console'),
  assertPortFree(harnessPort, 'harness readiness'),
  ...nodes.flatMap((node) => [
    assertPortFree(node.s3, `node ${node.number} S3`),
    assertPortFree(node.api, `node ${node.number} management`),
    assertPortFree(node.rpc, `node ${node.number} RPC`),
  ]),
]);

spawnNode(nodes[0]);
await verifyBackend(apiUrl(nodes[0]), 'cluster');

for (const node of nodes.slice(1)) {
  const token = await issueJoinToken(apiUrl(nodes[0]), `Playwright node ${node.number}`);
  spawnNode(node, token);
  await verifyBackend(apiUrl(node), 'cluster');
}

await verifyCluster(apiUrl(nodes[0]), 3);
readinessServer = createHttpServer((_request, response) => {
  response.writeHead(200, { 'content-type': 'text/plain' });
  response.end('verified three-node Record Store cluster\n');
}).listen(harnessPort, host);
process.stdout.write(`Verified three-node Record Store cluster through ${apiUrl(nodes[0])}\n`);

function nodePorts(number, s3Fallback, apiFallback, rpcFallback) {
  return {
    number,
    s3: port(`RECORD_STORE_CLUSTER_E2E_NODE_${number}_S3_PORT`, s3Fallback),
    api: port(`RECORD_STORE_CLUSTER_E2E_NODE_${number}_API_PORT`, apiFallback),
    rpc: port(`RECORD_STORE_CLUSTER_E2E_NODE_${number}_RPC_PORT`, rpcFallback),
  };
}

function port(name, fallback) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < 1 || value > 65_535) {
    throw new Error(`${name} must be a valid TCP port`);
  }
  return value;
}

function assertPortFree(value, label) {
  return new Promise((resolve, reject) => {
    const probe = createTcpServer();
    probe.unref();
    probe.once('error', (error) =>
      reject(
        new Error(
          `${label} E2E port ${host}:${value} is already occupied; refusing to adopt an unknown service (${error.code ?? error.message})`,
        ),
      ),
    );
    probe.listen(value, host, () => probe.close(resolve));
  });
}

function spawnNode(node, joinToken) {
  const child = spawn('cargo', ['run', '--quiet', '--bin', 'record-store-server'], {
    cwd: repositoryRoot,
    env: {
      ...process.env,
      RECORD_STORE_MODE: 'cluster',
      RECORD_STORE_STORAGE_DATA_DIRECTORY: join(runDirectory, `node-${node.number}`),
      RECORD_STORE_S3_BIND: `${host}:${node.s3}`,
      RECORD_STORE_API_BIND: `${host}:${node.api}`,
      RECORD_STORE_RPC_BIND: `${host}:${node.rpc}`,
      RECORD_STORE_RPC_ADVERTISE: `${host}:${node.rpc}`,
      RECORD_STORE_CLUSTER_S3_ENDPOINT: `http://${host}:${node.s3}`,
      RECORD_STORE_CLUSTER_FAILURE_DOMAIN: `region=e2e,zone=z${node.number},rack=r${node.number}`,
      RECORD_STORE_CLUSTER_REPLICATION_FACTOR: '3',
      RECORD_STORE_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT: '98',
      RECORD_STORE_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT: '99',
      RECORD_STORE_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT: '100',
      ...(node.number > 1
        ? {
            RECORD_STORE_CLUSTER_SEEDS: `${host}:${nodes[0].rpc}`,
            RECORD_STORE_CLUSTER_JOIN_TOKEN: joinToken,
          }
        : {}),
      RECORD_STORE_ROOT_ACCESS_KEY: 'e2e-root-access',
      RECORD_STORE_ROOT_SECRET_KEY: 'e2e-root-secret-at-least-sixteen',
      RECORD_STORE_CREDENTIAL_MASTER_KEY: 'e2e-credential-master-key-at-least-32-bytes',
      RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN: managementToken,
      RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN: 'e2e-management-auditor-token-32-bytes-long',
      RECORD_STORE_METRICS_SCRAPE_TOKEN: 'e2e-dedicated-metrics-token-at-least-32-bytes',
      RECORD_STORE_LOG: 'record_store=warn',
    },
    stdio: 'inherit',
  });
  processes.push(child);
  child.once('exit', (code, signal) => {
    if (!shuttingDown) {
      console.error(
        `Record Store cluster node ${node.number} exited before shutdown (code=${code}, signal=${signal})`,
      );
      shutdown(code ?? 1);
    }
  });
  return child;
}

async function issueJoinToken(baseUrl, description) {
  const response = await fetch(`${baseUrl}/api/v1/cluster/join-tokens`, {
    method: 'POST',
    headers: {
      authorization: `Bearer ${managementToken}`,
      'content-type': 'application/json',
    },
    body: JSON.stringify({ description, lifetime_seconds: 600 }),
  });
  if (!response.ok) throw new Error(`join-token request failed with HTTP ${response.status}`);
  const body = await response.json();
  if (typeof body?.token !== 'string') throw new Error('join-token response omitted its token');
  return body.token;
}

async function verifyBackend(baseUrl, expectedMode) {
  const deadline = Date.now() + 600_000;
  while (Date.now() < deadline) {
    try {
      const ready = await fetch(`${baseUrl}/ready`);
      if (ready.ok) {
        const response = await authenticatedFetch(`${baseUrl}/api/v1/system/info`);
        if (!response.ok) throw new Error(`identity endpoint returned HTTP ${response.status}`);
        const identity = await response.json();
        if (
          identity?.name !== 'record-store' ||
          identity?.mode !== expectedMode ||
          typeof identity?.version !== 'string' ||
          typeof identity?.cluster_id !== 'string'
        ) {
          throw new Error(`unexpected backend identity: ${JSON.stringify(identity)}`);
        }
        return;
      }
    } catch (error) {
      if (error instanceof Error && error.message.startsWith('unexpected backend identity')) {
        throw error;
      }
    }
    await delay(100);
  }
  throw new Error(`cluster node did not become ready on ${baseUrl}`);
}

async function verifyCluster(baseUrl, expectedNodes) {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const response = await authenticatedFetch(`${baseUrl}/api/v1/cluster`);
    if (response.ok) {
      const status = await response.json();
      if (
        status?.nodes?.length === expectedNodes &&
        status.nodes.every((node) => node.state === 'healthy')
      ) {
        return;
      }
    }
    await delay(100);
  }
  throw new Error(`cluster did not report ${expectedNodes} healthy nodes`);
}

function authenticatedFetch(url) {
  return fetch(url, { headers: { authorization: `Bearer ${managementToken}` } });
}

function apiUrl(node) {
  return `http://${host}:${node.api}`;
}

function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function shutdown(code = 0) {
  if (shuttingDown) return;
  shuttingDown = true;
  readinessServer?.close();
  for (const child of processes) child.kill('SIGTERM');
  try {
    rmSync(runDirectory, { recursive: true, force: true });
  } catch {
    // The operating system can clean an abandoned test directory.
  }
  process.exit(code);
}

process.on('SIGTERM', () => shutdown(0));
process.on('SIGINT', () => shutdown(0));
