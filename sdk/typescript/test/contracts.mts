import { defineHarness, defineTarget, defineConnector, serve, type ControlResponse, type PluginManifest } from '../index.mjs';

const target = defineTarget({ id: 'typed-target', version: '1.0.0', plan: job => job });
const connector = defineConnector({ id: 'typed-connector', version: '1.0.0', evidence: () => ({ available: false }) });
const harness = defineHarness({ id: 'typed-harness', version: '1.0.0', plan: () => ({ ready: true }), run: async ({ prompt, emit }) => { await emit({ type: 'text', text: prompt }); } });
void serve(target);
void serve(connector);
void serve(harness);
// @ts-expect-error Remote placement is not part of protocol v1.
defineTarget({ id: 'bad', version: '1.0.0', plan: () => ({ placement: 'cloud' }) });
// @ts-expect-error Connector availability cannot authorize worker execution.
defineConnector({ id: 'bad', version: '1.0.0', evidence: () => ({ available: 'yes' }) });
// @ts-expect-error A harness requires an execution callback.
defineHarness({ id: 'bad', version: '1.0.0', plan: () => ({ ready: true }) });
// @ts-expect-error Responses cannot contain both result and error.
const invalid: ControlResponse<null> = { jsonrpc: '2.0', id: 1, result: null, error: { code: 1, message: 'bad' } };
const manifest: PluginManifest = { id: 'typed-target', version: '1.0.0', kind: 'target', protocol: { min: 1, max: 1 }, executable: '/node', args: ['/plugin.ts'], capabilities: ['target.plan'], integrity: { root: '/package', sha256: 'pin' } };
void invalid;
void manifest;
