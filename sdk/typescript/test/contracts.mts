import { defineHarness, defineTarget, defineConnector, serve, TARGET_OPERATIONS, type ControlResponse, type PluginManifest } from '../index.mjs';

const target = defineTarget({ id: 'typed-target', version: '1.0.0',
  capabilities: () => ({ placements: ['typed-target'], operations: TARGET_OPERATIONS, optional: [] }),
  placement: () => ({ backend: 'host.local' }),
  completed: ({ grant }) => ({ received: grant }),
  prepare: ({ grant }) => ({ grant, action: { primitive: 'workspace.fork', strategy: 'copy' } }),
  start: ({ grant }) => ({ grant, action: { primitive: 'process.spawn' } }),
  inspect: ({ grant }) => ({ grant, action: { primitive: 'process.inspect' } }),
  cancel: ({ grant }) => ({ grant, action: { primitive: 'process.terminate' } }),
  collect: ({ grant }) => ({ grant, action: { primitive: 'artifacts.capture' } }),
  reclaim: ({ grant }) => ({ grant, action: { primitive: 'workspace.reclaim' } }),
});
const connector = defineConnector({ id: 'typed-connector', version: '1.0.0', evidence: () => ({ available: false }) });
const harness = defineHarness({ id: 'typed-harness', version: '1.0.0', plan: () => ({ ready: true }), run: async ({ prompt, emit }) => { await emit({ type: 'text', text: prompt }); } });
void serve(target);
void serve(connector);
void serve(harness);
// @ts-expect-error Planning/readiness alone is not a target implementation.
defineTarget({ id: 'bad', version: '1.0.0', capabilities: () => ({ ready: true }) });
// @ts-expect-error Connector availability cannot authorize worker execution.
defineConnector({ id: 'bad', version: '1.0.0', evidence: () => ({ available: 'yes' }) });
// @ts-expect-error A harness requires an execution callback.
defineHarness({ id: 'bad', version: '1.0.0', plan: () => ({ ready: true }) });
// @ts-expect-error Responses cannot contain both result and error.
const invalid: ControlResponse<null> = { jsonrpc: '2.0', id: 1, result: null, error: { code: 1, message: 'bad' } };
const manifest: PluginManifest = { id: 'typed-target', version: '1.0.0', kind: 'target', protocol: { min: 1, max: 1 }, executable: '/node', args: ['/plugin.ts'], capabilities: ['target.execute'], integrity: { root: '/package', sha256: 'pin' } };
void invalid;
void manifest;
import type { NativeControlRequest, NativeCommand } from '../index.mjs';
const native: NativeControlRequest = { jsonrpc: '2.0', id: 3, method: 'harness.normalize', params: { text: 'fixture' } };
const command: NativeCommand = { program: '/fixture', args: ['--', { prompt: true }], environment: [] };
// @ts-expect-error Native IPC receives availability booleans, never credential strings.
const secret: NativeControlRequest = { jsonrpc: '2.0', id: 3, method: 'harness.probe', params: { context: { environment: {}, credentials: { KEY: 'secret' } } } };
void native; void command; void secret;
import type { TargetJob, TargetProposal, ControlRequest, TargetDefinition } from '../index.mjs';
const startJob: TargetJob<'start'> = { placement: 'typed-target', run_id: 'run', grant: 'grant', actions: [{ primitive: 'process.spawn' }], context: {} };
const startPlan: TargetProposal<'start'> = { grant: startJob.grant, action: startJob.actions[0]! };
const startRequest: ControlRequest = { jsonrpc: '2.0', id: 4, method: 'target.start', params: startJob };
// @ts-expect-error Start offers cannot fork a workspace.
const forkJob: TargetJob<'start'> = { ...startJob, actions: [{ primitive: 'workspace.fork', strategy: 'copy' }] };
// @ts-expect-error Start proposals cannot reclaim a workspace.
const reclaimPlan: TargetProposal<'start'> = { grant: 'grant', action: { primitive: 'workspace.reclaim' } };
// @ts-expect-error The public request union must correlate method and offered actions.
const reclaimRequest: ControlRequest = { jsonrpc: '2.0', id: 5, method: 'target.start', params: { ...startJob, actions: [{ primitive: 'workspace.reclaim' }] } };
// @ts-expect-error The actual target callback cannot return a fork plan for start.
const badStart: TargetDefinition['start'] = ({ grant }) => ({ grant, action: { primitive: 'workspace.fork', strategy: 'copy' } });
void startPlan; void startRequest; void forkJob; void reclaimPlan; void reclaimRequest; void badStart;
