import { createServer, createConnection } from 'node:net';
import { chmodSync, lstatSync, realpathSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { randomBytes } from 'node:crypto';
import { fields, object, fail, sha256, permissionDirectory } from './config.mjs';

const REQUEST_BYTES = 32_768;
const CONTROL_BYTES = 4096;
const RESPONSE_BYTES = 300_000;
const id = value => typeof value === 'string' && value.length > 0 && Buffer.byteLength(value) <= 128 && !/[\x00-\x1f\x7f]/.test(value);
const kinds = ['allow_once', 'reject_once', 'allow_always', 'reject_always'];
const meta = value => value === undefined || value === null || object(value);
const sameDirectory = (pin, workspace) => {
  const actual = permissionDirectory(pin.directory, workspace);
  if (actual.dev !== pin.dev || actual.ino !== pin.ino) fail('acp_unsafe_permission_directory');
};

/** Trusted same-UID local control, not an OS security boundary. No approval timers. */
export async function openPermissions(pin, workspace) {
  sameDirectory(pin, workspace);
  const endpoint = join(pin.directory, `p-${randomBytes(12).toString('hex')}.sock`);
  const nonce = randomBytes(32).toString('hex');
  const pending = new Map(), seen = new Set(), tools = new Set(), clients = new Set();
  let closed = false, closing;
  function control(message) {
    if (closed) return { error: 'closed' };
    if (fields(message, ['op']) && message.op === 'list') {
      return { endpoint, nonce, pending: [...pending.values()].map(item => item.review) };
    }
    if (!fields(message, ['op', 'endpoint', 'nonce', 'sessionId', 'requestId', 'digest', 'optionId', 'kind']) ||
        message.op !== 'decide' || message.endpoint !== endpoint || message.nonce !== nonce) return { error: 'invalid_decision' };
    const item = pending.get(message.requestId);
    if (!item || message.sessionId !== item.review.sessionId || message.digest !== item.review.digest ||
        !['allow_once', 'reject_once'].includes(message.kind) ||
        !item.review.request.options.some(option => option.optionId === message.optionId && option.kind === message.kind)) {
      return { error: 'invalid_decision' };
    }
    pending.delete(message.requestId);
    item.settle({ outcome: { outcome: 'selected', optionId: message.optionId } });
    return { ok: true };
  }
  const server = createServer(socket => {
    if (closed || clients.size >= 8) { socket.destroy(); return; }
    clients.add(socket);
    let bytes = Buffer.alloc(0), used = false;
    socket.setTimeout(5000, () => socket.destroy()); // Idle client bound, never a permission deadline.
    socket.on('error', () => {});
    socket.on('close', () => clients.delete(socket));
    socket.on('data', chunk => {
      if (used || bytes.length + chunk.length > CONTROL_BYTES) { socket.destroy(); return; }
      bytes = Buffer.concat([bytes, chunk]);
      const newline = bytes.indexOf(10);
      if (newline < 0) return;
      used = true;
      let response;
      try {
        if (newline !== bytes.length - 1) throw new Error();
        response = JSON.stringify(control(JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(0, newline)))));
      } catch { response = JSON.stringify({ error: 'invalid_message' }); }
      if (Buffer.byteLength(response) + 1 > RESPONSE_BYTES) { socket.destroy(); return; }
      socket.end(response + '\n');
    });
  });
  // No unlink before bind: EADDRINUSE is a failure, never endpoint adoption.
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen({ path: endpoint, backlog: 8 }, () => { server.removeListener('error', reject); resolve(); });
  });
  try {
    chmodSync(endpoint, 0o600);
    sameDirectory(pin, workspace);
  } catch (error) { await new Promise(resolve => server.close(resolve)); throw error; }
  server.on('error', () => { void close(); });
  function close() {
    if (closing) return closing;
    closed = true;
    for (const item of pending.values()) item.settle(null);
    pending.clear();
    for (const socket of clients) socket.destroy();
    // Node/libuv removes the socket it bound on close; never scan/unlink siblings.
    closing = new Promise(resolve => server.close(resolve));
    return closing;
  }
  return {
    endpoint,
    get pendingCount() { return pending.size; },
    close,
    request(message, sessionId, signal) {
      const params = message.params;
      if (closed || signal.aborted || !fields(params, ['sessionId', 'toolCall', 'options', '_meta']) ||
          params.sessionId !== sessionId || !meta(params._meta) || !id(sessionId) ||
          !object(params.toolCall) || !id(params.toolCall.toolCallId) ||
          !Array.isArray(params.options) || !params.options.length || params.options.length > 16 ||
          Buffer.byteLength(JSON.stringify(params)) > REQUEST_BYTES || seen.has(message.id) ||
          tools.has(params.toolCall.toolCallId) || seen.size >= 64 || pending.size >= 8) fail('acp_invalid_permission');
      const options = new Set();
      for (const option of params.options) {
        if (!fields(option, ['optionId', 'name', 'kind', '_meta']) || !id(option.optionId) ||
            typeof option.name !== 'string' || Buffer.byteLength(option.name) > 1024 || !meta(option._meta) ||
            !kinds.includes(option.kind) || options.has(option.optionId)) fail('acp_invalid_permission');
        options.add(option.optionId);
      }
      if (!params.options.some(option => ['allow_once', 'reject_once'].includes(option.kind))) fail('acp_invalid_permission');
      seen.add(message.id);
      tools.add(params.toolCall.toolCallId);
      const digest = sha256(JSON.stringify({ endpoint, nonce, sessionId, requestId: message.id, request: params }));
      return new Promise((resolve, reject) => {
        const abort = () => { pending.delete(message.id); settle(null); };
        const settle = result => {
          signal.removeEventListener('abort', abort);
          if (result && !closed && !signal.aborted) resolve(result);
          else reject(Object.assign(new Error('acp_aborted'), { code: 'acp_aborted' }));
        };
        pending.set(message.id, { review: { sessionId, requestId: message.id, digest, request: params }, settle });
        signal.addEventListener('abort', abort, { once: true });
      });
    },
  };
}

/** One bounded exchange; callers must review list output as untrusted private data. */
export async function permissionControl(endpoint, message) {
  permissionDirectory(dirname(endpoint));
  if (realpathSync(endpoint) !== endpoint) fail('acp_unsafe_permission_socket');
  const before = lstatSync(endpoint);
  if (!before.isSocket() || before.uid !== process.getuid() || (before.mode & 0o077)) fail('acp_unsafe_permission_socket');
  const bytes = Buffer.from(JSON.stringify(message) + '\n');
  if (bytes.length > CONTROL_BYTES) fail('acp_invalid_permission');
  return new Promise((resolve, reject) => {
    const socket = createConnection(endpoint);
    let response = Buffer.alloc(0), done = false;
    const finish = (error, value) => {
      if (done) return;
      done = true;
      socket.destroy();
      if (error) reject(Object.assign(new Error('acp_permission_control_failed'), { code: 'acp_permission_control_failed' }));
      else resolve(value);
    };
    socket.setTimeout(5000, () => finish(true));
    socket.on('error', () => finish(true));
    socket.on('close', () => { if (!done) finish(true); });
    socket.on('connect', () => {
      try {
        permissionDirectory(dirname(endpoint));
        const after = lstatSync(endpoint);
        if (before.dev !== after.dev || before.ino !== after.ino || !after.isSocket() ||
            after.uid !== process.getuid() || (after.mode & 0o077)) throw new Error();
        socket.write(bytes);
      } catch { finish(true); }
    });
    socket.on('data', chunk => {
      if (response.length + chunk.length > RESPONSE_BYTES) { finish(true); return; }
      response = Buffer.concat([response, chunk]);
      const newline = response.indexOf(10);
      if (newline < 0) return;
      try {
        if (newline !== response.length - 1) throw new Error();
        finish(false, JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(response.subarray(0, newline))));
      } catch { finish(true); }
    });
  });
}
