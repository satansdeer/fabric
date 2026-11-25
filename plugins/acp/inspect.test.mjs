import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync, realpathSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { prepare } from './package.mjs';
import { fileHash, sha256, VERSION } from './config.mjs';

const node = realpathSync(process.execPath), nodeHash = fileHash(node);
const literal = '$(touch MUST_NOT_EXIST); "literal" λ';
const records = path => existsSync(path) ? readFileSync(path, 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line)) : [];
function fixture(t, mode = 'normal') {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'acp-inspect-')));
  const home = join(root, 'home'), cwd = join(root, 'workspace'), agent = join(root, 'agent.mjs'), log = join(root, 'log');
  mkdirSync(home); mkdirSync(cwd);
  writeFileSync(agent, readFileSync(new URL('./fixtures/inspect-agent.mjs', import.meta.url)));
  const pkg = join(root, 'package');
  prepare({ executable: { path: node, sha256: nodeHash }, args: [agent, log, mode, literal],
    nativeHome: home, cwd: 'fabric-workspace', pins: [{ path: agent, sha256: fileHash(agent) }], requestTimeoutMs: 500 }, pkg);
  const active = new Set();
  t.after(async () => {
    for (const run of active) { run.child.kill('SIGTERM'); await run.closed; }
    for (const record of records(log)) if (record.pid) {
      const command = spawnSync('/bin/ps', ['-o', 'command=', '-p', String(record.pid)], { encoding: 'utf8' });
      if (command.status === 0 && command.stdout.includes(log)) {
        try { process.kill(record.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      }
    }
    rmSync(root, { recursive: true, force: true });
  });
  function start(args = ['--cwd', cwd]) {
    const child = spawn(join(pkg, 'fabric-acp-bridge'), ['--inspect', ...args], { cwd: root,
      env: { HOME: root, LC_ALL: 'C', PATH: '/SECRET_PATH_CANARY', UNSELECTED_SECRET: 'SECRET_ENV_CANARY' },
      stdio: ['ignore', 'pipe', 'pipe'] });
    const run = { child, stdout: '', stderr: '' };
    child.stdout.on('data', bytes => { run.stdout += bytes; });
    child.stderr.on('data', bytes => { run.stderr += bytes; });
    run.closed = new Promise((resolve, reject) => { child.on('error', reject); child.on('close', status => { active.delete(run); resolve(status); }); });
    active.add(run);
    return run;
  }
  function result(run, expected) {
    assert.equal(run.stderr, '');
    assert.ok(Buffer.byteLength(run.stdout) < 4096);
    assert.equal(run.stdout.trim().split('\n').length, 1);
    assert.doesNotMatch(run.stdout, /SECRET_|UNTRUSTED_|MUST_NOT_EXIST/);
    for (const path of [root, home, cwd, agent, node]) assert.equal(run.stdout.includes(path), false);
    const diagnostic = JSON.parse(run.stdout);
    assert.equal(diagnostic.stage, expected ? 'failed' : 'negotiated');
    assert.equal(diagnostic.code, expected ?? 'acp_initialized');
    assert.equal(diagnostic.auth, 'unverified'); assert.equal(diagnostic.model, 'unverified');
    assert.equal(diagnostic.peer_identity, 'untrusted');
    const launched = records(log).filter(record => record.pid);
    assert.equal(diagnostic.cleanup, launched.length ? 'reaped' : 'not_started');
    for (const record of launched) {
      assert.throws(() => process.kill(record.pid, 0), { code: 'ESRCH' }, 'diagnostic preceded owned reaping');
      assert.equal(record.cwd, cwd); assert.equal(record.home, home); assert.equal(record.literal, literal);
      assert.deepEqual(record.environment.filter(key => process.platform !== 'darwin' || key !== '__CF_USER_TEXT_ENCODING'), ['HOME', 'LC_ALL']);
    }
    assert.deepEqual(records(log).filter(record => Object.hasOwn(record, 'method')).map(record => record.method), launched.length ? ['initialize'] : []);
    for (const { response } of records(log).filter(record => record.response)) {
      assert.deepEqual(response, { jsonrpc: '2.0', id: mode === 'late-request' ? 'late' : 'blocking',
        error: { code: -32601, message: 'Method not found' } });
    }
    assert.equal(existsSync(join(root, 'MUST_NOT_EXIST')), false);
    assert.equal(existsSync(join(cwd, 'MUST_NOT_EXIST')), false);
    return diagnostic;
  }
  async function until(run, predicate) {
    const end = Date.now() + 5000;
    while (!predicate(records(log))) {
      assert.ok(Date.now() < end && run.child.exitCode === null && run.child.signalCode === null, 'fixture did not reach owned state');
      await delay(5);
    }
  }
  return { root, cwd, agent, log, pkg, start, result, until };
}

test('packaged initialize-only command: safe evidence and success only after reaping', { timeout: 10_000 }, async t => {
  const f = fixture(t, 'ignore-term'), run = f.start();
  await f.until(run, entries => entries.some(entry => entry.terminating));
  assert.equal(run.stdout, '', 'success emitted before TERM-ignoring child was reaped');
  assert.equal(await run.closed, 0);
  const result = f.result(run);
  assert.equal(result.protocolVersion, 1);
  assert.deepEqual(result.launch, { bridge: VERSION, config_sha256: sha256(readFileSync(join(f.pkg, 'launch.json'))),
    executable_sha256: nodeHash, runtime_sha256: nodeHash, resource_sha256: [fileHash(f.agent)] });
});

test('initialize failures are bounded, sanitized and reaped, including exit zero and blocking requests', { timeout: 30_000 }, async t => {
  const failures = { error: 'acp_initialize_failed', protocol: 'acp_unsupported_protocol', malformed: 'acp_malformed_frame',
    oversized: 'acp_frame_too_large', notifications: 'acp_callback_overflow', request: 'acp_unsupported_request',
    'late-request': 'acp_unsupported_request', exit: 'acp_disconnected', hang: 'acp_deadline' };
  for (const [mode, code] of Object.entries(failures)) await t.test(mode, async t => {
    const f = fixture(t, mode), run = f.start();
    assert.equal(await run.closed, 1);
    const result = f.result(run, code);
    if (mode !== 'late-request') assert.equal(Object.hasOwn(result, 'protocolVersion'), false);
  });
});

test('SIGINT/SIGTERM during initialize and teardown cannot report success or leave children', { timeout: 15_000 }, async t => {
  for (const mode of ['hang', 'ignore-term']) for (const signal of ['SIGINT', 'SIGTERM']) await t.test(`${mode} ${signal}`, async t => {
    const f = fixture(t, mode), run = f.start();
    await f.until(run, entries => entries.some(entry => mode === 'hang' ? entry.method === 'initialize' : entry.terminating));
    assert.equal(run.stdout, '');
    run.child.kill(signal);
    assert.equal(await run.closed, 1);
    f.result(run, 'acp_aborted');
  });
});

test('missing/extra/invalid input and changed launch pins fail before execution', { timeout: 20_000 }, async t => {
  const f = fixture(t);
  const alias = join(f.root, 'alias'); symlinkSync(f.cwd, alias);
  for (const args of [[], ['--cwd'], ['--cwd', f.cwd, 'extra'], ['--other', f.cwd], ['--cwd', '.'],
    ['--cwd', alias], ['--cwd', f.agent], ['--cwd', join(f.root, 'missing')]]) {
    const run = f.start(args); assert.equal(await run.closed, 1);
    const result = f.result(run, args.length === 2 && args[0] === '--cwd' ? (args[1].endsWith('/missing') ? 'acp_inspection_failed' : 'acp_unsafe_path') : 'acp_inspect_arguments');
    assert.equal(result.launch, null);
  }
  const configPath = join(f.pkg, 'launch.json'), original = readFileSync(configPath);
  for (const kind of ['runtime', 'executable', 'resource', 'extra', 'missing', 'oversized']) {
    const config = JSON.parse(original);
    if (kind === 'runtime' || kind === 'executable') config[kind].sha256 = '0'.repeat(64);
    if (kind === 'extra') config.env = { SECRET_CONFIG_CANARY: 'secret' };
    if (kind === 'missing') delete config.args;
    if (kind === 'resource') writeFileSync(f.agent, "throw new Error('SECRET_REPLACEMENT_CANARY');");
    writeFileSync(configPath, kind === 'oversized' ? 'x'.repeat(32_769) : JSON.stringify(config));
    const run = f.start(); assert.equal(await run.closed, 1);
    f.result(run, ['runtime', 'executable', 'resource'].includes(kind) ? 'acp_launch_changed' : kind === 'oversized' ? 'acp_inspection_failed' : 'acp_invalid_config');
    assert.equal(existsSync(f.log), false, 'changed or invalid pins executed the child');
    if (kind === 'resource') writeFileSync(f.agent, readFileSync(new URL('./fixtures/inspect-agent.mjs', import.meta.url)));
  }
});
