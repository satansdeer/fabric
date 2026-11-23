#!/usr/bin/env node
// External ingress/outbound adapter, not a worker scheduler. All execution belongs to Fabric.
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { readFileSync, writeFileSync, realpathSync, unlinkSync, lstatSync, readdirSync, existsSync } from 'node:fs';
import { dirname, join, resolve, isAbsolute } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { create } from '../../sdk/typescript/cli.mjs';
import { loadPackage } from '../../sdk/typescript/package.mjs';
import { frame, frames, exact } from '../../sdk/typescript/protocol.mjs';
import { validNormalizedTrigger } from '../../sdk/typescript/webhook.mjs';
import { loadConfig, privateJSON, inspect, requireThat, forward } from './connector.mjs';

const here = dirname(fileURLToPath(import.meta.url));
export function packageConnector(path, configPath) {
  loadConfig(configPath);
  const root = create('connector', path, 'github-issues');
  unlinkSync(join(root, 'plugin.ts'));
  unlinkSync(join(root, 'tsconfig.json'));
  for (const name of ['plugin.mjs', 'connector.mjs', 'bridge.mjs']) {
    const source = readFileSync(join(here, name), 'utf8').replaceAll('../../sdk/typescript/', './sdk/');
    writeFileSync(join(root, name), source, { mode: 0o600, flag: 'wx' });
  }
  const pkg = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
  pkg.files = ['plugin.mjs', 'connector.mjs', 'bridge.mjs', 'sdk'];
  pkg.fabricPlugin.entry = 'plugin.mjs';
  pkg.fabricPlugin.args = ['--config', configPath];
  writeFileSync(join(root, 'package.json'), JSON.stringify(pkg, null, 2) + '\n');
  return { package: root, sha256: loadPackage(root).hash, version: pkg.version };
}

export function inputFor(c, request) {
  const now = Math.floor(Date.now() / 1000);
  return { version: 1, now, policy: { ...c.binding, auth: 'hmac-sha256', secret_ref: c.webhook_secret_ref,
    max_payload_bytes: 8192, max_age_seconds: 120 },
    request: { ...request, timestamp: now, expires_at: now + 120 } };
}

export async function exchange(options, c, request, operation = 'normalize') {
  requireThat(isAbsolute(options.plugin) && realpathSync(options.plugin) === options.plugin && /^[a-f0-9]{64}$/.test(options.sha256));
  const pkg = loadPackage(options.plugin);
  requireThat(pkg.hash === options.sha256 && pkg.info.id === 'github-issues' && pkg.info.kind === 'connector' &&
    pkg.info.version === '1.0.0' && JSON.stringify(pkg.info.args) === JSON.stringify(['--config', options.config]));
  const child = spawn(process.execPath, [join(options.plugin, pkg.info.entry), ...pkg.info.args, '--fabric-plugin-stdio'], {
    cwd: '/', env: { PATH: '/usr/bin:/bin', LC_ALL: 'C' }, stdio: ['pipe', 'pipe', 'ignore'], detached: true });
  // Outbound hooks include bounded network IO/artifact verification; ingress never waits for a worker.
  const closed = once(child, 'close');
  closed.catch(() => {}); // Observe launch errors immediately; the original promise still rejects below.
  const timer = setTimeout(() => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }, operation === 'normalize' ? 10000 : 240000);
  child.stdin.on('error', () => {});
  const responses = frames(child.stdout)[Symbol.asyncIterator]();
  let id = 0;
  async function call(method, params) {
    child.stdin.write(frame({ jsonrpc: '2.0', id: ++id, method, params }));
    const item = await responses.next();
    requireThat(!item.done && exact(item.value, ['jsonrpc', 'id', 'result']) && item.value.jsonrpc === '2.0' && item.value.id === id);
    return item.value.result;
  }
  try {
    const identity = await call('initialize', { protocol: 1 });
    requireThat(exact(identity, ['protocol', 'id', 'version', 'kind']) && identity.protocol === 1 && identity.id === pkg.info.id &&
      identity.version === pkg.info.version && identity.kind === 'connector');
    requireThat((await call('health', {})).ready === true);
    const capabilities = await call('connector.capabilities', {});
    requireThat(capabilities.version === 1 && JSON.stringify(capabilities.optional) === JSON.stringify(['subscribe', 'reply']));
    let result;
    if (operation === 'normalize') {
      const input = inputFor(c, request), verified = await call('connector.verify', input);
      requireThat(exact(verified, ['verified']) && verified.verified === true);
      result = await call('connector.normalize', input);
      requireThat(validNormalizedTrigger(result) && Object.entries(c.binding).every(([k, v]) => result[k] === v));
    } else {
      requireThat(['subscribe', 'reply'].includes(operation));
      const output = await call('connector.' + operation, { version: 1, data: operation === 'subscribe' ? {} : { request } });
      requireThat(exact(output, ['version', 'data']) && output.version === 1);
      result = output.data;
    }
    requireThat((await call('shutdown', {})).ready === true);
    child.stdin.end();
    requireThat((await responses.next()).done);
    const [code, signal] = await closed;
    requireThat(code === 0 && signal === null);
    return result;
  } finally {
    clearTimeout(timer);
    if (child.exitCode === null && child.signalCode === null) {
      try { process.kill(-child.pid, 'SIGKILL'); } catch {}
    }
    await closed.catch(() => {});
  }
}

export async function listen(options, c) {
  requireThat(isAbsolute(options.inbox) && realpathSync(options.inbox) === options.inbox);
  const stat = lstatSync(options.inbox);
  requireThat(stat.isDirectory() && stat.uid === process.getuid() && !(stat.mode & 0o077));
  requireThat(/^\d{1,5}$/.test(options.port) && Number(options.port) <= 65535);
  let busy = false;
  const server = createServer(async (incoming, response) => {
    const send = (code, value) => { response.writeHead(code, { 'content-type': 'application/json' }); response.end(JSON.stringify(value)); };
    if (incoming.method !== 'POST' || incoming.url !== '/github') return send(404, { error: 'not_found' });
    if (busy) return send(503, { error: 'busy' });
    busy = true;
    try {
      const chunks = []; let bytes = 0;
      for await (const chunk of incoming) { bytes += chunk.length; requireThat(bytes <= 8192); chunks.push(chunk); }
      const raw = new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks, bytes));
      const now = Math.floor(Date.now() / 1000);
      const request = { body: JSON.stringify({ event: incoming.headers['x-github-event'], delivery: incoming.headers['x-github-delivery'], raw }),
        timestamp: now, expires_at: now + 120, nonce: incoming.headers['x-github-delivery'], authorization: incoming.headers['x-hub-signature-256'] };
      const trigger = await exchange(options, c, request);
      const path = join(options.inbox, trigger.event_id + '.json');
      try {
        if (!existsSync(path)) requireThat(readdirSync(options.inbox).length < 256);
        writeFileSync(path, JSON.stringify(request) + '\n', { mode: 0o600, flag: 'wx' });
      } catch (error) {
        if (error.code !== 'EEXIST') throw error;
        const prior = await exchange(options, c, privateJSON(path));
        requireThat(prior.event_id === trigger.event_id);
      }
      // Preserve the first signed envelope before forwarding, including on uncertain network outcomes.
      const result = await forward(c, trigger);
      send(result.duplicate ? 200 : 202, result);
    } catch { send(403, { error: 'github_connector_rejected' }); }
    finally { busy = false; }
  });
  server.requestTimeout = 15000;
  server.headersTimeout = 10000;
  server.maxHeadersCount = 32;
  server.maxConnections = 8;
  await new Promise((accept, reject) => { server.once('error', reject); server.listen(Number(options.port), '127.0.0.1', accept); });
  return server;
}

export async function main(args) {
  const command = args.shift(), options = {};
  const allowed = { package: ['config', 'out'], preview: ['config', 'request'], forward: ['config', 'request', 'plugin', 'sha256'],
    subscribe: ['config', 'plugin', 'sha256'], reply: ['config', 'request', 'plugin', 'sha256'], serve: ['config', 'plugin', 'sha256', 'inbox', 'port'] };
  requireThat(Object.hasOwn(allowed, command));
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i].slice(2);
    requireThat(args[i].startsWith('--') && allowed[command].includes(key) && !Object.hasOwn(options, key) && typeof args[i + 1] === 'string');
    options[key] = args[i + 1];
  }
  requireThat(allowed[command].every(k => Object.hasOwn(options, k)));
  const c = loadConfig(options.config);
  let result;
  if (command === 'package') result = packageConnector(options.out, options.config);
  else if (command === 'preview') {
    const { data } = inspect(c, privateJSON(options.request), c.webhook_secret_ref, false);
    result = { issue_url: data.issue_url, approval_sha256: data.approval_sha256, admitted: false };
  } else if (command === 'serve') {
    const server = await listen(options, c);
    process.stdout.write(JSON.stringify({ listening: `http://127.0.0.1:${server.address().port}/github` }) + '\n');
    for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => { server.close(); server.closeAllConnections(); });
    return;
  } else {
    const request = command === 'subscribe' ? undefined : privateJSON(options.request);
    result = command === 'forward' ? await forward(c, await exchange(options, c, request)) : await exchange(options, c, request, command);
  }
  process.stdout.write(JSON.stringify(result) + '\n');
}
if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  main(process.argv.slice(2)).catch(() => {
    process.stderr.write('GitHub connector: invalid input, denied permission or unavailable evidence.\n');
    process.exitCode = 1;
  });
}
