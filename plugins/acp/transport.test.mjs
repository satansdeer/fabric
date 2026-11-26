import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, realpath, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { launchAcp } from './transport.mjs';

const fixture = fileURLToPath(new URL('./fixtures/agent.mjs', import.meta.url));
const deferred = () => {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
};
const code = expected => failure => {
  assert.equal(failure.code, expected);
  assert.equal(failure.message, expected);
  assert.equal(failure.cause, undefined);
  assert.ok(!String(failure.stack).includes('PRIVATE_CHILD_TEXT'));
  return true;
};
async function launch(t, mode = 'normal', options = {}, value) {
  const cwd = await mkdtemp(join(tmpdir(), 'fabric-acp-'));
  const notifications = [];
  const connection = launchAcp({
    executable: process.execPath, args: [fixture, mode, JSON.stringify(value ?? null)],
    cwd, env: { ACP_FIXTURE: 'literal' }, onNotification: value => { notifications.push(value); },
    requestTimeoutMs: 2000, initializeTimeoutMs: 2000, terminationGraceMs: 30, ...options,
  });
  t.after(async () => { await connection.close(); await rm(cwd, { recursive: true, force: true }); });
  return { connection, notifications, cwd };
}
function reaped(connection) {
  if (connection.pid !== undefined) assert.throws(() => process.kill(connection.pid, 0), { code: 'ESRCH' });
}
async function stopped(connection, expected) {
  assert.deepEqual(await connection.closed, { code: expected });
  reaped(connection);
  await assert.rejects(connection.request('echo'), code(expected));
}

test('negotiation is nonbilling, literal launch is isolated, notifications remain distinct', { timeout: 10_000 }, async t => {
  const parentKey = `FABRIC_ACP_TEST_PARENT_${process.pid}`;
  assert.equal(Object.hasOwn(process.env, parentKey), false);
  process.env[parentKey] = 'parent-only';
  t.after(() => { delete process.env[parentKey]; });
  const { connection, notifications, cwd } = await launch(t);
  await assert.rejects(connection.request('session/new'), code('acp_not_ready'));
  const initialization = connection.initialize();
  assert.equal(connection.initialize(), initialization);
  const metadata = await initialization;
  assert.deepEqual(metadata, {
    protocolVersion: 1,
    agentCapabilities: { loadSession: true, promptCapabilities: { image: true }, sessionCapabilities: { list: {} } },
    authMethods: [{ id: 'local', name: 'Local metadata', description: null }],
    agentInfo: { name: 'fixture', version: '1', title: '界 🐎' },
  });
  assert.deepEqual(await connection.request('initialize-seen'), {
    jsonrpc: '2.0', id: 1, method: 'initialize', params: {
      protocolVersion: 1, clientCapabilities: {}, clientInfo: { name: 'fabric-acp-transport', version: '0.1.0' },
    },
  });
  const launchInfo = await connection.request('inspect-initialize', { parentKey });
  assert.equal(launchInfo.explicit, 'literal');
  assert.equal(launchInfo.inherited, false);
  assert.equal(await realpath(launchInfo.cwd), await realpath(cwd));
  assert.deepEqual(launchInfo.args, ['normal', 'null']);
  const literal = '$(false); | & " \' \\ 🐎';
  assert.deepEqual(await connection.request('echo', { literal }), { literal });
  assert.equal(await connection.request('fragments'), 'naïve 🐎');
  assert.deepEqual(notifications.shift(), { jsonrpc: '2.0', method: 'session/update', params: { text: '界🐎' } });
  await connection.request('unknown-notification');
  assert.deepEqual(notifications.shift(), {
    jsonrpc: '2.0', method: 'unknown/capabilities', params: { terminal: true, fs: { writeTextFile: true } },
  });
  connection.notify('session/cancel', { sessionId: 'explicit' });
  assert.deepEqual(await connection.request('last-notification'), {
    jsonrpc: '2.0', method: 'session/cancel', params: { sessionId: 'explicit' },
  });
  await connection.close();
  await stopped(connection, 'acp_closed');
});

test('out-of-order calls and two simultaneous independent connections', { timeout: 10_000 }, async t => {
  const [{ connection: first }, { connection: second }] = await Promise.all([launch(t), launch(t)]);
  await Promise.all([first.initialize(), second.initialize()]);
  assert.notEqual(first.pid, second.pid);
  const result = await Promise.all([
    first.request('reverse', { value: 'first-a' }), first.request('reverse', { value: 'first-b' }),
    second.request('reverse', { value: 'second-a' }), second.request('reverse', { value: 'second-b' }),
  ]);
  assert.deepEqual(result, ['first-a', 'first-b', 'second-a', 'second-b'].map(value => ({ value })));
  await first.cancel();
  await stopped(first, 'acp_aborted');
  assert.deepEqual(await second.request('echo', { alive: true }), { alive: true });
  await second.close();
  reaped(second);
});

test('unsupported blocking requests get method-not-found, never permissions or fake answers', { timeout: 10_000 }, async t => {
  const { connection } = await launch(t);
  await connection.initialize();
  const answers = await connection.request('blocking');
  assert.deepEqual(answers, ['ask', 1, 'permission', 'terminal'].map(id => ({
    jsonrpc: '2.0', id, error: { code: -32601, message: 'Method not found' },
  })));
});

const invalidNegotiations = [
  [null, 'acp_invalid_initialize'],
  [{ protocolVersion: 2 }, 'acp_unsupported_protocol'],
  [{ protocolVersion: '1' }, 'acp_invalid_initialize'],
  ...[
    { agentCapabilities: null }, { agentCapabilities: [] }, { agentCapabilities: { loadSession: 'yes' } },
    { agentCapabilities: { promptCapabilities: null } }, { agentCapabilities: { promptCapabilities: { image: 1 } } },
    { agentCapabilities: { mcpCapabilities: { http: [] } } }, { agentCapabilities: { sessionCapabilities: { list: true } } },
    { agentCapabilities: { auth: { logout: false } } },
    { agentCapabilities: { mcpCapabilities: { acp: {} } } }, { agentCapabilities: { providers: true } },
    { agentCapabilities: { auth: { status: [] } } },
    ...['fork', 'options', 'status', 'title'].map(key => ({ agentCapabilities: { sessionCapabilities: { [key]: true } } })),
    { agentCapabilities: { sessionCapabilities: { systemPrompt: { mode: { override: false } } } } },
    { agentCapabilities: { unknown: true } }, { agentCapabilities: { _meta: [] } },
    { authMethods: null }, { authMethods: {} }, { authMethods: [null] }, { authMethods: [{ id: 'x' }] },
    { authMethods: [{ id: 'x', name: 3 }] }, { authMethods: [{ id: 'x', name: 'n', description: false }] },
    { authMethods: [{ id: 'x', name: 'n', type: 'terminal', args: ['login'] }] },
    { authMethods: [{ id: 'x', name: 'a' }, { id: 'x', name: 'b' }] },
    { agentInfo: { name: 'n' } }, { agentInfo: { name: 'n', version: 1 } }, { agentInfo: [] },
  ].map(fields => [{ protocolVersion: 1, ...fields }, 'acp_invalid_initialize']),
];
test('unsupported protocol and malformed negotiation fail closed and reap', { timeout: 20_000 }, async t => {
  for (const [index, [value, expected]] of invalidNegotiations.entries()) {
    await t.test(`negotiation ${index}`, async t => {
      const { connection } = await launch(t, 'init-value', {}, value);
      await assert.rejects(connection.initialize(), code(expected));
      await stopped(connection, expected);
    });
  }
});

test('omitted fields mean no optional support; valid nullable and extension metadata is inert', { timeout: 10_000 }, async t => {
  for (const value of [{ protocolVersion: 1 }, {
    protocolVersion: 1,
    agentCapabilities: { auth: { logout: {} }, sessionCapabilities: { close: null, systemPrompt: { mode: { override: {} } } }, _meta: { terminal: true } },
    authMethods: [{ id: 'x', name: 'Untrusted', type: 'agent' }], agentInfo: null,
  }, {
    protocolVersion: 1,
    agentCapabilities: {
      loadSession: true, promptCapabilities: { image: true, embeddedContext: true },
      mcpCapabilities: { http: true, sse: true, acp: true },
      auth: { logout: {}, status: {} }, providers: {},
      sessionCapabilities: { additionalDirectories: {}, close: {}, delete: {}, fork: {}, list: {},
        options: {}, resume: {}, status: {}, systemPrompt: { mode: { override: {} } }, title: {} },
      _meta: { claudeCode: { promptQueueing: true }, authStatus: {} },
    },
  }]) {
    const { connection } = await launch(t, 'init-value', {}, value);
    const metadata = await connection.initialize();
    assert.deepEqual(metadata.agentCapabilities, value.agentCapabilities ?? {});
    assert.deepEqual(metadata.authMethods, value.authMethods ?? []);
    assert.equal(metadata.agentInfo, null);
    // Even agent capability claims cannot enable a client operation.
    assert.equal((await connection.request('blocking'))[3].error.code, -32601);
  }
});

const faults = [
  ['json', 'acp_malformed_frame'], ['utf8', 'acp_malformed_frame'], ['oversized', 'acp_frame_too_large'],
  ['truncated', 'acp_truncated_frame'], ['eof', 'acp_disconnected'], ['exit', 'acp_disconnected'],
  ['bad-error', 'acp_invalid_envelope'], ['mixed', 'acp_invalid_envelope'], ['both', 'acp_invalid_envelope'],
  ['array', 'acp_invalid_envelope'], ['params', 'acp_invalid_envelope'], ['request-id', 'acp_invalid_envelope'],
  ['extra', 'acp_invalid_envelope'],
  ...[null, false, 1.5, 99999, '2'].map(id => ['remote-id', 'acp_invalid_response_id', id]),
];
test('bad frames and EOF reject every pending call coherently without child diagnostics', { timeout: 20_000 }, async t => {
  for (const [index, [kind, expected, id]] of faults.entries()) {
    await t.test(`${index}: ${kind}`, async t => {
      const { connection } = await launch(t, 'normal', { maxRecordBytes: 512 });
      await connection.initialize();
      const waiting = connection.request('hang');
      const fault = connection.request('fault', { kind, id, bytes: 513 });
      const results = await Promise.allSettled([waiting, fault]);
      assert.equal(results[0].status, 'rejected');
      assert.equal(results[1].status, 'rejected');
      assert.equal(results[0].reason, results[1].reason);
      code(expected)(results[0].reason);
      await stopped(connection, expected);
    });
  }
});

test('duplicate replies are fatal even after a matching reply', { timeout: 10_000 }, async t => {
  const { connection } = await launch(t);
  await connection.initialize();
  const waiting = connection.request('hang');
  const check = assert.rejects(waiting, code('acp_invalid_response_id'));
  assert.equal(await connection.request('fault', { kind: 'duplicate' }), 'first');
  await check;
  await stopped(connection, 'acp_invalid_response_id');
});

test('remote errors are sanitized, with no retries; initialize errors terminate', { timeout: 10_000 }, async t => {
  const { connection } = await launch(t);
  await connection.initialize();
  await assert.rejects(connection.request('remote-error'), code('acp_remote_error'));
  assert.deepEqual(await connection.request('echo', { usable: true }), { usable: true });
  const { connection: failed } = await launch(t, 'init-error');
  await assert.rejects(failed.initialize(), code('acp_initialize_failed'));
  await stopped(failed, 'acp_initialize_failed');
});

test('initialization deadline, abort and close terminate even a TERM-ignoring child', { timeout: 10_000 }, async t => {
  for (const action of ['deadline', 'abort', 'close', 'cancel']) {
    const started = deferred();
    const controller = new AbortController();
    const { connection } = await launch(t, 'init-hang', {
      initializeTimeoutMs: action === 'deadline' ? 100 : 2000,
      onNotification: () => started.resolve(),
    });
    const expected = action === 'deadline' ? 'acp_deadline' : action === 'close' ? 'acp_closed' : 'acp_aborted';
    const check = assert.rejects(connection.initialize({ signal: controller.signal }), code(expected));
    if (action !== 'deadline') {
      await started.promise;
      if (action === 'abort') controller.abort('PRIVATE_CHILD_TEXT');
      else await connection[action]();
    }
    await check;
    await stopped(connection, expected);
  }
  const { connection } = await launch(t, 'ignore-term');
  await connection.initialize();
  await connection.close();
  reaped(connection);
});

test('pending requests: deadline, per-call abort, launch abort, cancel and close', { timeout: 10_000 }, async t => {
  for (const action of ['deadline', 'abort', 'launch-abort', 'cancel', 'close']) {
    const started = deferred();
    const launchController = new AbortController();
    const controller = new AbortController();
    const { connection } = await launch(t, 'normal', {
      signal: launchController.signal, onNotification: () => started.resolve(),
    });
    await connection.initialize();
    const expected = action === 'deadline' ? 'acp_deadline' : action === 'close' ? 'acp_closed' : 'acp_aborted';
    const first = connection.request('hang', {}, { signal: controller.signal, timeoutMs: action === 'deadline' ? 50 : 2000 });
    const second = connection.request('hang');
    const checks = Promise.all([assert.rejects(first, code(expected)), assert.rejects(second, code(expected))]);
    await started.promise;
    if (action === 'abort') controller.abort();
    if (action === 'launch-abort') launchController.abort();
    if (action === 'cancel' || action === 'close') await connection[action]();
    await checks;
    await stopped(connection, expected);
  }
});

test('a blocked notification callback cannot block replies, close or abort', { timeout: 10_000 }, async t => {
  for (const action of ['close', 'cancel', 'abort']) {
    const started = deferred();
    const release = deferred();
    const controller = new AbortController();
    let callbackSignal;
    const { connection } = await launch(t, 'normal', {
      signal: controller.signal,
      onNotification: (_value, signal) => { callbackSignal = signal; started.resolve(); return release.promise; },
    });
    await connection.initialize();
    const expected = action === 'close' ? 'acp_closed' : 'acp_aborted';
    const check = assert.rejects(connection.request('hang'), code(expected));
    await started.promise;
    assert.deepEqual(await connection.request('echo', { progresses: true }), { progresses: true });
    if (action === 'abort') controller.abort();
    else await connection[action]();
    await check;
    assert.equal(callbackSignal.aborted, true);
    await stopped(connection, expected);
    release.resolve();
  }
});

test('callback failures and bounded delivery fail closed', { timeout: 10_000 }, async t => {
  for (const overflow of [false, true]) {
    const release = deferred();
    const { connection } = await launch(t, 'normal', {
      maxNotificationCallbacks: 1,
      onNotification: () => { if (!overflow) throw new Error('PRIVATE_CHILD_TEXT'); return release.promise; },
    });
    await connection.initialize();
    const expected = overflow ? 'acp_callback_overflow' : 'acp_callback_failed';
    await assert.rejects(connection.request(overflow ? 'notify-twice' : 'hang'), code(expected));
    await stopped(connection, expected);
    release.resolve();
  }
});

test('the record bound counts UTF-8 bytes and CR but not LF', { timeout: 10_000 }, async t => {
  const { connection } = await launch(t, 'normal', { maxRecordBytes: 512 });
  await connection.initialize();
  for (const crlf of [false, true]) {
    const result = await connection.request('sized', { bytes: 512, crlf });
    assert.equal(result.recordBytes, 512);
    assert.match(result.padding, /^界+x{0,2}$/u);
  }
  await assert.rejects(connection.request('sized', { bytes: 513, crlf: false }), code('acp_frame_too_large'));
  await stopped(connection, 'acp_frame_too_large');
});

test('caller bounds: pending limit, outbound size, validation and failed spawn', { timeout: 10_000 }, async t => {
  const { connection, cwd } = await launch(t, 'normal', { maxPendingRequests: 1, maxRecordBytes: 512 });
  await connection.initialize();
  const pending = connection.request('hang');
  const check = assert.rejects(pending, code('acp_closed'));
  await assert.rejects(connection.request('echo'), code('acp_pending_limit'));
  await connection.close();
  await check;
  const { connection: sized } = await launch(t, 'normal', { maxRecordBytes: 512 });
  await sized.initialize();
  await assert.rejects(sized.request('echo', { content: 'x'.repeat(513) }), code('acp_frame_too_large'));
  await stopped(sized, 'acp_frame_too_large');
  const base = { executable: process.execPath, args: [], cwd: tmpdir(), env: {}, onNotification() {} };
  for (const changed of [
    { executable: 'node' }, { args: 'bad' }, { args: ['a\0b'] }, { cwd: '.' }, { env: undefined },
    { env: { A: undefined } }, { env: { 'A=B': 'x' } }, { onNotification: undefined },
    { requestTimeoutMs: 0 }, { initializeTimeoutMs: Infinity }, { maxRecordBytes: 16_777_217 },
  ]) assert.throws(() => launchAcp({ ...base, ...changed }), code('acp_invalid_options'));
  const aborted = new AbortController();
  aborted.abort();
  assert.throws(() => launchAcp({ ...base, signal: aborted.signal }), code('acp_aborted'));
  for (const executable of [join(fixture, 'does-not-exist'), join(cwd, 'does-not-exist')]) {
    let failed;
    try { failed = launchAcp({ ...base, executable }); }
    catch (failure) { code('acp_spawn_failed')(failure); continue; }
    t.after(() => failed.close());
    await assert.rejects(failed.initialize(), code('acp_spawn_failed'));
    await stopped(failed, 'acp_spawn_failed');
  }
});
