import { readBounded, identity, exact, frame, frames, resultFor, fail, PROTOCOL_VERSION, SDK_VERSION, NATIVE_MAX_FRAME } from './protocol.mjs';

export { PROTOCOL_VERSION, SDK_VERSION };
function define(kind, options) {
  identity({ ...options, kind });
  if (typeof options[kind === 'connector' ? 'evidence' : 'plan'] !== 'function' || (kind === 'harness' && typeof options.run !== 'function')) fail('invalid_definition');
  return Object.freeze({ ...options, kind });
}
export const defineHarness = (options) => define('harness', options);
export const TARGET_OPERATIONS = Object.freeze(['prepare', 'start', 'inspect', 'cancel', 'collect', 'reclaim']);
export function defineTarget(options) {
  identity({ ...options, kind: 'target' });
  if (['capabilities', ...TARGET_OPERATIONS].some(method => typeof options[method] !== 'function')) fail('invalid_definition');
  return Object.freeze({ ...options, kind: 'target' });
}
export const defineConnector = (options) => define('connector', options);
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
  for await (const request of frames(process.stdin, NATIVE_MAX_FRAME)) {
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
        const kinds = ['unsupported_capability', 'unsupported_version', 'model_unsupported', 'model_unverified', 'unsupported_placement', 'invalid_profile', 'authorization_required', 'invalid_arguments'];
        result = { result: null, failure: kinds.includes(e?.kind) ? e.kind : 'invalid_arguments' };
      }
    }
    await write({ jsonrpc: '2.0', id: expected++, result }, NATIVE_MAX_FRAME);
  }
  fail('incomplete_exchange');
}

async function serveTarget(plugin) {
  let id = 1;
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
      if (!exact(params, [])) fail('invalid_params');
      await write({ jsonrpc: '2.0', id, result: { ready: true } });
      process.stdin.destroy();
      return;
    } else if (method === 'target.capabilities') {
      if (!exact(params, [])) fail('invalid_params');
      result = await plugin.capabilities();
      if (!exact(result, ['placements', 'operations', 'optional']) ||
          JSON.stringify(result.placements) !== JSON.stringify([plugin.id]) ||
          JSON.stringify(result.operations) !== JSON.stringify(TARGET_OPERATIONS) ||
          !Array.isArray(result.optional) || result.optional.length) fail('unsupported_target_capabilities');
    } else {
      const operation = method.startsWith('target.') && method.slice(7);
      if (!TARGET_OPERATIONS.includes(operation) || !exact(params, ['placement', 'run_id']) ||
          params.placement !== plugin.id || typeof params.run_id !== 'string' ||
          !params.run_id.length || params.run_id.length > 128) fail('invalid_params');
      result = await plugin[operation](params);
      if (!exact(result, ['operation']) || !['unsupported', `local.${operation}`].includes(result.operation)) fail('invalid_target_proposal');
    }
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
    let step = 0;
    for await (const request of frames(process.stdin)) {
      const methods = ['initialize', 'health', plugin.kind === 'connector' ? 'connector.evidence' : `${plugin.kind}.plan`, 'shutdown'];
      if (!exact(request, ['jsonrpc', 'id', 'method', 'params']) || request.jsonrpc !== '2.0' || request.id !== step + 1 || request.method !== methods[step]) fail('invalid_request');
      const params = request.params;
      const expected = step === 0 ? { protocol: PROTOCOL_VERSION } : {};
      if (!exact(params, Object.keys(expected)) || Object.keys(expected).some(k => params[k] !== expected[k])) fail(step === 0 ? 'protocol_mismatch' : 'invalid_params');
      let result;
      try {
        if (step === 0) result = { protocol: PROTOCOL_VERSION, ...identity(plugin) };
        else if (step === 2) result = resultFor(plugin.kind, await plugin[plugin.kind === 'connector' ? 'evidence' : 'plan'](params));
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
