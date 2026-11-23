import { spawn } from 'node:child_process';
import { mkdtempSync, realpathSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { directory } from './package.mjs';
import { exact, frame, frames, resultFor, fail, NATIVE_MAX_FRAME, NATIVE_NORMALIZE_MAX_FRAME } from './protocol.mjs';
import { TARGET_OPERATIONS, TARGET_ACTIONS, validTargetPlan } from './index.mjs';
import { createHmac } from 'node:crypto';
import { isDeepStrictEqual } from 'node:util';
import { webhookSigningBytes, validNormalizedTrigger } from './webhook.mjs';

export const FIXTURE_PROMPT = 'Fabric provider-free fixture';
function kill(child) { if (child.pid) { try { process.kill(-child.pid, 'SIGKILL'); } catch {} } }
async function processFixture(entry, args, operation, timeout, native = false) {
  const home = realpathSync(mkdtempSync(join(tmpdir(), 'fabric-plugin-')));
  let child;
  let timer;
  let timedOut = false;
  try {
    child = spawn(native ? entry : process.execPath, native ? args(home) : [entry, ...args(home)], {
      cwd: '/', env: { PATH: '/usr/bin:/bin', HOME: home, XDG_CONFIG_HOME: home, XDG_CACHE_HOME: home, XDG_DATA_HOME: home, LC_ALL: 'C' },
      stdio: ['pipe', 'pipe', 'ignore'], detached: true,
    });
    const closed = new Promise(resolve => child.once('close', (code, signal) => resolve({ code, signal })));
    child.stdin.on('error', () => {});
    child.on('error', () => {});
    timer = setTimeout(() => { timedOut = true; kill(child); }, timeout);
    try {
      await operation(child, home);
      const status = await closed;
      if (timedOut) fail('timeout');
      if (status.code !== 0 || status.signal) fail('unsuccessful_exit');
    } catch (e) {
      kill(child);
      await closed;
      if (timedOut) fail('timeout');
      throw e;
    }
  } finally {
    clearTimeout(timer);
    if (child) kill(child);
    rmSync(home, { recursive: true, force: true });
  }
}
// Reference-only CLI doubles: never discover or execute an installed provider CLI.
async function nativeConformance(info, entry, timeout) {
  const versions = { omp: 'omp/19.1.0', codex: 'codex-cli 0.171.0' };
  if (!versions[info.id]) fail('native_fixture_required');
  let result;
  const events = [];
  await processFixture(entry, () => [...(info.args ?? []), '--fabric-plugin-stdio'], async (child, home) => {
    const binary = join(home, info.native.executable);
    writeFileSync(binary, `#!/bin/sh\nprintf '%s\\n' '${versions[info.id]}'\n`, { mode: 0o700 });
    const root = join(home, 'native');
    mkdirSync(root);
    // Fixture metadata only. No authentication store is read or copied.
    writeFileSync(join(root, info.id === 'omp' ? 'agent.db' : 'auth.json'), 'provider-free fixture');
    const context = { binary, environment: { HOME: home, PATH: '/usr/bin:/bin', [info.native.root_environment[0]]: root }, credentials: {} };
    const responses = frames(child.stdout, NATIVE_MAX_FRAME)[Symbol.asyncIterator]();
    let id = 0;
    async function rpc(method, params) {
      const limit = method === 'harness.normalize' ? NATIVE_NORMALIZE_MAX_FRAME : NATIVE_MAX_FRAME;
      child.stdin.write(frame({ jsonrpc: '2.0', id: ++id, method, params }, limit));
      const next = await responses.next();
      if (next.done || !exact(next.value, ['jsonrpc', 'id', 'result']) || next.value.jsonrpc !== '2.0' || next.value.id !== id) fail('invalid_response');
      return next.value.result;
    }
    const hello = await rpc('initialize', { protocol: 1 });
    if (!exact(hello, ['protocol', 'id', 'version', 'kind']) || hello.protocol !== 1 || ['id', 'version', 'kind'].some(k => hello[k] !== info[k])) fail('negotiation_failed');
    if ((await rpc('health', {})).ready !== true) fail('invalid_readiness');
    async function native(method, params) {
      const reply = await rpc(method, params);
      if (!exact(reply, ['result', 'failure']) || reply.failure !== null || reply.result === null) fail('native_fixture_failed');
      return reply.result;
    }
    const probe = await native('harness.probe', { context });
    const discovery = await native('harness.native_context', { context });
    if (probe.version !== versions[info.id] || discovery.profile.harness_id !== info.id || !discovery.credential_available) fail('invalid_native_discovery');
    const manifest = await native('harness.capabilities', { version: probe.version });
    if (manifest.harness_id !== info.id || manifest.exact_version !== probe.version) fail('invalid_native_capabilities');
    const request = { profile: discovery.profile };
    result = await native('harness.plan', { request, version: probe.version, context });
    const prompt_file = join(home, 'prompt.txt');
    writeFileSync(prompt_file, FIXTURE_PROMPT);
    const command = await native('harness.command', { request, profile: result.plan.profile, prompt_file, context });
    if (command.program !== binary || !command.args.some(a => a?.prompt === true)) fail('invalid_native_command');
    for (let i = 0; i < 2; i++) {
      const event = info.id === 'omp'
        ? { type: 'message_update', assistantMessageEvent: { type: 'text_delta', delta: FIXTURE_PROMPT } }
        : { type: 'item.completed', item: { type: 'agent_message', text: FIXTURE_PROMPT } };
      const normalized = await native('harness.normalize', { event });
      if (normalized.event?.type !== 'output' || normalized.event.text !== FIXTURE_PROMPT) fail('invalid_native_event');
      events.push(normalized.event);
    }
    if ((await rpc('shutdown', {})).ready !== true) fail('invalid_readiness');
    child.stdin.end();
    if (!(await responses.next()).done) fail('extra_response');
  }, timeout, info.runtime === 'native');
  return { id: info.id, kind: info.kind, protocol: 1, result, events };
}
export async function conformance(path, { timeout = 5000, connectorFixture } = {}) {
  if (!Number.isInteger(timeout) || timeout < 1 || timeout > 60_000) fail('invalid_timeout');
  const { info } = directory(path); // Negotiation metadata is checked before any plugin code runs.
  const entry = resolve(path, info.entry);
  if (info.native) return nativeConformance(info, entry, timeout);
  const method = info.kind === 'connector' ? 'connector.capabilities' : 'harness.plan';
  let result;
  await processFixture(entry, () => [...(info.args ?? []), '--fabric-plugin-stdio'], async (child, home) => {
    const responses = frames(child.stdout)[Symbol.asyncIterator]();
    let id = 0;
    const call = async (method, params) => {
      child.stdin.write(frame({ jsonrpc: '2.0', id: ++id, method, params }));
      const next = await responses.next();
      if (next.done) fail('closed_output');
      const r = next.value;
      if (exact(r, ['jsonrpc', 'id', 'error']) && r.jsonrpc === '2.0' && r.id === id) fail('remote_error');
      if (!exact(r, ['jsonrpc', 'id', 'result']) || r.jsonrpc !== '2.0' || r.id !== id) fail('invalid_response');
      return r.result;
    };
    const initialized = await call('initialize', { protocol: 1 });
    if (!exact(initialized, ['protocol', 'id', 'version', 'kind']) || initialized.protocol !== 1 ||
        ['id', 'version', 'kind'].some(k => initialized[k] !== info[k])) fail('negotiation_failed: check plugin identity/version/protocol');
    const ready = value => { if (!exact(value, ['ready']) || value.ready !== true) fail('invalid_readiness'); };
    ready(await call('health', {}));
    if (info.kind === 'target') {
      result = await call('target.capabilities', {});
      if (!exact(result, ['placements', 'operations', 'optional']) ||
          JSON.stringify(result.placements) !== JSON.stringify([info.id]) ||
          JSON.stringify(result.operations) !== JSON.stringify(TARGET_OPERATIONS) ||
          !Array.isArray(result.optional) || result.optional.length) fail('unsupported_target_capabilities');
      const placement = await call('target.placement', { placement: info.id, backends: ['host.local'] });
      if (!exact(placement, ['backend']) || placement.backend !== 'host.local') fail('unsupported_target_placement');
      // Transport fixtures only. Real execution/ownership evidence comes from Core, not this probe.
      const evidence = {
        prepare: { size_bytes: 1, cloned_files: 0, copied_files: 1 },
        start: { exit_code: 0, exit_signal: null, stop_reason: null },
        inspect: { exited: true }, cancel: { termination_requested: true },
        collect: { artifact_count: 1 }, reclaim: { removed: true },
      };
      for (const op of TARGET_OPERATIONS) {
        const job = { placement: info.id, run_id: 'conformance', grant: String(id + 1),
          actions: TARGET_ACTIONS[op], context: {} };
        const plan = await call(`target.${op}`, job);
        if (!validTargetPlan(plan, job) || plan.action.primitive === 'unsupported') fail('unsupported_target_operation');
        const received = await call('target.completed', { grant: job.grant, evidence: evidence[op] });
        if (!exact(received, ['received']) || received.received !== job.grant) fail('invalid_target_receipt');
      }
    } else {
      result = resultFor(info.kind, await call(method, {}));
      if (info.kind === 'harness' && !result.ready) fail('harness_not_ready');
      if (info.kind === 'connector') {
        const fixture = connectorFixture ?? {
          body: JSON.stringify({ event_id: 'event-1', idempotency_key: 'business-1', data: { title: FIXTURE_PROMPT } }),
          expected: { version: 1, source_id: 'fixture', tenant_id: 'fixture', scope: 'events.read',
            route: 'fixture', event_id: 'event-1', idempotency_key: 'business-1', data: { title: FIXTURE_PROMPT } },
        };
        if (!(exact(fixture, ['body', 'expected']) || exact(fixture, ['body', 'expected', 'authorization'])) ||
          typeof fixture.body !== 'string' || !validNormalizedTrigger(fixture.expected) ||
          fixture.authorization !== undefined && (typeof fixture.authorization !== 'string' || fixture.authorization.length > 512)) fail('invalid_connector_fixture');
        const key = Buffer.alloc(32, 7), secret_ref = join(home, 'fixture-key');
        writeFileSync(secret_ref, key, { mode: 0o600 });
        const { source_id, tenant_id, scope, route } = fixture.expected;
        const input = { version: 1, now: 1000, policy: { source_id, tenant_id, scope, route,
          auth: 'hmac-sha256', secret_ref, max_payload_bytes: 8192, max_age_seconds: 60 },
          request: { body: fixture.body, timestamp: 1000, expires_at: 1060, nonce: 'conformance', authorization: '' } };
        input.request.authorization = fixture.authorization ?? ('sha256=' + createHmac('sha256', key).update(webhookSigningBytes(input)).digest('hex'));
        const verification = await call('connector.verify', input);
        if (!exact(verification, ['verified']) || verification.verified !== true) fail('connector_not_verified');
        const trigger = await call('connector.normalize', input);
        if (!validNormalizedTrigger(trigger) || !isDeepStrictEqual(trigger, fixture.expected)) fail('invalid_connector_trigger');
      }
    }
    ready(await call('shutdown', {}));
    child.stdin.end();
    if (!(await responses.next()).done) fail('extra_response');
  }, timeout, info.runtime === 'native');
  const events = [];
  if (info.kind === 'harness') {
    await processFixture(entry, home => {
      const prompt = join(home, 'prompt.txt');
      writeFileSync(prompt, FIXTURE_PROMPT, { mode: 0o600 });
      return ['--fabric-plugin-run', prompt];
    }, async child => {
      child.stdin.end();
      let bytes = 0;
      for await (const event of frames(child.stdout)) {
        bytes += frame(event).length;
        if (!event || typeof event.type !== 'string' || events.length >= 1024 || bytes > 1024 * 1024) fail('invalid_harness_events');
        events.push(event);
      }
      if (!events.length) fail('missing_harness_events');
    }, timeout);
  }
  return { id: info.id, kind: info.kind, protocol: 1, result, events };
}
