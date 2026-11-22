import { readBounded, identity, exact, frame, frames, resultFor, fail, PROTOCOL_VERSION, SDK_VERSION } from './protocol.mjs';

export { PROTOCOL_VERSION, SDK_VERSION };
function define(kind, options) {
  identity({ ...options, kind });
  if (typeof options[kind === 'connector' ? 'evidence' : 'plan'] !== 'function' || (kind === 'harness' && typeof options.run !== 'function')) fail('invalid_definition');
  return Object.freeze({ ...options, kind });
}
export const defineHarness = (options) => define('harness', options);
export const defineTarget = (options) => define('target', options);
export const defineConnector = (options) => define('connector', options);
const write = (value) => new Promise((resolve, reject) => process.stdout.write(frame(value), e => e ? reject(e) : resolve()));

/** Transport adapter only. Fabric owns launch, cancellation, workspaces and artifacts. */
export async function serve(plugin) {
  try {
    const args = process.argv.slice(2);
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
