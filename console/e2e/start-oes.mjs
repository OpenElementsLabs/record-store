/**
 * Starts a disposable standalone OES server for end-to-end tests.
 *
 * The console is exercised against the real Rust management API rather than a
 * mock, because the point of these tests is to catch drift between the two.
 * State goes to a fresh temporary directory and is removed on exit.
 */
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = fileURLToPath(new URL('../..', import.meta.url));
const dataDirectory = mkdtempSync(join(tmpdir(), 'oes-e2e-'));

const environment = {
  ...process.env,
  OES_STORAGE_DATA_DIRECTORY: dataDirectory,
  OES_S3_BIND: '127.0.0.1:7600',
  OES_API_BIND: '127.0.0.1:7601',
  OES_RPC_BIND: '127.0.0.1:7603',
  OES_ROOT_ACCESS_KEY: 'e2e-root-access',
  OES_ROOT_SECRET_KEY: 'e2e-root-secret-at-least-sixteen',
  OES_CREDENTIAL_MASTER_KEY: 'e2e-credential-master-key-at-least-32-bytes',
  OES_MANAGEMENT_SYSTEM_TOKEN:
    process.env.OES_E2E_TOKEN ?? 'e2e-management-system-token-32-bytes-long',
  OES_MANAGEMENT_AUDITOR_TOKEN: 'e2e-management-auditor-token-32-bytes-long',
  OES_LOG: 'oes=warn',
};

const server = spawn('cargo', ['run', '--quiet', '--bin', 'oes-server'], {
  cwd: repositoryRoot,
  env: environment,
  stdio: 'inherit',
});

function shutdown(code) {
  server.kill('SIGTERM');
  try {
    rmSync(dataDirectory, { recursive: true, force: true });
  } catch {
    // A leftover temporary directory is not worth failing the run over.
  }
  process.exit(code ?? 0);
}

process.on('SIGTERM', () => shutdown(0));
process.on('SIGINT', () => shutdown(0));
server.on('exit', (code) => shutdown(code ?? 0));
