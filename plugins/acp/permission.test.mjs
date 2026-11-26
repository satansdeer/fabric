import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createConnection } from 'node:net';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, readdirSync, existsSync,
  realpathSync, rmSync, chmodSync, symlinkSync, renameSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { prepare } from './package.mjs';
import { fileHash, permissionDirectory } from './config.mjs';
import { permissionControl } from './permission.mjs';

const node = realpathSync(process.execPath);
const fixture = realpathSync(fileURLToPath(new URL('./fixtures/turn-agent.mjs', import.meta.url)));
const records = log => existsSync(log) ? readFileSync(log, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse) : [];
async function until(check) {
  for (let i = 0; i < 500; i++) {
    const result = await check();
    if (result) return result;
    await delay(10);
  }
  assert.fail('fixture did not reach expected state');
}
function decision(list, kind = 'allow_once', optionId = 'once') {
  const item = list.pending[0];
  return { op: 'decide', endpoint: list.endpoint, nonce: list.nonce, sessionId: item.sessionId,
    requestId: item.requestId, digest: item.digest, kind, optionId };
}

// Real packaged bridge/companion and deterministic fake peers; no Core host or native provider.
test('owned ACP permission control regressions', { timeout: 120_000 }, async t => {
  // Short canonical paths also exercise the macOS sockaddr_un limit.
  const root = realpathSync(mkdtempSync('/tmp/acp-p-'));
  const control = join(root, 'control'), workspace = join(root, 'workspace'), home = join(root, 'home');
  for (const path of [control, workspace, home]) mkdirSync(path, { mode: 0o700 });
  const active = new Set();
  t.after(async () => {
    for (const turn of active) turn.child.kill('SIGTERM');
    await Promise.all([...active].map(turn => turn.closed));
    rmSync(root, { recursive: true, force: true });
  });
  const base = { executable: { path: node, sha256: fileHash(node) }, args: [fixture], nativeHome: home,
    cwd: 'fabric-workspace', pins: [{ path: fixture, sha256: fileHash(fixture) }], requestTimeoutMs: 2000 };
  let sequence = 0;
  function start(mode = 'permission-normal', selected = true, directory = control, beforeSpawn = () => {}) {
    const number = sequence++, log = join(root, `peer-${number}.jsonl`), pkg = join(root, `pkg-${number}`);
    prepare({ ...base, args: [fixture, log], ...(selected ? { permission: { directory } } : {}) }, pkg);
    beforeSpawn();
    const child = spawn(join(pkg, 'fabric-acp-bridge'), ['--turn', `fixture:${mode}`],
      { cwd: workspace, env: { HOME: home, LC_ALL: 'C' }, stdio: ['ignore', 'pipe', 'pipe'] });
    const turn = { child, log, pkg, stdout: '', stderr: '' };
    child.stdout.on('data', bytes => { turn.stdout += bytes; });
    child.stderr.on('data', bytes => { turn.stderr += bytes; });
    turn.closed = new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('close', (code, signal) => { active.delete(turn); resolve({ code, signal }); });
    });
    active.add(turn);
    return turn;
  }
  async function review(turn) {
    await until(() => records(turn.log).some(record => record.mode?.startsWith('permission-')));
    const endpoints = readdirSync(control).filter(name => name.endsWith('.sock'));
    for (const name of endpoints) {
      const list = await permissionControl(join(control, name), { op: 'list' });
      if (list.pending.length) return list;
    }
    return until(async () => {
      for (const name of readdirSync(control).filter(name => name.endsWith('.sock'))) {
        const list = await permissionControl(join(control, name), { op: 'list' });
        if (list.pending.length) return list;
      }
    });
  }
  async function finished(turn, expected = 0, endpoint) {
    assert.equal((await turn.closed).code, expected, turn.stdout + turn.stderr);
    assert.equal(turn.stderr, '');
    assert.doesNotMatch(turn.stdout, /PRIVATE_|permission-0|connection-local|\.sock|nonce|rawInput/);
    if (endpoint) assert.equal(existsSync(endpoint), false);
    const pid = records(turn.log).find(record => record.pid)?.pid;
    if (pid) assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
  }

  await t.test('pending stays inert; explicit companion allow-once executes exactly once', async () => {
    const turn = start(), list = await review(turn);
    assert.equal(existsSync(`${turn.log}.executed`), false);
    const companion = join(turn.pkg, 'fabric-acp-permission');
    const inspected = spawnSync(companion, ['list', list.endpoint], { encoding: 'utf8', timeout: 5000 });
    assert.equal(inspected.status, 0, inspected.stderr);
    assert.match(inspected.stdout, /PRIVATE_PERMISSION_COMMAND/);
    assert.doesNotMatch(inspected.stdout, /[\u007f-\u009f\u202a-\u202e\u2066-\u2069]/);
    assert.equal(JSON.parse(inspected.stdout).pending[0].request.toolCall.title, 'PRIVATE_PERMISSION_TITLE\u009b31m\u202e');
    assert.equal(JSON.parse(inspected.stdout).pending[0].digest, list.pending[0].digest);
    assert.equal(existsSync(`${turn.log}.executed`), false);
    const path = join(root, 'decision.json');
    writeFileSync(path, JSON.stringify(decision(list)), { mode: 0o600 });
    const allowed = spawnSync(companion, ['decide', list.endpoint, path], { encoding: 'utf8', timeout: 5000 });
    assert.equal(allowed.status, 0, allowed.stderr);
    assert.deepEqual(JSON.parse(allowed.stdout), { ok: true });
    await finished(turn, 0, list.endpoint);
    assert.equal(readFileSync(`${turn.log}.executed`, 'utf8'), 'executed\n');
    assert.equal(records(turn.log).filter(record => record.response?.result).length, 1);
  });

  await t.test('invalid decisions do not consume the pending request; reject-once never executes', async () => {
    const turn = start(), list = await review(turn), valid = decision(list);
    for (const wrong of [
      { nonce: 'wrong' }, { endpoint: '/wrong' }, { requestId: 'unknown' }, { sessionId: 'other-session' },
      { digest: '0'.repeat(64) }, { optionId: 'unknown' }, { optionId: 'no' },
      { optionId: 'always', kind: 'allow_always' }, { kind: 'reject_once' }, { extra: true },
    ]) {
      assert.deepEqual(await permissionControl(list.endpoint, { ...valid, ...wrong }), { error: 'invalid_decision' });
      assert.equal((await permissionControl(list.endpoint, { op: 'list' })).pending[0].digest, valid.digest);
      assert.equal(existsSync(`${turn.log}.executed`), false);
    }
    assert.deepEqual(await permissionControl(list.endpoint, decision(list, 'reject_once', 'no')), { ok: true });
    await finished(turn, 0, list.endpoint);
    assert.equal(existsSync(`${turn.log}.executed`), false);
    await assert.rejects(permissionControl(list.endpoint, valid));
  });

  await t.test('replayed or stale reviewed decisions cannot grant a second pending request', async () => {
    const turn = start('permission-two');
    let list = await review(turn);
    list = await until(async () => {
      const current = await permissionControl(list.endpoint, { op: 'list' });
      if (current.pending.length === 2) return current;
    });
    const first = decision(list);
    assert.deepEqual(await permissionControl(list.endpoint, first), { ok: true });
    assert.deepEqual(await permissionControl(list.endpoint, first), { error: 'invalid_decision' });
    const next = await permissionControl(list.endpoint, { op: 'list' });
    assert.equal(next.pending.length, 1);
    const second = decision(next, 'reject_once', 'no');
    assert.deepEqual(await permissionControl(list.endpoint, { ...second, digest: first.digest }), { error: 'invalid_decision' });
    assert.deepEqual(await permissionControl(list.endpoint, second), { ok: true });
    await finished(turn, 0, list.endpoint);
    assert.equal(readFileSync(`${turn.log}.executed`, 'utf8'), 'executed\n');
  });

  await t.test('owned abort cleans pending clients and endpoint, without affecting an independent turn', async () => {
    const first = start(), a = await review(first);
    const second = start();
    await until(() => records(second.log).some(record => record.mode === 'permission-normal'));
    const b = await until(async () => {
      const name = readdirSync(control).find(name => join(control, name) !== a.endpoint);
      if (!name) return;
      const list = await permissionControl(join(control, name), { op: 'list' });
      if (list.pending.length) return list;
    });
    assert.notEqual(a.endpoint, b.endpoint);
    assert.notEqual(a.nonce, b.nonce);
    assert.deepEqual(await permissionControl(b.endpoint, decision(a)), { error: 'invalid_decision' });
    const socket = createConnection(a.endpoint);
    await new Promise(resolve => socket.once('connect', resolve));
    const socketClosed = new Promise(resolve => socket.once('close', resolve));
    first.child.kill('SIGTERM');
    await finished(first, 1, a.endpoint);
    await socketClosed;
    assert.equal(records(first.log).some(record => record.method === 'session/cancel'), true);
    assert.equal(existsSync(`${first.log}.executed`), false);
    await assert.rejects(permissionControl(a.endpoint, decision(a)));
    assert.equal((await permissionControl(b.endpoint, { op: 'list' })).pending.length, 1);
    assert.deepEqual(await permissionControl(b.endpoint, decision(b)), { ok: true });
    await finished(second, 0, b.endpoint);
    assert.equal(readFileSync(`${second.log}.executed`, 'utf8'), 'executed\n');
  });

  await t.test('malformed control frames and bounded client overload cannot approve', async () => {
    const turn = start(), list = await review(turn);
    async function raw(bytes) {
      const socket = createConnection(list.endpoint);
      const chunks = [];
      socket.on('data', chunk => chunks.push(chunk));
      await new Promise((resolve, reject) => {
        socket.on('error', reject);
        socket.on('close', resolve);
        socket.on('connect', () => socket.write(bytes));
      });
      return Buffer.concat(chunks).toString();
    }
    assert.deepEqual(JSON.parse(await raw('{broken\n')), { error: 'invalid_message' });
    assert.deepEqual(JSON.parse(await raw('{}\n{}\n')), { error: 'invalid_message' });
    assert.equal(await raw('x'.repeat(4097)), '');
    const holders = [];
    try {
      for (let i = 0; i < 8; i++) {
        const socket = createConnection(list.endpoint);
        await new Promise((resolve, reject) => { socket.once('connect', resolve); socket.once('error', reject); });
        holders.push(socket);
      }
      await assert.rejects(permissionControl(list.endpoint, decision(list)));
      assert.equal(existsSync(`${turn.log}.executed`), false);
    } finally {
      await Promise.all(holders.map(socket => new Promise(resolve => { socket.once('close', resolve); socket.destroy(); })));
    }
    await until(async () => {
      try { return (await permissionControl(list.endpoint, { op: 'list' })).pending.length === 1; } catch { return false; }
    });
    assert.deepEqual(await permissionControl(list.endpoint, decision(list, 'reject_once', 'no')), { ok: true });
    await finished(turn, 0, list.endpoint);
    assert.equal(existsSync(`${turn.log}.executed`), false);
  });

  await t.test('malformed, duplicate, wrong-session, late and overflowing peer requests fail closed', async () => {
    for (const mode of ['malformed', 'shape', 'oversize', 'options', 'session', 'duplicate', 'tool-duplicate', 'late', 'early-stop', 'overflow']) {
      const turn = start(`permission-${mode}`);
      await finished(turn, 1);
      assert.match(turn.stdout, /acp_invalid_permission/);
      assert.equal(existsSync(`${turn.log}.executed`), false);
      assert.deepEqual(readdirSync(control), []);
    }
  });

  await t.test('no selection and unknown interactive methods retain unsupported-request failure', async () => {
    for (const [mode, selected] of [['permission-normal', false], ['tool-fs-request', true],
      ['tool-terminal-request', true], ['vendor-request', true]]) {
      const turn = start(mode, selected);
      await finished(turn, 1);
      assert.match(turn.stdout, /acp_unsupported_request/);
      assert.equal(existsSync(`${turn.log}.executed`), false);
      assert.deepEqual(readdirSync(control), []);
    }
  });

  await t.test('directory pin rejects symlinks, public access, replacement, workspace and overlong paths', async () => {
    const link = join(root, 'link');
    symlinkSync(control, link);
    assert.throws(() => permissionDirectory(link));
    chmodSync(control, 0o750);
    assert.throws(() => permissionDirectory(control));
    chmodSync(control, 0o700);
    assert.throws(() => permissionDirectory(workspace, workspace));
    const long = join(root, 'x'.repeat(80));
    mkdirSync(long, { mode: 0o700 });
    assert.throws(() => permissionDirectory(long));
    const inWorkspace = start('permission-normal', true, workspace);
    await finished(inWorkspace, 1);
    assert.equal(existsSync(inWorkspace.log), false);
    // Packaging pins inode/device, not only a reusable pathname.
    const pinned = start('permission-normal', true, control, () => {
      renameSync(control, join(root, 'old-control'));
      mkdirSync(control, { mode: 0o700 });
    });
    await finished(pinned, 1);
    assert.equal(existsSync(pinned.log), false);
    assert.deepEqual(readdirSync(control), []);
  });
});
