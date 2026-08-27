/** Starts and verifies a disposable standalone OES backend for Playwright. */
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { createServer as createHttpServer } from 'node:http';
import { createServer as createTcpServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = fileURLToPath(new URL('../..', import.meta.url));
const dataDirectory = mkdtempSync(join(tmpdir(), 'oes-e2e-standalone-'));
const host = '127.0.0.1';
const s3Port = port('OES_E2E_S3_PORT', 47_600);
const apiPort = port('OES_E2E_API_PORT', 47_601);
const consolePort = port('OES_E2E_CONSOLE_PORT', 47_602);
const rpcPort = port('OES_E2E_RPC_PORT', 47_603);
const harnessPort = port('OES_E2E_HARNESS_PORT', 47_604);
const managementToken = process.env.OES_E2E_TOKEN ?? 'e2e-management-system-token-32-bytes-long';

await Promise.all([
  assertPortFree(s3Port, 'S3', 'OES_E2E_S3_PORT'),
  assertPortFree(apiPort, 'management', 'OES_E2E_API_PORT'),
  assertPortFree(consolePort, 'console', 'OES_E2E_CONSOLE_PORT'),
  assertPortFree(rpcPort, 'RPC', 'OES_E2E_RPC_PORT'),
  assertPortFree(harnessPort, 'harness readiness', 'OES_E2E_HARNESS_PORT'),
]);

const server = spawn('cargo', ['run', '--quiet', '--bin', 'oes-server'], {
  cwd: repositoryRoot,
  env: {
    ...process.env,
    OES_MODE: 'standalone',
    OES_STORAGE_DATA_DIRECTORY: dataDirectory,
    OES_S3_BIND: `${host}:${s3Port}`,
    OES_API_BIND: `${host}:${apiPort}`,
    OES_RPC_BIND: `${host}:${rpcPort}`,
    OES_ROOT_ACCESS_KEY: 'e2e-root-access',
    OES_ROOT_SECRET_KEY: 'e2e-root-secret-at-least-sixteen',
    OES_CREDENTIAL_MASTER_KEY: 'e2e-credential-master-key-at-least-32-bytes',
    OES_MANAGEMENT_SYSTEM_TOKEN: managementToken,
    OES_MANAGEMENT_AUDITOR_TOKEN: 'e2e-management-auditor-token-32-bytes-long',
    OES_METRICS_SCRAPE_TOKEN: 'e2e-dedicated-metrics-token-at-least-32-bytes',
    OES_LOG: 'oes=warn',
  },
  stdio: 'inherit',
});

let exited = false;
let shuttingDown = false;
let readinessServer;
server.once('exit', (code, signal) => {
  exited = true;
  if (!shuttingDown) {
    console.error(`OES E2E backend exited before shutdown (code=${code}, signal=${signal})`);
    shutdown(code ?? 1);
  }
});

await verifyBackend(`http://${host}:${apiPort}`, 'standalone');
readinessServer = createHttpServer((_request, response) => {
  response.writeHead(200, { 'content-type': 'text/plain' });
  response.end('verified standalone OES\n');
}).listen(harnessPort, host);
process.stdout.write(`Verified standalone OES backend on ${host}:${apiPort}\n`);

function port(name, fallback) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < 1 || value > 65_535) {
    throw new Error(`${name} must be a valid TCP port`);
  }
  return value;
}

function assertPortFree(value, label, variable) {
  return new Promise((resolve, reject) => {
    const probe = createTcpServer();
    probe.unref();
    probe.once('error', (error) => {
      reject(
        new Error(
          `${label} E2E port ${host}:${value} is already occupied; refusing to adopt an ` +
            `unknown service (${error.code ?? error.message}). Set ${variable} to a free ` +
            `port to move this listener.`,
        ),
      );
    });
    probe.listen(value, host, () => probe.close(resolve));
  });
}

async function verifyBackend(baseUrl, expectedMode) {
  const deadline = Date.now() + 600_000;
  while (Date.now() < deadline) {
    if (exited) throw new Error('OES exited before its identity could be verified');
    try {
      const ready = await fetch(`${baseUrl}/ready`);
      if (ready.ok) {
        const response = await fetch(`${baseUrl}/api/v1/system/info`, {
          headers: { authorization: `Bearer ${managementToken}` },
        });
        if (!response.ok) throw new Error(`identity endpoint returned HTTP ${response.status}`);
        const identity = await response.json();
        if (
          identity?.name !== 'oes' ||
          identity?.mode !== expectedMode ||
          typeof identity?.version !== 'string'
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
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`OES did not become ready on ${baseUrl}`);
}

function shutdown(code = 0) {
  if (shuttingDown) return;
  shuttingDown = true;
  readinessServer?.close();
  if (!exited) server.kill('SIGTERM');
  try {
    rmSync(dataDirectory, { recursive: true, force: true });
  } catch {
    // The operating system can clean an abandoned test directory.
  }
  process.exit(code);
}

process.on('SIGTERM', () => shutdown(0));
process.on('SIGINT', () => shutdown(0));
