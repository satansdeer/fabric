#!/usr/bin/env node
import { mkdirSync, writeFileSync, lstatSync, realpathSync, readdirSync, rmSync, renameSync, mkdtempSync, chmodSync } from 'node:fs';
import { dirname, basename, resolve, join, isAbsolute } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';
import { directory, loadPackage, pack, contentHash } from './package.mjs';
import { conformance } from './conformance.mjs';
import { SDK_VERSION, kinds, validId, validVersion, fail, readBounded, MAX_PACKAGE } from './protocol.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const runtimeFiles = ['index.mjs', 'index.d.mts', 'protocol.mjs', 'webhook.mjs', 'package.mjs', 'conformance.mjs', 'cli.mjs'];
const warning = 'TRUST WARNING: plugins execute as your user, not in a sandbox. Install only reviewed code; --trust is explicit consent. No npm lifecycle scripts are run.';
export function create(kind, path, id = basename(resolve(path))) {
  if (!Object.hasOwn(kinds, kind) || !validId(id)) fail('invalid_identity');
  const root = resolve(path);
  if (realpathSync(dirname(root)) !== dirname(root)) fail('unsafe_package_path');
  mkdirSync(root, { mode: 0o700 }); // Never overwrite an existing project.
  mkdirSync(join(root, 'sdk'), { mode: 0o700 });
  mkdirSync(join(root, 'sdk/samples'), { mode: 0o700 });
  for (const name of runtimeFiles) writeFileSync(join(root, 'sdk', name), readBounded(join(here, name), MAX_PACKAGE), { mode: 0o600, flag: 'wx' });
  for (const k of Object.keys(kinds)) writeFileSync(join(root, 'sdk/samples', `${k}.ts`), readBounded(join(here, 'samples', `${k}.ts`), 65_536), { mode: 0o600, flag: 'wx' });
  const source = readBounded(join(here, 'samples', `${kind}.ts`), 65_536).toString('utf8').replaceAll(`sample-${kind}`, id).replace("'../index.mjs'", "'./sdk/index.mjs'");
  writeFileSync(join(root, 'plugin.ts'), source, { mode: 0o600, flag: 'wx' });
  writeFileSync(join(root, 'package.json'), JSON.stringify({
    name: id, version: '1.0.0', type: 'module', engines: { node: '>=22.18.0' },
    files: ['plugin.ts', 'sdk'],
    scripts: { test: 'node sdk/cli.mjs test .', dev: 'node sdk/cli.mjs dev .' },
    fabricPlugin: { id, kind, entry: 'plugin.ts', sdk: SDK_VERSION, protocol: 1 },
  }, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  writeFileSync(join(root, 'tsconfig.json'), JSON.stringify({ compilerOptions: { strict: true, noEmit: true, module: 'NodeNext', target: 'ES2022' }, include: ['plugin.ts'] }, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  return root;
}
/** Package the separately built reference executable; never builds or probes a provider. */
export function reference(id, path, binary) {
  const roots = { omp: ['PI_CODING_AGENT_DIR'], codex: ['CODEX_HOME'] };
  const fingerprints = {
    omp: { domain: 'omp-native-config-v1', files: ['config.yml', 'config.yaml', 'settings.json', 'models.yml', 'models.yaml', 'mcp.json', '.mcp.json'], roots: [['native-agent-dir', ''], ['native-home', '.omp/agent']] },
    codex: { domain: 'codex-native-config-v1', files: ['config.toml'], roots: [['native-config-dir', '']] },
  };
  if ((!Object.hasOwn(roots, id) && id !== 'local') || !binary) fail('invalid_identity');
  const root = resolve(path);
  if (realpathSync(dirname(root)) !== dirname(root)) fail('unsafe_package_path');
  const bytes = readBounded(realpathSync(binary), MAX_PACKAGE - 65_536);
  mkdirSync(root, { mode: 0o700 });
  writeFileSync(join(root, 'plugin'), bytes, { mode: 0o700, flag: 'wx' });
  writeFileSync(join(root, 'package.json'), JSON.stringify({
    name: id, version: '0.1.0', files: ['plugin'],
    fabricPlugin: id === 'local'
      ? { id, kind: 'target', entry: 'plugin', sdk: SDK_VERSION, protocol: 1, runtime: 'native', args: [] }
      : { id, kind: 'harness', entry: 'plugin', sdk: SDK_VERSION, protocol: 1, runtime: 'native', args: [id], native: { executable: id, root_environment: roots[id], fingerprint: fingerprints[id] } },
  }, null, 2) + '\n', { mode: 0o600, flag: 'wx' });
  return root;
}
function privateDirectory(path) {
  if (!isAbsolute(path) || realpathSync(path) !== path) fail('unsafe_registry');
  const stat = lstatSync(path);
  if (!stat.isDirectory() || stat.uid !== process.getuid() || (stat.mode & 0o077)) fail('unsafe_registry');
  let parent = path;
  for (;;) {
    try { lstatSync(join(parent, '.git')); fail('project_registry_forbidden'); }
    catch (e) { if (e.code !== 'ENOENT') throw e; }
    if (dirname(parent) === parent) break;
    parent = dirname(parent);
  }
}
function host(fabric, registry, command, argument) {
  if (!isAbsolute(fabric) || realpathSync(fabric) !== fabric || !lstatSync(fabric).isFile()) fail('fabric_path_required');
  const r = spawnSync(fabric, ['plugins', '--registry', registry, command, ...(argument ? [argument] : [])], {
    cwd: '/', env: { PATH: '/usr/bin:/bin', LC_ALL: 'C' }, encoding: 'utf8', maxBuffer: 1024 * 1024, timeout: 10_000,
  });
  if (r.error || r.status !== 0) fail('fabric_rejected'); // Never reflect host paths or remote diagnostics.
}
function installedFiles(root) {
  const files = new Map();
  let total = 0;
  let entries = 0;
  function visit(path, depth = 0) {
    if (depth > 16) fail('invalid_package');
    for (const name of readdirSync(path)) {
      if (++entries > 512) fail('invalid_package');
      const full = join(path, name);
      const stat = lstatSync(full);
      if (stat.isDirectory()) visit(full, depth + 1);
      else if (stat.isFile()) {
        const bytes = readBounded(full, MAX_PACKAGE - total);
        total += bytes.length;
        files.set(full.slice(root.length + 1), bytes);
        if (files.size > 256) fail('invalid_package');
      } else fail('unsafe_package_path');
    }
  }
  visit(root);
  return files;
}
export function install(path, { registry, fabric, version, sha256, trust = false, upgrade = false }) {
  process.stderr.write(warning + '\n');
  if (!trust) fail('trust_required');
  if (!validVersion(version) || typeof sha256 !== 'string' || sha256.length !== 64 || !/^[a-f0-9]+$/.test(sha256)) fail('pin_required');
  const snapshot = loadPackage(path);
  if (snapshot.info.version !== version || snapshot.hash !== sha256) fail('pin_mismatch');
  privateDirectory(registry);
  host(fabric, registry, 'list'); // Existing Core registry validation stays authoritative.
  const packages = join(registry, 'packages');
  try { mkdirSync(packages, { mode: 0o700 }); } catch (e) { if (e.code !== 'EEXIST') throw e; }
  privateDirectory(packages);
  const destination = join(packages, sha256);
  let staged;
  let manifestPath;
  try {
    try {
      privateDirectory(destination);
      if (contentHash(installedFiles(destination)) !== sha256) fail('pin_mismatch');
    } catch (e) {
      if (e.code !== 'ENOENT') throw e;
      staged = mkdtempSync(join(packages, '.install-'));
      for (const [name, bytes] of snapshot.files) {
        const full = join(staged, name);
        mkdirSync(dirname(full), { recursive: true, mode: 0o700 });
        writeFileSync(full, bytes, { mode: 0o600, flag: 'wx' });
      }
      if (snapshot.info.runtime === 'native') chmodSync(join(staged, snapshot.info.entry), 0o700);
      renameSync(staged, destination);
      staged = undefined;
    }
    const manifest = {
      id: snapshot.info.id, version, protocol: { min: 1, max: 1 }, kind: snapshot.info.kind,
      executable: snapshot.info.runtime === 'native' ? join(destination, snapshot.info.entry) : realpathSync(process.execPath),
      args: snapshot.info.runtime === 'native' ? snapshot.info.args ?? [] : [join(destination, snapshot.info.entry), ...(snapshot.info.args ?? [])],
      capabilities: [kinds[snapshot.info.kind]], integrity: { root: destination, sha256 },
      ...(snapshot.info.native ? { native: snapshot.info.native } : {}),
    };
    // Kept outside the hashed package, removed after Core persists the manifest.
    const pendingManifest = join(registry, `.manifest-${process.pid}-${Date.now()}.json`);
    writeFileSync(pendingManifest, JSON.stringify(manifest), { mode: 0o600, flag: 'wx' });
    manifestPath = pendingManifest;
    host(fabric, registry, upgrade ? 'upgrade' : 'install', manifestPath);
    return { id: manifest.id, version, sha256 };
  } finally {
    // Do not roll back copied content after a possibly committed host write.
    if (staged) rmSync(staged, { recursive: true, force: true });
    if (manifestPath) rmSync(manifestPath, { force: true });
  }
}
const diagnostics = {
  trust_required: 'Pass --trust only after reviewing the plugin and its pinned content.',
  pin_required: 'Supply --version SEMVER and --sha256 CONTENT_HASH from fabric-plugin hash.',
  pin_mismatch: 'Pinned version/content SHA-256 does not match. Review the package; do not replace the pin blindly.',
  sdk_mismatch: 'SDK mismatch: use SDK 0.1.0 and regenerate or update the plugin before admission.',
  protocol_mismatch: 'Protocol mismatch: this SDK and Fabric support plugin protocol 1.',
  negotiation_failed: 'Negotiation failed: plugin id/version/kind/protocol must match package metadata.',
  unsafe_registry: 'Use an existing canonical owner-only (0700) registry outside every Git checkout.',
  project_registry_forbidden: 'Plugin registries inside Git checkouts are forbidden.',
  fabric_path_required: 'Supply --fabric with the canonical absolute path to an installed Fabric binary.',
  fabric_rejected: 'Fabric rejected the registry operation. Use the owner-managed Fabric CLI to inspect it.',
  unsafe_archive_entry: 'Unsafe archive entry: only bounded regular package/ files and directories are accepted.',
  invalid_archive: 'Invalid archive: use a local npm pack or fabric-plugin pack gzip/ustar package.',
  timeout: 'Plugin conformance deadline exceeded; the child process group was terminated.',
};
export async function main(args) {
  const command = args.shift();
  const allowed = { create: ['id'], reference: ['binary'], dev: ['once'], test: ['connector-fixture'], hash: [], pack: ['out'], install: ['registry', 'fabric', 'version', 'sha256', 'trust', 'upgrade'] };
  if (!Object.hasOwn(allowed, command)) fail('usage');
  const options = {};
  const positional = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (!a.startsWith('--')) { positional.push(a); continue; }
    const key = a.slice(2);
    if (!allowed[command].includes(key) || Object.hasOwn(options, key)) fail('usage');
    if (['once', 'trust', 'upgrade'].includes(key)) options[key] = true;
    else {
      if (!args[i + 1] || args[i + 1].startsWith('--')) fail('usage');
      options[key] = args[++i];
    }
  }
  if (positional.length !== (['create', 'reference'].includes(command) ? 2 : 1)) fail('usage');
  if (command === 'reference') {
    reference(positional[0], positional[1], options.binary);
    console.log('Packaged reference plugin; installation still requires explicit content-pinned trust.');
  } else if (command === 'create') {
    create(positional[0], positional[1], options.id);
    console.log('Created provider-free TypeScript plugin. Run npm test in the project.');
  } else if (command === 'test' || command === 'dev' && options.once) {
    process.stderr.write('Trusted-local conformance: runs plugin code without a sandbox; fixtures require no provider.\n');
    const connectorFixture = options['connector-fixture'] ? JSON.parse(readBounded(resolve(options['connector-fixture']), 65_536).toString('utf8')) : undefined;
    const proof = await conformance(positional[0], { connectorFixture });
    console.log(JSON.stringify({ id: proof.id, kind: proof.kind, protocol: proof.protocol, conformance: 'passed', harnessEvents: proof.events.length }));
  } else if (command === 'dev') {
    directory(positional[0]);
    const child = spawn(process.execPath, [`--watch-path=${resolve(positional[0])}`, fileURLToPath(import.meta.url), 'test', resolve(positional[0])], { stdio: 'inherit' });
    for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => child.kill(signal));
    child.once('error', () => { process.exitCode = 1; });
    await new Promise(resolve => child.once('close', code => { process.exitCode = code ?? 1; resolve(); }));
  } else if (command === 'hash') {
    const p = loadPackage(positional[0]);
    console.log(JSON.stringify({ id: p.info.id, version: p.info.version, sha256: p.hash }));
  } else if (command === 'pack') {
    if (!options.out) fail('usage');
    const p = directory(positional[0]);
    writeFileSync(resolve(options.out), pack(p), { mode: 0o600, flag: 'wx' });
    console.log(JSON.stringify({ version: p.info.version, sha256: p.hash }));
  } else {
    if (!options.registry || !options.fabric) fail('usage');
    console.log(JSON.stringify(install(positional[0], options)));
  }
}
if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  main(process.argv.slice(2)).catch(error => {
    const code = typeof error?.message === 'string' ? error.message.split(':')[0] : '';
    const message = code === 'usage' ? 'Usage: fabric-plugin create KIND DIR [--id ID] | reference omp|codex|local DIR --binary BINARY | dev DIR [--once] | test DIR | hash PATH | pack DIR --out FILE.tgz | install PATH --registry DIR --fabric BINARY --version VERSION --sha256 HASH --trust [--upgrade]' : diagnostics[code] ?? 'Invalid package/input or failed plugin operation. No remote error details are displayed.';
    process.stderr.write(`Fabric plugin: ${message}\n`);
    process.exitCode = 1;
  });
}
