import { readBounded, identity, exact, frame, frames, resultFor, fail, PROTOCOL_VERSION, SDK_VERSION, NATIVE_MAX_FRAME } from './protocol.mjs';

export { PROTOCOL_VERSION, SDK_VERSION };
function define(kind, options) {
  identity({ ...options, kind });
  if (typeof options[kind === 'connector' ? 'evidence' : 'plan'] !== 'function' || (kind === 'harness' && typeof options.run !== 'function')) fail('invalid_definition');
  return Object.freeze({ ...options, kind });
}
export const defineHarness = (options) => define('harness', options);
export const defineTarget = (options) => define('target', options);
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
    let step = 0;
    for await (const request of frames(process.stdin)) {
      const methods = ['initialize', 'health', plugin.kind === 'connector' ? 'connector.evidence' : `${plugin.kind}.plan`, 'shutdown'];
      if (!exact(request, ['jsonrpc', 'id', 'method', 'params']) || request.jsonrpc !== '2.0' || request.id !== step + 1 || request.method !== methods[step]) fail('invalid_request');
      const params = request.params;
      const expected = step === 0 ? { protocol: PROTOCOL_VERSION } : step === 2 && plugin.kind === 'target' ? { placement: 'local' } : {};
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
