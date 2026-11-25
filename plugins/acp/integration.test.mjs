import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync, readdirSync, chmodSync, rmSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { prepare } from './package.mjs';
import { fileHash, sha256, VERSION } from './config.mjs';
import { create, install } from '../../sdk/typescript/cli.mjs';
import { directory, pack, archive } from '../../sdk/typescript/package.mjs';

const source = realpathSync(join(dirname(fileURLToPath(import.meta.url)), '../..'));
const host = process.env.FABRIC_ACP_TEST_HOST ?? join(source, 'target/debug/fabric');
const node = realpathSync(process.execPath);
function remove(path) {
  if (!existsSync(path)) return;
  chmodSync(path, 0o700);
  for (const entry of readdirSync(path, { withFileTypes: true })) if (entry.isDirectory()) remove(join(path, entry.name));
  rmSync(path, { recursive: true, force: true });
}
const lines = path => existsSync(path) ? readFileSync(path, 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line)) : [];
function reaped(pid) {
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' }, `owned child ${pid} still alive`);
}

test('installed ACP package through the real Fabric host (no providers)', { timeout: 180_000 }, async t => {
  assert.ok(existsSync(host), 'Real Fabric host required; set FABRIC_ACP_TEST_HOST to an absolute built host. Never skipped.');
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fabric-acp-')));
  const registry = join(root, 'registry'), home = join(root, 'native-home'), repo = join(root, 'repo');
  const log = join(root, 'children.jsonl'), storage = join(root, 'storage');
  const children = new Set();
  t.after(async () => {
    await Promise.all([...children].map(child => new Promise(resolve => {
      child.once('close', resolve); child.kill('SIGTERM');
    })));
    for (const record of lines(log)) if (record.pid) {
      const command = spawnSync('/bin/ps', ['-o', 'command=', '-p', String(record.pid)], { encoding: 'utf8' }).stdout;
      if (command.includes(log)) {
        try { process.kill(record.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      }
    }
    remove(root);
  });
  mkdirSync(registry, { mode: 0o700 }); mkdirSync(home, { mode: 0o700 });
  const agent = join(root, 'turn-agent.mjs');
  writeFileSync(agent, readFileSync(join(source, 'plugins/acp/fixtures/turn-agent.mjs')));
  const config = { executable: { path: node, sha256: fileHash(node) }, args: [agent, log],
    nativeHome: home, cwd: 'fabric-workspace', pins: [{ path: agent, sha256: fileHash(agent) }], requestTimeoutMs: 2000 };
  const pkg = join(root, 'package');
  const snapshot = prepare(config, pkg);
  const second = prepare(config, join(root, 'second-package'));
  assert.equal(snapshot.hash, second.hash);
  assert.deepEqual(pack(snapshot), pack(second));
  assert.equal(archive(pack(snapshot)).hash, snapshot.hash);
  assert.throws(() => prepare({ ...config, env: { NODE_OPTIONS: 'unsafe' } }, join(root, 'invalid')));
  assert.throws(() => prepare({ ...config, executable: { path: node, sha256: '0'.repeat(64) } }, join(root, 'changed')));
  install(pkg, { registry, fabric: host, version: '0.1.0', sha256: snapshot.hash, trust: true });
  const bridgeDir = join(registry, 'packages', snapshot.hash);
  // Core's real local worker/target primitives, not an SDK-only scheduler mock.
  const target = create('target', join(root, 'local'));
  install(target, { registry, fabric: host, version: '1.0.0', sha256: directory(target).hash, trust: true });
  const env = { HOME: root, TMPDIR: root, LC_ALL: 'C', PATH: `${bridgeDir}:${dirname(node)}:/usr/bin:/bin`,
    FABRIC_PLUGIN_REGISTRY: registry, FABRIC_ACP_HOME: home,
    UNSELECTED_SECRET: 'SECRET_ENV_CANARY', NODE_OPTIONS: '--require=/SECRET_LOADER_CANARY',
    GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null' };
  function invoke(program, args, { cwd = root, success = true } = {}) {
    const result = spawnSync(program, args, { cwd, env, encoding: 'utf8', timeout: 60_000, maxBuffer: 2_097_152 });
    assert.ifError(result.error);
    if (success) assert.equal(result.status, 0, result.stdout + result.stderr);
    else assert.notEqual(result.status, 0, result.stdout + result.stderr);
    return result;
  }
  const args = values => ['--root', storage, ...values];
  const fabric = (values, options) => invoke(host, args(values), options);
  assert.deepEqual(JSON.parse(fabric(['plugins', '--registry', registry, 'check', 'acp']).stdout), { compatible: true, healthy: true });
  const doctor = JSON.parse(fabric(['harnesses', 'doctor', 'acp', '--json']).stdout);
  assert.equal(doctor.version, VERSION);
  assert.equal(doctor.manifest.capabilities.find(c => c.capability === 'usage.report').support, 'unsupported');
  assert.equal(doctor.manifest.capabilities.find(c => c.capability === 'model.override').support, 'unsupported');
  // Reuse the existing local commit: no commits, hooks, remotes or user storage are mutated.
  invoke('/usr/bin/git', ['clone', '--quiet', '--local', '--no-hardlinks', source, repo]);
  const original = readFileSync(join(repo, 'README.md'), 'utf8');
  const context = JSON.parse(fabric(['init', '--harness', 'acp', '--check', '--json'], { cwd: repo }).stdout).native_context;
  assert.equal(context.credential_available, false);
  assert.deepEqual(context.credential_paths, []);
  assert.deepEqual(context.profile.model_policy, { kind: 'native-default' });
  const connect = fabric(['harness', 'connect', 'acp', '--yes', '--json'], { success: false });
  assert.match(connect.stderr + connect.stdout, /unavailable or changed/);
  // Explicit reviewed profiles are an existing Core API; do not forge readiness to connect.
  const profile = join(root, 'profile.json');
  writeFileSync(profile, JSON.stringify(context.profile));
  fabric(['project', 'add', '--name', 'fixture', '--path', repo]);
  const recipe = join(root, 'recipe.toml'), promptFile = join(root, 'prompt.txt');
  writeFileSync(recipe, "version=1\n[[steps]]\nargv=['/usr/bin/true']\n");
  assert.deepEqual(lines(log), [], 'normal plugin inspection must not launch an ACP peer');
  const inspection = spawnSync(join(bridgeDir, 'fabric-acp-bridge'), ['--inspect', '--cwd', repo],
    { cwd: root, env: { HOME: root, LC_ALL: 'C' }, encoding: 'utf8', timeout: 15_000 });
  assert.ifError(inspection.error);
  assert.equal(inspection.status, 0, inspection.stdout + inspection.stderr);
  assert.equal(inspection.stderr, '');
  const initialized = JSON.parse(inspection.stdout);
  assert.equal(initialized.stage, 'negotiated');
  assert.equal(initialized.protocolVersion, 1);
  assert.equal(initialized.cleanup, 'reaped');
  assert.equal(initialized.launch.config_sha256, sha256(readFileSync(join(bridgeDir, 'launch.json'))));
  assert.doesNotMatch(inspection.stdout, /SECRET_|UNTRUSTED_/);
  assert.deepEqual(lines(log).map(record => record.method), ['initialize']);
  fabric(['project', 'prepare', 'fixture', '--recipe', recipe, '--json']);
  const runArgs = ['run', 'fixture', '--harness', 'acp', '--placement', 'local', '--profile', profile,
    '--prompt-file', promptFile, '--copy-limit-bytes', '67108864', '--timeout-seconds', '30', '--json'];
  function evidence(result, expected) {
    const report = JSON.parse(result.stdout);
    assert.equal(report.state, expected ? 'FAILED' : 'SUCCEEDED', result.stdout + result.stderr);
    assert.equal(report.capture.native_failure, expected);
    assert.equal(report.cleanup.action, 'reclaimed');
    assert.equal(existsSync(report.workspace_path), false);
    const { artifacts } = JSON.parse(fabric(['runs', 'artifacts', report.run_id, '--json']).stdout);
    const contents = new Map();
    for (const artifact of artifacts) {
      const bytes = readFileSync(join(storage, artifact.relative_path));
      assert.equal(bytes.length, artifact.size_bytes);
      assert.equal(sha256(bytes), artifact.sha256);
      contents.set(artifact.kind, bytes.toString('utf8'));
    }
    const events = contents.get('events').trim().split('\n').map(line => JSON.parse(line)).filter(record => record.event).map(record => record.event);
    assert.equal(events.some(event => event.type === 'usage' || event.type === 'tool'), false);
    assert.doesNotMatch(contents.get('stdout') + contents.get('stderr') + contents.get('events'), /SECRET_|UNTRUSTED_/);
    t.diagnostic(JSON.stringify({ host, package_sha256: snapshot.hash, run_id: report.run_id, state: report.state,
      native_failure: report.capture.native_failure, events_sha256: sha256(contents.get('events')) }));
    return { report, events, contents };
  }
  const prompt = `literal $(touch ${root}/MUST_NOT_EXIST); \"quoted\"\nUnicode: λ 😀`;
  writeFileSync(promptFile, prompt);
  const complete = evidence(fabric(runArgs), null);
  assert.deepEqual(complete.events.filter(event => event.type === 'output'), [{ type: 'output', stream: 'stdout', text: prompt }]);
  assert.match(complete.contents.get('tracked_patch'), /ACP fixture worker/);
  assert.equal(existsSync(join(root, 'MUST_NOT_EXIST')), false);
  assert.equal(readFileSync(join(repo, 'README.md'), 'utf8'), original);
  writeFileSync(promptFile, 'fixture:slow');
  const slow = evidence(fabric(runArgs), null);
  assert.deepEqual(slow.events.filter(event => event.type === 'output'),
    [{ type: 'output', stream: 'stdout', text: 'fixture:slow' }]);
  const failures = { malformed: 'acp_invalid_update', mismatch: 'acp_invalid_update', tool: 'acp_unsupported_update',
    image: 'acp_invalid_update', request: 'acp_unsupported_request', failure: 'acp_remote_error',
    stop: 'acp_invalid_stop', disconnect: 'acp_disconnected' };
  for (const [mode, code] of Object.entries(failures)) {
    writeFileSync(promptFile, `fixture:${mode}`);
    const proof = evidence(fabric(runArgs, { success: false }), code);
    assert.deepEqual(proof.events.filter(event => event.type === 'failure'), [{ type: 'failure', code, message: code }]);
  }
  const cancelledPrompt = `fixture:cancel\ncancel-only $(touch ${root}/MUST_NOT_EXIST); \"λ\"`;
  const survivorPrompt = `fixture:concurrent\nsurvivor-only $(touch ${root}/MUST_NOT_EXIST); \"😀\"`;
  function start(prompt, name) {
    const path = join(root, `${name}.txt`);
    writeFileSync(path, prompt);
    const values = runArgs.map(value => value === promptFile ? path : value);
    const child = spawn(host, args(values), { cwd: root, env, stdio: ['ignore', 'pipe', 'pipe'] });
    children.add(child);
    const run = { child, stdout: '', stderr: '' };
    child.stdout.on('data', bytes => { run.stdout += bytes; });
    child.stderr.on('data', bytes => { run.stderr += bytes; });
    run.closed = new Promise((resolve, reject) => {
      child.on('error', reject);
      child.on('close', status => { children.delete(child); resolve(status); });
    });
    return run;
  }
  const cancelling = start(cancelledPrompt, 'cancel'), surviving = start(survivorPrompt, 'survivor');
  const until = Date.now() + 20_000;
  while (!['cancel', 'concurrent'].every(mode => lines(log).some(record => record.mode === mode))) {
    assert.ok(Date.now() < until && cancelling.child.exitCode === null && surviving.child.exitCode === null,
      'Both installed-plugin fixture turns must reach pending prompts concurrently');
    await delay(20);
  }
  const [cancelPeer, survivorPeer] = ['cancel', 'concurrent'].map(mode => lines(log).find(record => record.mode === mode));
  assert.notEqual(cancelPeer.pid, survivorPeer.pid);
  assert.notEqual(cancelPeer.parent, survivorPeer.parent);
  assert.notEqual(cancelPeer.cwd, survivorPeer.cwd);
  assert.equal(cancelPeer.prompt, cancelledPrompt); assert.equal(survivorPeer.prompt, survivorPrompt);
  assert.ok(existsSync(cancelPeer.cwd) && existsSync(survivorPeer.cwd));
  process.kill(cancelPeer.pid, 0); process.kill(survivorPeer.pid, 0);
  cancelling.child.kill('SIGINT');
  assert.notEqual(await cancelling.closed, 0);
  const cancelled = JSON.parse(cancelling.stdout);
  assert.equal(cancelled.state, 'CANCELLED', cancelling.stdout + cancelling.stderr);
  assert.equal(cancelled.cleanup.action, 'reclaimed');
  assert.equal(cancelled.workspace_path, cancelPeer.cwd);
  assert.equal(existsSync(cancelPeer.cwd), false);
  reaped(cancelPeer.pid);
  assert.equal(surviving.child.exitCode, null, 'cancelling one host run stopped the other');
  process.kill(survivorPeer.pid, 0);
  assert.ok(existsSync(survivorPeer.cwd));
  writeFileSync(`${log}.release`, 'release the independently owned surviving turn');
  assert.equal(await surviving.closed, 0, surviving.stdout + surviving.stderr);
  const survivor = evidence(surviving, null);
  assert.notEqual(survivor.report.run_id, cancelled.run_id);
  assert.equal(survivor.report.workspace_path, survivorPeer.cwd);
  assert.deepEqual(survivor.events.filter(event => event.type === 'output'),
    [{ type: 'output', stream: 'stdout', text: survivorPrompt }]);
  assert.ok(survivor.contents.get('tracked_patch').includes('survivor-only'));
  assert.equal(survivor.contents.get('tracked_patch').includes('cancel-only'), false);
  assert.equal(existsSync(join(root, 'MUST_NOT_EXIST')), false);
  assert.equal(readFileSync(join(repo, 'README.md'), 'utf8'), original);
  reaped(survivorPeer.pid);
  t.diagnostic(JSON.stringify({ cancelled_run: cancelled.run_id, survivor_run: survivor.report.run_id,
    cancellation: 'SIGINT with both independent installed-plugin turns active', cleanup: 'both reclaimed' }));
  for (const record of lines(log).filter(record => record.pid)) {
    assert.equal(record.group, record.parentGroup, 'ACP child escaped bridge process group');
    assert.equal(record.home, home);
    // Darwin adds this key even to `env -i ... node`; it is not inherited consent.
    const environment = record.environment.filter(key => process.platform !== 'darwin' || key !== '__CF_USER_TEXT_ENCODING');
    assert.deepEqual(environment, ['HOME', 'LC_ALL']);
    assert.notEqual(record.cwd, repo);
    assert.ok(record.cwd.startsWith(storage + '/'));
    reaped(record.pid);
  }
  // Revalidates content identity before another launch, without ever running a replacement.
  const count = lines(log).filter(record => record.pid).length;
  writeFileSync(agent, 'throw new Error(\"must never execute replacement\");');
  const changed = fabric(['harnesses', 'doctor', 'acp', '--json'], { success: false });
  assert.doesNotMatch(changed.stdout + changed.stderr, /must never execute replacement/);
  assert.equal(lines(log).filter(record => record.pid).length, count);
});
