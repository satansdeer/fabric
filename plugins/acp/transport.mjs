import { spawn } from 'node:child_process';
import { isAbsolute } from 'node:path';

const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const own = (value, key) => Object.hasOwn(value, key);
const text = value => typeof value === 'string';
const boolean = value => typeof value === 'boolean';
const nullable = check => value => value === null || check(value);
const idValid = value => text(value) || Number.isSafeInteger(value);
const error = code => Object.assign(new Error(code), { code });
const keys = (value, allowed) => object(value) && Object.keys(value).every(key => allowed.includes(key));
// Optional fields only; required fields are checked at their use sites. Unknown
// capabilities fail closed; extension data belongs in opaque _meta objects.
function shape(value, fields = {}) {
  return object(value) && Object.entries(value).every(([key, value]) => key === '_meta'
    ? value === null || object(value)
    : own(fields, key) && fields[key](value));
}
const empty = value => shape(value);
const prompt = value => shape(value, { image: boolean, audio: boolean, embeddedContext: boolean });
const mcp = value => shape(value, { http: boolean, sse: boolean });
const systemPrompt = value => shape(value, {
  mode: nullable(value => shape(value, { override: nullable(empty) })),
});
const sessions = value => shape(value, {
  close: nullable(empty), delete: nullable(empty), list: nullable(empty),
  resume: nullable(empty), additionalDirectories: nullable(empty),
  systemPrompt: nullable(systemPrompt),
});
function negotiation(value) {
  if (!object(value) || !Number.isSafeInteger(value.protocolVersion)) throw error('acp_invalid_initialize');
  if (value.protocolVersion !== 1) throw error('acp_unsupported_protocol');
  const capabilities = own(value, 'agentCapabilities') ? value.agentCapabilities : {};
  const authMethods = own(value, 'authMethods') ? value.authMethods : [];
  if (!shape(value, {
    protocolVersion: Number.isSafeInteger, agentCapabilities: object,
    authMethods: Array.isArray, agentInfo: nullable(object),
  }) || !shape(capabilities, {
    loadSession: boolean, promptCapabilities: prompt, mcpCapabilities: mcp,
    sessionCapabilities: sessions,
    auth: value => shape(value, { logout: nullable(empty) }),
  })) throw error('acp_invalid_initialize');
  const ids = new Set();
  for (const method of authMethods) {
    if (!shape(method, { id: text, name: text, description: nullable(text), type: value => value === 'agent' }) ||
        !text(method.id) || !text(method.name) || ids.has(method.id)) throw error('acp_invalid_initialize');
    ids.add(method.id);
  }
  const info = value.agentInfo ?? null;
  if (info !== null && (!shape(info, { name: text, version: text, title: nullable(text) }) ||
      !text(info.name) || !text(info.version))) throw error('acp_invalid_initialize');
  return { protocolVersion: 1, agentCapabilities: capabilities, authMethods, agentInfo: info };
}
function bound(value, fallback, min, max) {
  value ??= fallback;
  if (!Number.isSafeInteger(value) || value < min || value > max) throw error('acp_invalid_options');
  return value;
}
const literal = value => text(value) && !value.includes('\0');
const validSignal = signal => signal === undefined || signal instanceof AbortSignal;

/** Trusted-local transport only. Launch performs no discovery or initialization. */
export function launchAcp(options) {
  return new AcpConnection(options);
}

class AcpConnection {
  #child;
  #pending = new Map();
  #nextId = 1;
  #failure;
  #initialized = false;
  #initialization;
  #buffer;
  #used = 0;
  #decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
  #maxRecord;
  #maxPending;
  #maxCallbacks;
  #callbacks = 0;
  #callback;
  #controller = new AbortController();
  #requestTimeout;
  #initializeTimeout;
  #grace;
  #killTimer;
  #closed;
  #resolveClosed;

  constructor(options) {
    if (!object(options)) throw error('acp_invalid_options');
    const { executable, args, cwd, env, onNotification, signal } = options;
    if (!literal(executable) || !isAbsolute(executable) || !Array.isArray(args) || !args.every(literal) ||
        !literal(cwd) || !isAbsolute(cwd) || !object(env) ||
        !Object.entries(env).every(([key, value]) => key.length && !/[=\0]/.test(key) && literal(value)) ||
        typeof onNotification !== 'function' || !validSignal(signal)) throw error('acp_invalid_options');
    this.#maxRecord = bound(options.maxRecordBytes, 1_048_576, 256, 16_777_216);
    this.#maxPending = bound(options.maxPendingRequests, 64, 1, 1024);
    this.#maxCallbacks = bound(options.maxNotificationCallbacks, 64, 1, 1024);
    this.#requestTimeout = bound(options.requestTimeoutMs, 10_000, 1, 300_000);
    this.#initializeTimeout = bound(options.initializeTimeoutMs, 10_000, 1, 300_000);
    this.#grace = bound(options.terminationGraceMs, 250, 0, 5000);
    if (signal?.aborted) throw error('acp_aborted');
    this.#buffer = Buffer.allocUnsafe(this.#maxRecord);
    this.#callback = onNotification;
    const abort = () => this.#fail('acp_aborted');
    this.#closed = new Promise(resolve => { this.#resolveClosed = resolve; });
    try {
      this.#child = spawn(executable, args, { cwd, env: { ...env }, shell: false, stdio: ['pipe', 'pipe', 'pipe'] });
    } catch { throw error('acp_spawn_failed'); }
    signal?.addEventListener('abort', abort, { once: true });
    this.#child.on('error', () => this.#fail('acp_spawn_failed'));
    this.#child.stdin.on('error', () => this.#fail('acp_disconnected'));
    this.#child.stdout.on('error', () => this.#fail('acp_disconnected'));
    this.#child.stderr.on('error', () => this.#fail('acp_disconnected'));
    this.#child.stderr.resume(); // Drain and discard; never log raw child output.
    this.#child.stdout.on('data', chunk => this.#receive(chunk));
    this.#child.stdout.on('end', () => this.#fail(this.#used ? 'acp_truncated_frame' : 'acp_disconnected'));
    this.#child.on('exit', () => this.#fail('acp_disconnected'));
    this.#child.on('close', () => {
      this.#fail('acp_disconnected');
      clearTimeout(this.#killTimer);
      signal?.removeEventListener('abort', abort);
      this.#resolveClosed({ code: this.#failure.code });
    });
  }

  get pid() { return this.#child.pid; }
  /** Resolves only after Node has reaped the owned child and closed its pipes. */
  get closed() { return this.#closed; }

  initialize({ signal } = {}) {
    if (this.#initialization) return this.#initialization;
    this.#initialization = (async () => {
      try {
        const result = await this.#call('initialize', {
          protocolVersion: 1, clientCapabilities: {},
          clientInfo: { name: 'fabric-acp-transport', version: '0.1.0' },
        }, this.#initializeTimeout, signal);
        const metadata = negotiation(result);
        if (this.#failure) throw this.#failure;
        this.#initialized = true;
        return metadata;
      } catch (failure) {
        this.#fail(failure.code === 'acp_remote_error' ? 'acp_initialize_failed' : failure.code);
        await this.#closed;
        throw this.#failure;
      }
    })();
    return this.#initialization;
  }

  request(method, params = {}, { signal, timeoutMs = this.#requestTimeout } = {}) {
    if (this.#failure) return Promise.reject(this.#failure);
    if (!this.#initialized || method === 'initialize') return Promise.reject(error('acp_not_ready'));
    return this.#call(method, params, timeoutMs, signal);
  }

  notify(method, params = {}) {
    if (this.#failure) throw this.#failure;
    if (!this.#initialized || method === 'initialize') throw error('acp_not_ready');
    this.#validateCall(method, params);
    this.#send({ jsonrpc: '2.0', method, params });
    if (this.#failure) throw this.#failure;
  }

  cancel() { this.#fail('acp_aborted'); return this.#closed; }
  close() { this.#fail('acp_closed'); return this.#closed; }

  #validateCall(method, params) {
    if (!text(method) || !method.length || !(object(params) || Array.isArray(params))) throw error('acp_invalid_request');
  }

  async #call(method, params, timeoutMs, signal) {
    if (this.#failure) throw this.#failure;
    this.#validateCall(method, params);
    timeoutMs = bound(timeoutMs, this.#requestTimeout, 1, 300_000);
    if (!validSignal(signal)) throw error('acp_invalid_options');
    if (signal?.aborted) { this.#fail('acp_aborted'); throw this.#failure; }
    if (this.#pending.size >= this.#maxPending) throw error('acp_pending_limit');
    if (!Number.isSafeInteger(this.#nextId)) { this.#fail('acp_id_exhausted'); throw this.#failure; }
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      const abort = () => this.#fail('acp_aborted');
      const timer = setTimeout(() => this.#fail('acp_deadline'), timeoutMs);
      const cleanup = () => { clearTimeout(timer); signal?.removeEventListener('abort', abort); };
      this.#pending.set(id, { resolve, reject, cleanup });
      signal?.addEventListener('abort', abort, { once: true });
      this.#send({ jsonrpc: '2.0', id, method, params });
    });
  }

  #send(value) {
    if (this.#failure) return;
    let bytes;
    try { bytes = Buffer.from(JSON.stringify(value) + '\n'); }
    catch { this.#fail('acp_invalid_request'); return; }
    if (bytes.length - 1 > this.#maxRecord) { this.#fail('acp_frame_too_large'); return; }
    if (this.#child.stdin.writableLength + bytes.length > this.#maxRecord + 1) {
      this.#fail('acp_write_overflow'); return;
    }
    this.#child.stdin.write(bytes, failure => { if (failure) this.#fail('acp_disconnected'); });
  }

  #receive(chunk) {
    let start = 0;
    while (start < chunk.length && !this.#failure) {
      const newline = chunk.indexOf(10, start);
      const end = newline < 0 ? chunk.length : newline;
      const length = end - start;
      if (this.#used + length > this.#maxRecord) { this.#fail('acp_frame_too_large'); return; }
      chunk.copy(this.#buffer, this.#used, start, end);
      this.#used += length;
      if (newline < 0) return;
      let value;
      try { value = JSON.parse(this.#decoder.decode(this.#buffer.subarray(0, this.#used))); }
      catch { this.#fail('acp_malformed_frame'); return; }
      this.#used = 0;
      this.#dispatch(value);
      start = newline + 1;
    }
  }

  #dispatch(value) {
    if (!object(value) || value.jsonrpc !== '2.0') { this.#fail('acp_invalid_envelope'); return; }
    if (own(value, 'method')) {
      if (!keys(value, ['jsonrpc', 'id', 'method', 'params']) || !text(value.method) || !value.method.length ||
          (own(value, 'params') && !object(value.params) && !Array.isArray(value.params)) ||
          (own(value, 'id') && !idValid(value.id))) { this.#fail('acp_invalid_envelope'); return; }
      if (own(value, 'id')) {
        this.#send({ jsonrpc: '2.0', id: value.id, error: { code: -32601, message: 'Method not found' } });
        return;
      }
      if (this.#callbacks >= this.#maxCallbacks) { this.#fail('acp_callback_overflow'); return; }
      this.#callbacks++;
      // Do not await user code in the reader: replies, abort and close must progress.
      Promise.resolve().then(() => {
        if (!this.#failure) return this.#callback(value, this.#controller.signal);
      }).catch(() => this.#fail('acp_callback_failed')).finally(() => { this.#callbacks--; });
      return;
    }
    if (!keys(value, ['jsonrpc', 'id', 'result', 'error']) || own(value, 'result') === own(value, 'error')) {
      this.#fail('acp_invalid_envelope'); return;
    }
    if (!own(value, 'id') || !idValid(value.id) || !this.#pending.has(value.id)) {
      this.#fail('acp_invalid_response_id'); return;
    }
    if (own(value, 'error') && (!keys(value.error, ['code', 'message', 'data']) ||
        !Number.isSafeInteger(value.error.code) || !text(value.error.message))) {
      this.#fail('acp_invalid_envelope'); return;
    }
    const pending = this.#pending.get(value.id);
    this.#pending.delete(value.id);
    pending.cleanup();
    if (own(value, 'error')) pending.reject(error('acp_remote_error'));
    else pending.resolve(value.result);
  }

  #fail(code) {
    if (this.#failure) return;
    this.#failure = error(code);
    for (const pending of this.#pending.values()) { pending.cleanup(); pending.reject(this.#failure); }
    this.#pending.clear();
    this.#used = 0;
    this.#buffer = null;
    this.#controller.abort();
    this.#child.stdin.destroy();
    this.#child.stdout.destroy();
    this.#child.stderr.destroy();
    // Only the process handle we launched, never name/PID discovery or detachment.
    if (this.#child.exitCode === null && this.#child.signalCode === null) {
      this.#child.kill('SIGTERM');
      this.#killTimer = setTimeout(() => this.#child.kill('SIGKILL'), this.#grace);
    }
  }
}
