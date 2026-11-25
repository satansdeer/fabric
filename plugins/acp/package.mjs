#!/usr/bin/env node
import { mkdirSync, writeFileSync, realpathSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { readBounded } from '../../sdk/typescript/protocol.mjs';
import { directory, pack } from '../../sdk/typescript/package.mjs';
import { validate, fileHash, sha256, fail, fields } from './config.mjs';

const here = dirname(fileURLToPath(import.meta.url));
/** Offline packaging only: never probes or executes the selected ACP program. */
export function prepare(config, destination) {
  if (!fields(config, ['executable', 'args', 'nativeHome', 'cwd', 'pins', 'requestTimeoutMs'])) fail('acp_invalid_config');
  const runtime = realpathSync(process.execPath);
  if (Buffer.byteLength(`#!${runtime}\n`) > 127 || /\s/.test(runtime)) fail('acp_unsafe_runtime');
  const launch = validate({
    executable: config.executable, args: config.args, nativeHome: config.nativeHome,
    cwd: config.cwd, pins: config.pins ?? [], requestTimeoutMs: config.requestTimeoutMs ?? 60_000,
    runtime: { path: runtime, sha256: fileHash(runtime) },
  });
  const root = resolve(destination);
  if (realpathSync(dirname(root)) !== dirname(root)) fail('acp_unsafe_path');
  const bytes = JSON.stringify(launch) + '\n';
  const files = ['fabric-acp-bridge', 'bridge.mjs', 'inspect.mjs', 'config.mjs', 'transport.mjs', 'launch.json', 'sdk'];
  mkdirSync(root, { mode: 0o700 }); // Never overwrite an existing package.
  try {
    mkdirSync(join(root, 'sdk'), { mode: 0o700 });
    for (const name of ['index.mjs', 'protocol.mjs', 'webhook.mjs']) {
      writeFileSync(join(root, 'sdk', name), readBounded(join(here, '../../sdk/typescript', name), 1_048_576), { mode: 0o600, flag: 'wx' });
    }
    for (const name of ['bridge.mjs', 'inspect.mjs', 'config.mjs', 'transport.mjs', 'README.md']) {
      writeFileSync(join(root, name), readBounded(join(here, name), 1_048_576), { mode: 0o600, flag: 'wx' });
    }
    writeFileSync(join(root, 'launch.json'), bytes, { mode: 0o600, flag: 'wx' });
    writeFileSync(join(root, 'fabric-acp-bridge'), `#!${runtime}\nimport { main } from './bridge.mjs';\nawait main();\n`, { mode: 0o700, flag: 'wx' });
    writeFileSync(join(root, 'package.json'), JSON.stringify({
      name: 'fabric-acp', version: '0.1.0', type: 'module', files,
      fabricPlugin: { id: 'acp', kind: 'harness', entry: 'fabric-acp-bridge', runtime: 'native', args: [], sdk: '0.1.0', protocol: 1,
        native: { executable: 'fabric-acp-bridge', root_environment: ['FABRIC_ACP_HOME'],
          fingerprint: { domain: `fabric-acp-v1-${sha256(bytes)}`, files: [], roots: [] } } },
    }, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
    return directory(root);
  } catch (error) { rmSync(root, { recursive: true, force: true }); throw error; }
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    if (process.argv.length !== 5) fail('acp_package_arguments');
    const snapshot = prepare(JSON.parse(readBounded(resolve(process.argv[2]), 32_768)), process.argv[3]);
    writeFileSync(process.argv[4], pack(snapshot), { mode: 0o600, flag: 'wx' });
    console.log(JSON.stringify({ sha256: snapshot.hash, package: resolve(process.argv[3]) }));
  } catch { console.error('acp_package_failed'); process.exitCode = 1; }
}
