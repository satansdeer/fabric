import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, realpathSync, mkdirSync, writeFileSync, readFileSync, rmSync, existsSync, symlinkSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';
import { spawn, spawnSync } from 'node:child_process';
import { gzipSync, gunzipSync } from 'node:zlib';
import { create, install } from '../cli.mjs';
import { directory, archive, pack, loadPackage } from '../package.mjs';
import { conformance, FIXTURE_PROMPT } from '../conformance.mjs';
import { validVersion, validId, safePath, MAX_FRAME } from '../protocol.mjs';

const sdk = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const cli = join(sdk, 'cli.mjs');
function temporary(t) {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fabric-sdk-test-')));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  return root;
}
function environment(root) {
  return { PATH: `${dirname(process.execPath)}:/usr/bin:/bin`, HOME: root, TMPDIR: root, LC_ALL: 'C' };
}
function run(args, root, expected = 0) {
  const r = spawnSync(process.execPath, [cli, ...args], { cwd: root, env: environment(root), encoding: 'utf8', timeout: 15_000, maxBuffer: 1024 * 1024 });
  assert.equal(r.error, undefined);
  assert.equal(r.status, expected, r.stderr);
  return r;
}
function updatePackage(project, change) {
  const path = join(project, 'package.json');
  const pkg = JSON.parse(readFileSync(path, 'utf8'));
  change(pkg);
  writeFileSync(path, JSON.stringify(pkg));
}
function rawPlugin(project, body) { writeFileSync(join(project, 'plugin.ts'), body); }

for (const kind of ['harness', 'target', 'connector']) {
  test(`generated ${kind}: typed project -> real stdio control and execution -> reproducible package`, async t => {
    const root = temporary(t);
    const project = join(root, kind);
    run(['create', kind, project], root);
    const proof = await conformance(project);
    assert.equal(proof.id, kind);
    assert.equal(proof.protocol, 1);
    if (kind === 'target') assert.deepEqual(proof.result, { placement: 'local' });
    else if (kind === 'connector') assert.deepEqual(proof.result, { available: false });
    else {
      assert.deepEqual(proof.result, { ready: true });
      assert.deepEqual(proof.events, [{ type: 'text', text: `Provider-free fixture: ${FIXTURE_PROMPT}` }, { type: 'done' }]);
    }
    // PATH deliberately has no Rust toolchain. Generated projects need no npm install.
    const r = run(['dev', project, '--once'], root);
    assert.equal(JSON.parse(r.stdout).conformance, 'passed');
    const snapshot = directory(project);
    const bytes = pack(snapshot);
    assert.deepEqual(bytes, pack(snapshot));
    assert.equal(archive(bytes).hash, snapshot.hash);
    const tarball = join(root, `${kind}.tgz`);
    run(['pack', project, '--out', tarball], root);
    assert.deepEqual(JSON.parse(run(['hash', tarball], root).stdout), JSON.parse(run(['hash', project], root).stdout));
    assert.equal(loadPackage(tarball).info.kind, kind);
    {
      const checked = spawnSync(process.execPath, [join(sdk, 'node_modules/typescript/bin/tsc'), '--project', project], { env: environment(root), encoding: 'utf8', timeout: 15_000 });
      assert.equal(checked.status, 0, checked.stdout + checked.stderr);
    }
  });
}

test('pin/version/trust and SDK/protocol mismatches fail before any plugin execution', async t => {
  const root = temporary(t);
  const project = create('target', join(root, 'target'));
  const marker = join(root, 'executed');
  rawPlugin(project, `import { writeFileSync } from 'node:fs'; writeFileSync(${JSON.stringify(marker)}, 'bad');`);
  const snapshot = directory(project);
  const opts = { trust: true, version: '1.0.0', sha256: snapshot.hash, registry: join(root, 'registry'), fabric: '/unavailable' };
  assert.throws(() => install(project, { ...opts, trust: false }), /trust_required/);
  assert.throws(() => install(project, { ...opts, sha256: '0'.repeat(64) }), /pin_mismatch/);
  assert.throws(() => install(project, { ...opts, version: '2.0.0' }), /pin_mismatch/);
  writeFileSync(join(project, 'plugin.ts'), 'tampered SECRET_CANARY');
  assert.throws(() => install(project, opts), /pin_mismatch/);
  for (const [key, value, code] of [['sdk', '999.0.0', 'sdk_mismatch'], ['protocol', 2, 'protocol_mismatch']]) {
    updatePackage(project, pkg => { pkg.fabricPlugin.sdk = '0.1.0'; pkg.fabricPlugin.protocol = 1; pkg.fabricPlugin[key] = value; });
    await assert.rejects(conformance(project), new RegExp(code));
    const r = run(['test', project], root, 1);
    assert.match(r.stderr, /mismatch/);
    assert.doesNotMatch(r.stderr, /SECRET_CANARY/);
  }
  assert.equal(existsSync(marker), false);
});

test('stdio malformed/oversized/invalid UTF-8, wrong hello, remote secrets and hangs are rejected', async t => {
  const root = temporary(t);
  const project = create('target', join(root, 'target'));
  const cases = [
    [`process.stdout.write('SECRET_CANARY\\n')`, 'malformed_json'],
    [`process.stdout.write('x'.repeat(65536) + '\\n')`, 'frame_too_large'],
    [`process.stdout.write(Buffer.from([0xff, 10]))`, 'malformed_json'],
    [`process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:1,result:{protocol:2,id:'target',version:'1.0.0',kind:'target'}})+'\\n')`, 'negotiation_failed'],
    [`process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:1,error:{code:1,message:'SECRET_CANARY'}})+'\\n')`, 'remote_error'],
    [`process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:2,result:{}})+'\\n')`, 'invalid_response'],
    [`process.stdout.write('{}')`, 'truncated_frame'],
  ];
  for (const [source, code] of cases) {
    rawPlugin(project, source);
    await assert.rejects(conformance(project), new RegExp(code));
    const r = run(['test', project], root, 1);
    assert.doesNotMatch(r.stderr + r.stdout, /SECRET_CANARY/);
  }
  const pidFile = join(root, 'pid');
  rawPlugin(project, `import {writeFileSync} from 'node:fs'; writeFileSync(${JSON.stringify(pidFile)}, String(process.pid)); setInterval(()=>{}, 1000);`);
  await assert.rejects(conformance(project, { timeout: 500 }), /timeout/);
  const pid = Number(readFileSync(pidFile, 'utf8'));
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
});

test('SDK rejects malformed, oversized and out-of-order host requests without reflecting input', t => {
  const root = temporary(t);
  const project = create('connector', join(root, 'connector'));
  for (const input of [
    'SECRET_CANARY\n', 'x'.repeat(MAX_FRAME) + '\n', Buffer.from([0xff, 10]),
    JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocol: 2 } }) + '\n',
    JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'connector.evidence', params: {} }) + '\n',
  ]) {
    const r = spawnSync(process.execPath, [join(project, 'plugin.ts'), '--fabric-plugin-stdio'], { input, env: environment(root), encoding: 'utf8', timeout: 5000, maxBuffer: MAX_FRAME * 2 });
    assert.equal(r.status, 1);
    assert.equal(r.stdout, '');
    assert.doesNotMatch(r.stderr, /SECRET_CANARY/);
  }
});

test('callback errors and invalid target/connector results are fixed protocol failures', async t => {
  const root = temporary(t);
  for (const [kind, callback, code] of [
    ['target', `plan: () => ({placement:'remote', reclaim:true})`, 'remote_error'],
    ['connector', `evidence: () => { throw new Error('SECRET_CANARY'); }`, 'remote_error'],
  ]) {
    const project = create(kind, join(root, kind));
    const name = kind === 'target' ? 'defineTarget' : 'defineConnector';
    rawPlugin(project, `import {${name},serve} from './sdk/index.mjs'; await serve(${name}({id:'${kind}',version:'1.0.0',${callback}}));`);
    await assert.rejects(conformance(project), new RegExp(code));
    assert.doesNotMatch(run(['test', project], root, 1).stderr, /SECRET_CANARY/);
  }
});

function changedHeader(bytes, mutate) {
  const tar = gunzipSync(bytes);
  const h = tar.subarray(0, 512);
  mutate(h);
  h.fill(32, 148, 156);
  h.write(h.reduce((a, b) => a + b, 0).toString(8).padStart(6, '0') + '\0 ', 148, 8);
  return gzipSync(tar);
}
test('archive/path/file boundaries reject traversal, links, duplicate/truncated/bomb packages and devices', t => {
  const root = temporary(t);
  const project = create('target', join(root, 'target'));
  const bytes = pack(directory(project));
  for (const name of ['package/../escape', '/absolute', 'package/a/../../escape', 'package/a\\b', 'package/.git/config']) {
    const changed = changedHeader(bytes, h => { h.fill(0, 0, 100); h.write(name, 0); });
    assert.throws(() => archive(changed));
  }
  for (const type of [49, 50, 51, 54, 120]) assert.throws(() => archive(changedHeader(bytes, h => { h[156] = type; })));
  assert.throws(() => archive(bytes.subarray(0, bytes.length - 10)));
  assert.throws(() => archive(gzipSync(Buffer.alloc(34 * 1024 * 1024))));
  const tar = gunzipSync(bytes);
  assert.throws(() => archive(gzipSync(Buffer.concat([tar.subarray(0, -1024), tar]))));
  symlinkSync(join(project, 'sdk'), join(project, 'linked'));
  updatePackage(project, pkg => { pkg.files.push('linked/index.mjs'); });
  assert.throws(() => directory(project), /unsafe_package_path/);
  assert.throws(() => loadPackage('/dev/zero'), /invalid_file/);
  assert.equal(existsSync(join(root, 'escape')), false);
});

test('identity/version/path validation matches protocol syntax including newline and numeric prerelease edges', () => {
  for (const v of ['0.0.0', '1.2.3-alpha.1+build.007', '1.0.0+000', '1.0.0-0']) assert.equal(validVersion(v), true);
  for (const v of ['01.0.0', '1.0', '1.0.0-01', '1.0.0+', '1.0.0\n', '1.0.0-a..b']) assert.equal(validVersion(v), false);
  for (const v of ['omp', 'fake-a', 'bad\n', '../bad', 'A']) assert.equal(validId(v), false);
  for (const v of ['', '../a', '/a', 'a//b', 'a\n', 'a\\b']) assert.equal(safePath(v), false);
});

const npmCli = process.env.npm_execpath;
test('npm-packed SDK scaffolds offline; generated npm test and npm-pack package need no Rust or lifecycle trust', { skip: !npmCli && 'Run via npm test to exercise npm packaging' }, t => {
  const root = temporary(t);
  const config = join(root, 'empty.npmrc');
  writeFileSync(config, '');
  const globalConfig = join(root, 'global.npmrc');
  writeFileSync(globalConfig, '');
  const npm = (cwd, args) => {
    const r = spawnSync(process.execPath, [npmCli, '--userconfig', config, '--globalconfig', globalConfig, '--cache', join(root, 'cache'), ...args], { cwd, env: environment(root), encoding: 'utf8', timeout: 30_000, maxBuffer: 1024 * 1024 });
    assert.equal(r.status, 0, r.stderr + r.stdout);
    return r;
  };
  const packed = JSON.parse(npm(sdk, ['pack', '--ignore-scripts', '--json', '--pack-destination', root]).stdout)[0];
  const unpack = join(root, 'unpack');
  mkdirSync(unpack);
  const extracted = spawnSync('/usr/bin/tar', ['-xzf', join(root, packed.filename), '-C', unpack], { encoding: 'utf8' });
  assert.equal(extracted.status, 0, extracted.stderr);
  const project = join(root, 'packed-project');
  const created = spawnSync(process.execPath, [join(unpack, 'package/cli.mjs'), 'create', 'harness', project], { env: environment(root), encoding: 'utf8' });
  assert.equal(created.status, 0, created.stderr);
  assert.equal(JSON.parse(npm(project, ['test', '--silent']).stdout).conformance, 'passed');
  const marker = join(root, 'lifecycle-ran');
  updatePackage(project, pkg => { pkg.scripts.prepack = `node -e "require('fs').writeFileSync('${marker}', 'bad')"`; });
  const pluginPack = JSON.parse(npm(project, ['pack', '--ignore-scripts', '--json', '--pack-destination', root]).stdout)[0];
  assert.equal(loadPackage(join(root, pluginPack.filename)).hash, directory(project).hash);
  assert.equal(existsSync(marker), false);
});

function inCheckout(path) {
  for (;;) { if (existsSync(join(path, '.git'))) return true; if (dirname(path) === path) return false; path = dirname(path); }
}
const host = process.env.FABRIC_PLUGIN_TEST_HOST;
const hostSkip = !host ? 'Set FABRIC_PLUGIN_TEST_HOST to test actual Core installation' : inCheckout(realpathSync(tmpdir())) ? 'Core registry intentionally forbids checkout-local TMPDIR; external writes not authorized in this worker' : false;
test('actual Core: scaffold -> conformance -> local package install/check, tamper, upgrade, revoke', { skip: hostSkip }, async t => {
  const root = temporary(t);
  const registry = join(root, 'registry');
  mkdirSync(registry, { mode: 0o700 });
  function fabric(args, expected = 0) {
    const r = spawnSync(host, ['plugins', '--registry', registry, ...args], { env: environment(root), encoding: 'utf8', timeout: 15_000 });
    assert.equal(r.status, expected, r.stderr);
    assert.doesNotMatch(r.stderr + r.stdout, /SECRET_CANARY/);
    return r;
  }
  for (const kind of ['harness', 'target', 'connector']) {
    const project = create(kind, join(root, kind));
    await conformance(project);
    const first = directory(project);
    const tarball = join(root, `${kind}.tgz`);
    writeFileSync(tarball, pack(first));
    run(['install', tarball, '--registry', registry, '--fabric', host, '--version', '1.0.0', '--sha256', first.hash, '--trust'], root);
    assert.deepEqual(JSON.parse(fabric(['check', kind]).stdout), { compatible: true, healthy: true });
    const entry = join(registry, 'packages', first.hash, 'plugin.ts');
    const source = readFileSync(entry);
    writeFileSync(entry, 'SECRET_CANARY: tampered');
    assert.match(fabric(['check', kind], 1).stderr, /integrity_mismatch/);
    writeFileSync(entry, source);
    updatePackage(project, pkg => { pkg.version = '1.0.1'; });
    rawPlugin(project, readFileSync(join(project, 'plugin.ts'), 'utf8').replace("version: '1.0.0'", "version: '1.0.1'"));
    const next = directory(project);
    install(project, { registry, fabric: host, version: '1.0.1', sha256: next.hash, trust: true, upgrade: true });
    fabric(['check', kind]);
    fabric(['revoke', kind]);
    assert.match(fabric(['check', kind], 1).stderr, /revoked/);
  }
});

test('dev watches the explicit project and re-runs actual conformance after an edit', { skip: process.platform !== 'darwin' && 'Node --watch-path is macOS-first' }, async t => {
  const root = temporary(t);
  const project = create('connector', join(root, 'watched'));
  const child = spawn(process.execPath, [cli, 'dev', project], { env: environment(root), detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  const closed = new Promise(resolve => child.once('close', resolve));
  let output = '';
  let errors = '';
  let edited = false;
  child.stderr.on('data', bytes => { errors += bytes; });
  try {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`watch deadline: ${output} ${errors}`)), 15_000);
      child.once('error', reject);
      child.stdout.on('data', bytes => {
        output += bytes;
        const proofs = output.match(/"conformance":"passed"/g) ?? [];
        if (proofs.length === 1 && !edited) {
          edited = true;
          setTimeout(() => {
            const entry = join(project, 'plugin.ts');
            writeFileSync(entry, readFileSync(entry, 'utf8').replace('available: false', 'available: true'));
          }, 300);
        }
        if (proofs.length >= 2) { clearTimeout(timer); resolve(); }
      });
    });
  } finally {
    try { process.kill(-child.pid, 'SIGKILL'); } catch {}
    await closed;
  }
});
