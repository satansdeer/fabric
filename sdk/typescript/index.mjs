import { readBounded, identity, exact, frame, frames, resultFor, fail, PROTOCOL_VERSION, SDK_VERSION, NATIVE_MAX_FRAME, NATIVE_NORMALIZE_MAX_FRAME } from './protocol.mjs';
import { validConnectorInput, validNormalizedTrigger } from './webhook.mjs';
export { verifyWebhook, normalizeWebhook, webhookSigningBytes } from './webhook.mjs';

export { PROTOCOL_VERSION, SDK_VERSION };
function define(kind, options) {
  identity({ ...options, kind });
  if (typeof options.plan !== 'function' || (kind === 'harness' && typeof options.run !== 'function')) fail('invalid_definition');
  return Object.freeze({ ...options, kind });
}
export const defineHarness = (options) => define('harness', options);
export const TARGET_OPERATIONS = Object.freeze(['prepare', 'start', 'inspect', 'cancel', 'collect', 'reclaim']);
export function defineTarget(options) {
  identity({ ...options, kind: 'target' });
  if (['capabilities', 'placement'].some(method => typeof options[method] !== 'function') ||
      typeof options.remote !== 'function' && ['completed', ...TARGET_OPERATIONS].some(method => typeof options[method] !== 'function')) fail('invalid_definition');
  return Object.freeze({ ...options, kind: 'target' });
}
export const REMOTE_OPERATIONS = Object.freeze(['environment.ensure', 'environment.inspect', 'environment.destroy',
  'harness.probe', 'harness.plan', 'pty.open', 'pty.exchange', 'pty.close', 'workspace.create',
  'source.write', 'source.seal', 'worker.start', 'worker.poll', 'worker.cancel',
  'workspace.export', 'workspace.read', 'workspace.reclaim']);
export function defineConnector(options) {
  identity({ ...options, kind: 'connector' });
  if (['capabilities', 'verify', 'normalize'].some(name => typeof options[name] !== 'function') ||
      ['subscribe', 'reply'].some(name => options[name] !== undefined && typeof options[name] !== 'function')) fail('invalid_definition');
  return Object.freeze({ ...options, kind: 'connector' });
}
const write = (value, max) => new Promise((resolve, reject) => process.stdout.write(frame(value, max), e => e ? reject(e) : resolve()));
const nativeMethods = Object.freeze({
  'harness.probe': ['probe', ['context']],
  'harness.native_context': ['nativeContext', ['context']],
  'harness.capabilities': ['capabilities', ['version']],
  'harness.plan': ['plan', ['request', 'version', 'context']],
  'harness.command': ['command', ['request', 'profile', 'prompt_file', 'context']],
  'harness.normalize': ['normalize', null],
});
export function defineNativeHarness(options) {
  identity({ ...options, kind: 'harness' });
  if (Object.values(nativeMethods).some(([name]) => typeof options[name] !== 'function')) fail('invalid_definition');
  return Object.freeze({ ...options, kind: 'harness', native: true });
}
/** Deliberately reports only a typed rejection, never a provider/credential message. */
export function rejectNative(kind) { throw Object.assign(new Error('native_rejection'), { kind }); }
async function serveNative(plugin) {
  let expected = 1;
  const requestLimit = request => request?.method === 'harness.normalize' ? NATIVE_NORMALIZE_MAX_FRAME : NATIVE_MAX_FRAME;
  for await (const request of frames(process.stdin, NATIVE_NORMALIZE_MAX_FRAME, requestLimit)) {
    if (!exact(request, ['jsonrpc', 'id', 'method', 'params']) || request.jsonrpc !== '2.0' || request.id !== expected) fail('invalid_request');
    const { method, params } = request;
    let result;
    if (expected === 1) {
      if (method !== 'initialize' || !exact(params, ['protocol']) || params.protocol !== 1) fail('protocol_mismatch');
      result = { protocol: 1, ...identity(plugin) };
    } else if (expected === 2) {
      if (method !== 'health' || !exact(params, [])) fail('invalid_request');
      result = { ready: true };
    } else if (method === 'shutdown') {
      if (!exact(params, [])) fail('invalid_params');
      await write({ jsonrpc: '2.0', id: expected, result: { ready: true } }, NATIVE_MAX_FRAME);
      process.stdin.destroy();
      return;
    } else {
      const definition = Object.hasOwn(nativeMethods, method) && nativeMethods[method];
      if (!definition || !(definition[1] ? exact(params, definition[1]) : exact(params, ['event']) || exact(params, ['text']) && typeof params.text === 'string')) fail('invalid_params');
      try { result = { result: await plugin[definition[0]](params), failure: null }; }
      catch (e) {
        const kinds = ['unsupported_capability', 'unsupported_version', 'model_unsupported', 'model_unverified', 'unsupported_placement', 'invalid_profile', 'authorization_required', 'invalid_arguments', 'timeout'];
        result = { result: null, failure: kinds.includes(e?.kind) ? e.kind : 'invalid_arguments' };
      }
    }
    await write({ jsonrpc: '2.0', id: expected++, result }, NATIVE_MAX_FRAME);
  }
  fail('incomplete_exchange');
}

export const TARGET_ACTIONS = Object.freeze({
  prepare: [{ primitive: 'workspace.fork', strategy: 'clone_or_copy' }, { primitive: 'workspace.fork', strategy: 'copy' }],
  start: [{ primitive: 'process.spawn' }], inspect: [{ primitive: 'process.inspect' }],
  cancel: [{ primitive: 'process.terminate' }],
  collect: [{ primitive: 'artifacts.capture' }, { primitive: 'artifacts.recover' }],
  reclaim: [{ primitive: 'workspace.reclaim' }],
});
export const TARGET_ENVIRONMENT_ACTIONS = Object.freeze({
  ensure: [{ primitive: 'environment.ensure' }],
  inspect: [{ primitive: 'environment.inspect' }],
  destroy: [{ primitive: 'environment.destroy' }],
  workspace: TARGET_ACTIONS.prepare,
});
export function validTargetPlan(plan, job) {
  return exact(plan, ['grant', 'action']) && plan.grant === job.grant &&
    (exact(plan.action, ['primitive']) && plan.action.primitive === 'unsupported' ||
      job.actions.some(action => exact(plan.action, Object.keys(action)) &&
        Object.keys(action).every(key => action[key] === plan.action[key])));
}

async function serveTarget(plugin) {
  let id = 1, optional = [], selected = false, ticket = null, outcome = null, sequence = 0;
  const pending = new Set();
  const seen = new Set();
  const remote = () => optional.includes('remote.v1');
  try {
  for await (const request of frames(process.stdin)) {
    if (!exact(request, ['jsonrpc', 'id', 'method', 'params']) || request.jsonrpc !== '2.0' || request.id !== id) fail('invalid_request');
    const { method, params } = request;
    let result;
    if (id === 1) {
      if (method !== 'initialize' || !exact(params, ['protocol']) || params.protocol !== 1) fail('protocol_mismatch');
      result = { protocol: 1, ...identity(plugin) };
    } else if (id === 2) {
      if (method !== 'health' || !exact(params, [])) fail('invalid_request');
      result = { ready: true };
    } else if (method === 'shutdown') {
      if (!exact(params, []) || pending.size || ticket !== null) fail('invalid_params');
      await plugin.close?.();
      await write({ jsonrpc: '2.0', id, result: { ready: true } });
      process.stdin.destroy();
      return;
    } else if (method === 'target.capabilities') {
      if (!exact(params, [])) fail('invalid_params');
      result = await plugin.capabilities();
      if (!exact(result, ['placements', 'operations', 'optional']) ||
          JSON.stringify(result.placements) !== JSON.stringify([plugin.id]) ||
          JSON.stringify(result.operations) !== JSON.stringify(TARGET_OPERATIONS) ||
          !['[]', '["environment.v1"]', '["environment.v1","pty.v1"]', '["environment.v1","pty.v1","remote.v1"]'].includes(JSON.stringify(result.optional))) fail('unsupported_target_capabilities');
      optional = result.optional;
      if (remote() && typeof plugin.remote !== 'function') fail('invalid_definition');
    } else if (method === 'target.placement') {
      if (!exact(params, ['placement', 'backends']) || params.placement !== plugin.id ||
          JSON.stringify(params.backends) !== JSON.stringify([remote() ? 'remote.v1' : 'host.local'])) fail('invalid_params');
      result = await plugin.placement(params);
      if (!exact(result, ['backend']) || ![remote() ? 'remote.v1' : 'host.local', 'unsupported'].includes(result.backend)) fail('invalid_target_placement');
      selected = result.backend !== 'unsupported';
    } else if (method === 'target.environment.capabilities') {
      if (!selected || !optional.length || !exact(params, ['version']) || params.version !== 1) fail('invalid_params');
      result = { version: 1, persistent_home: true, interactive_exec_pty: optional.includes('pty.v1'),
        infrastructure_diagnostics: remote(), operations: ['ensure', 'inspect', 'destroy', 'workspace'] };
    } else if (method === 'target.remote.invoke') {
      if (!selected || !remote() || ticket !== null || !exact(params, ['version', 'operation', 'input']) ||
          params.version !== 1 || !REMOTE_OPERATIONS.includes(params.operation)) fail('invalid_params');
      ticket = String(++sequence);
      outcome = null;
      Promise.resolve().then(() => plugin.remote(params.operation, params.input)).then(
        value => { outcome = { state: 'complete', result: value }; },
        failure => { outcome = { state: 'failed', scope: failure?.scope === 'harness' ? 'harness' : 'target',
          kind: typeof failure?.kind === 'string' ? failure.kind : 'output' }; });
      result = { ticket };
    } else if (method === 'target.remote.poll') {
      if (!remote() || ticket === null || !exact(params, ['ticket']) || params.ticket !== ticket) fail('invalid_params');
      result = outcome ?? { state: 'pending' };
      if (outcome !== null) { ticket = null; outcome = null; }
    } else if (method.startsWith('target.pty.')) {
      const operation = method.slice('target.pty.'.length);
      if (remote() || !selected || !optional.includes('pty.v1') ||
          !['openExec', 'exchange', 'close'].includes(operation) || typeof plugin.pty?.[operation] !== 'function') fail('invalid_params');
      result = await plugin.pty[operation](params);
    } else if (method === 'target.completed') {
      if (remote()) fail('host_grants_forbidden_for_remote_backend');
      if (!exact(params, ['grant', 'evidence']) || !pending.delete(params.grant) ||
          !params.evidence || Array.isArray(params.evidence) || typeof params.evidence !== 'object') fail('invalid_params');
      result = await plugin.completed(params);
      if (!exact(result, ['received']) || result.received !== params.grant) fail('invalid_target_receipt');
    } else {
      if (remote()) fail('host_grants_forbidden_for_remote_backend');
      const environment = method.startsWith('target.environment.');
      const operation = method.slice(environment ? 'target.environment.'.length : 7);
      const actions = environment ? TARGET_ENVIRONMENT_ACTIONS : TARGET_ACTIONS;
      const identityKey = environment && operation !== 'workspace' ? 'environment_id' : 'run_id';
      const callback = environment ? plugin.environment?.[operation] : plugin[operation];
      if (!Object.hasOwn(actions, operation) || typeof callback !== 'function' ||
          environment && (!selected || !optional.includes('environment.v1')) ||
          !exact(params, ['placement', identityKey, 'grant', 'actions', 'context']) ||
          params.placement !== plugin.id || typeof params[identityKey] !== 'string' ||
          !params[identityKey].length || params[identityKey].length > 128 || typeof params.grant !== 'string' ||
          !params.grant.length || params.grant.length > 128 || /[^A-Za-z0-9_-]/.test(params.grant) || seen.has(params.grant) || !Array.isArray(params.actions) ||
          !params.actions.length || !params.actions.every(action => actions[operation].some(
            allowed => exact(action, Object.keys(allowed)) && Object.keys(allowed).every(key => allowed[key] === action[key]))) ||
          !params.context || Array.isArray(params.context) || typeof params.context !== 'object') fail('invalid_params');
      seen.add(params.grant);
      result = await callback(params);
      if (!validTargetPlan(result, params)) fail('invalid_target_proposal');
      if (result.action.primitive !== 'unsupported') pending.add(params.grant);
    }
    await write({ jsonrpc: '2.0', id: id++, result });
  }
  } finally {
    await plugin.close?.();
  }
  fail('incomplete_exchange');
}


async function serveConnector(plugin) {
  let id = 1, verified, normalized = false;
  for await (const request of frames(process.stdin)) {
    if (!exact(request, ['jsonrpc', 'id', 'method', 'params']) || request.jsonrpc !== '2.0' || request.id !== id) fail('invalid_request');
    const { method, params } = request;
    let result;
    if (id === 1) {
      if (method !== 'initialize' || !exact(params, ['protocol']) || params.protocol !== 1) fail('protocol_mismatch');
      result = { protocol: 1, ...identity(plugin) };
    } else if (id === 2) {
      if (method !== 'health' || !exact(params, [])) fail('invalid_request');
      result = { ready: true };
    } else if (id === 3) {
      if (method !== 'connector.capabilities' || !exact(params, [])) fail('invalid_request');
      result = resultFor('connector', await plugin.capabilities());
      if (JSON.stringify([...result.optional].sort()) !== JSON.stringify(['subscribe', 'reply'].filter(name => typeof plugin[name] === 'function').sort())) fail('invalid_definition');
    } else if (method === 'shutdown') {
      if (!exact(params, [])) fail('invalid_params');
      await write({ jsonrpc: '2.0', id, result: { ready: true } });
      process.stdin.destroy();
      return;
    } else if (id === 4 && method === 'connector.verify') {
      const grant = JSON.stringify(params);
      result = validConnectorInput(params) ? await plugin.verify(params) : { verified: false };
      if (!exact(result, ['verified']) || typeof result.verified !== 'boolean') fail('invalid_verification');
      // Keep immutable bytes before normalization; callbacks cannot mutate the verification grant.
      if (result.verified) verified = grant;
    } else if (id === 5 && method === 'connector.normalize' && verified && !normalized) {
      if (JSON.stringify(params) !== verified) fail('invalid_normalization_grant');
      normalized = true;
      result = await plugin.normalize(params);
      if (!validNormalizedTrigger(result)) fail('invalid_trigger');
    } else if (id === 4 && ['connector.subscribe', 'connector.reply'].includes(method)) {
      const name = method.slice(10);
      if (typeof plugin[name] !== 'function' || !exact(params, ['version', 'data']) || params.version !== 1) fail('unsupported_operation');
      result = await plugin[name](params);
      if (!exact(result, ['version', 'data']) || result.version !== 1) fail('invalid_result');
    } else fail('invalid_request');
    await write({ jsonrpc: '2.0', id: id++, result });
  }
  fail('incomplete_exchange');
}
/** Transport adapter only. Fabric owns launch, cancellation, workspaces and artifacts. */
export async function serve(plugin) {
  try {
    const args = process.argv.slice(2);
    if (plugin.native) {
      if (args.length !== 1 || args[0] !== '--fabric-plugin-stdio') fail('invalid_mode');
      return await serveNative(plugin);
    }
    if (args[0] === '--fabric-plugin-run' && args.length === 2 && plugin.kind === 'harness') {
      const prompt = new TextDecoder('utf-8', { fatal: true }).decode(readBounded(args[1], 1024 * 1024));
      await plugin.run({ prompt, emit: async event => {
        if (!event || typeof event.type !== 'string') fail('invalid_event');
        await write(event);
      } });
      return;
    }
    if (args.length !== 1 || args[0] !== '--fabric-plugin-stdio') fail('invalid_mode');
    if (plugin.kind === 'target') return await serveTarget(plugin);
    if (plugin.kind === 'connector') return await serveConnector(plugin);
    let step = 0;
    for await (const request of frames(process.stdin)) {
      const methods = ['initialize', 'health', 'harness.plan', 'shutdown'];
      if (!exact(request, ['jsonrpc', 'id', 'method', 'params']) || request.jsonrpc !== '2.0' || request.id !== step + 1 || request.method !== methods[step]) fail('invalid_request');
      const params = request.params;
      const expected = step === 0 ? { protocol: PROTOCOL_VERSION } : {};
      if (!exact(params, Object.keys(expected)) || Object.keys(expected).some(k => params[k] !== expected[k])) fail(step === 0 ? 'protocol_mismatch' : 'invalid_params');
      let result;
      try {
        if (step === 0) result = { protocol: PROTOCOL_VERSION, ...identity(plugin) };
        else if (step === 2) result = resultFor(plugin.kind, await plugin.plan(params));
        else result = { ready: true };
      } catch {
        await write({ jsonrpc: '2.0', id: request.id, error: { code: -32000, message: 'plugin_failed' } });
        process.exitCode = 1;
        return;
      }
      await write({ jsonrpc: '2.0', id: request.id, result });
      if (++step === 4) {
        process.stdin.destroy();
        return;
      }
    }
    if (step !== 4) fail('incomplete_exchange');
  } catch {
    process.stderr.write('Fabric plugin: invalid input or failed operation.\n');
    process.exitCode = 1;
  }
}
