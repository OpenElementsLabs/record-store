/**
 * Serves the console exactly as the production image does.
 *
 * `output: standalone` emits a `server.js` without the static assets beside it,
 * which is why the image copies them in. The E2E harness does the same, so the
 * suite exercises the artifact that actually ships instead of `next start` —
 * which Next.js does not support for a standalone build, and which would hide a
 * missing-asset regression in the image.
 */
import { spawn } from 'node:child_process';
import { cp } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const standalone = join(root, '.next', 'standalone');

await cp(join(root, '.next', 'static'), join(standalone, '.next', 'static'), {
  recursive: true,
});
await cp(join(root, 'public'), join(standalone, 'public'), { recursive: true });

const server = spawn('node', ['server.js'], {
  cwd: standalone,
  stdio: 'inherit',
  env: { HOSTNAME: '127.0.0.1', ...process.env },
});

for (const signal of ['SIGTERM', 'SIGINT']) {
  process.on(signal, () => server.kill(signal));
}
server.on('exit', (code) => process.exit(code ?? 0));
