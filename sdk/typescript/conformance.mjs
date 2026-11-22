import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { directory } from './package.mjs';
import { exact, frame, frames, resultFor, fail } from './protocol.mjs';

export const FIXTURE_PROMPT = 'Fabric provider-free fixture';
function kill(child) { if (child.pid) { try { process.kill(-child.pid, 'SIGKILL'); } catch {} } }
async function processFixture(entry, args, operation, timeout) {
  const home = mkdtempSync(join(tmpdir(), 'fabric-plugin-'));
  let child;
  let timer;
  let timedOut = false;
  try {
    child = spawn(process.execPath, [entry, ...args(home)], {
      cwd: '/', env: { PATH: '/usr/bin:/bin', HOME: home, XDG_CONFIG_HOME: home, XDG_CACHE_HOME: home, XDG_DATA_HOME: home, LC_ALL: 'C' },
      stdio: ['pipe', 'pipe', 'ignore'], detached: true,
    });
    const closed = new Promise(resolve => child.once('close', (code, signal) => resolve({ code, signal })));
    child.stdin.on('error', () => {});
    child.on('error', () => {});
    timer = setTimeout(() => { timedOut = true; kill(child); }, timeout);
    try {
      await operation(child);
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
export async function conformance(path, { timeout = 5000 } = {}) {
  if (!Number.isInteger(timeout) || timeout < 1 || timeout > 60_000) fail('invalid_timeout');
  const { info } = directory(path); // Negotiation metadata is checked before any plugin code runs.
  const entry = resolve(path, info.entry);
  const method = info.kind === 'connector' ? 'connector.evidence' : `${info.kind}.plan`;
  let result;
  await processFixture(entry, () => ['--fabric-plugin-stdio'], async child => {
    const responses = frames(child.stdout)[Symbol.asyncIterator]();
    const requests = [
      ['initialize', { protocol: 1 }], ['health', {}],
      [method, info.kind === 'target' ? { placement: 'local' } : {}], ['shutdown', {}],
    ];
    for (let i = 0; i < requests.length; i++) {
      child.stdin.write(frame({ jsonrpc: '2.0', id: i + 1, method: requests[i][0], params: requests[i][1] }));
      const next = await responses.next();
      if (next.done) fail('closed_output');
      const r = next.value;
      if (exact(r, ['jsonrpc', 'id', 'error']) && r.jsonrpc === '2.0' && r.id === i + 1) fail('remote_error');
      if (!exact(r, ['jsonrpc', 'id', 'result']) || r.jsonrpc !== '2.0' || r.id !== i + 1) fail('invalid_response');
      if (i === 0) {
        if (!exact(r.result, ['protocol', 'id', 'version', 'kind']) || r.result.protocol !== 1 || ['id', 'version', 'kind'].some(k => r.result[k] !== info[k])) fail('negotiation_failed: check plugin identity/version/protocol');
      } else if (i === 2) {
        result = resultFor(info.kind, r.result);
        if (info.kind === 'harness' && !result.ready) fail('harness_not_ready');
      } else if (!exact(r.result, ['ready']) || r.result.ready !== true) fail('invalid_readiness');
    }
    child.stdin.end();
    if (!(await responses.next()).done) fail('extra_response');
  }, timeout);
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
