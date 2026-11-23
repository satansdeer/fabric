export const SDK_VERSION: '0.1.0';
export const PROTOCOL_VERSION: 1;
export type PluginKind = 'harness' | 'target' | 'connector';
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export interface PluginIdentity { id: string; version: string; kind: PluginKind }
export interface Ready { ready: boolean }
/** Opaque host-owned run identity, never a path, command, environment or deletion grant. */
export type TargetAction =
  | { primitive: 'workspace.fork'; strategy: 'clone_or_copy' | 'copy' }
  | { primitive: 'process.spawn' | 'process.inspect' | 'process.terminate' | 'artifacts.capture' | 'artifacts.recover' | 'workspace.reclaim' | 'unsupported' };
export interface TargetJob { placement: string; run_id: string; grant: string; actions: TargetAction[]; context: { [key: string]: Json } }
export interface TargetPlacement { placement: string; backends: ['host.local'] }
export interface TargetCompleted { grant: string; evidence: { [key: string]: Json } }
export type TargetOperation = 'prepare' | 'start' | 'inspect' | 'cancel' | 'collect' | 'reclaim';
export const TARGET_OPERATIONS: readonly TargetOperation[];
export interface TargetCapabilities { placements: string[]; operations: readonly TargetOperation[]; optional: string[] }
export interface TargetProposal { grant: string; action: TargetAction }
export const TARGET_ACTIONS: Readonly<Record<TargetOperation, readonly TargetAction[]>>;
export function validTargetPlan(plan: unknown, job: TargetJob): boolean;
/** Evidence is availability only, never a credential or authorization. */
export interface ConnectorPayload { available: boolean }
export type HarnessEvent = { type: string; [key: string]: Json };
export interface HarnessContext { prompt: string; emit(event: HarnessEvent): Promise<void> }
type Result<T> = T | Promise<T>;
interface Identity { id: string; version: string }
export interface HarnessDefinition extends Identity {
  plan(params: Record<string, never>): Result<Ready>;
  run(context: HarnessContext): Result<void>;
}
export type TargetDefinition = Identity & {
  capabilities(): Result<TargetCapabilities>;
  placement(request: TargetPlacement): Result<{ backend: 'host.local' | 'unsupported' }>;
  completed(result: TargetCompleted): Result<{ received: string }>;
} & { [K in TargetOperation]: (job: TargetJob) => Result<TargetProposal> };
export interface ConnectorDefinition extends Identity { evidence(params: Record<string, never>): Result<ConnectorPayload> }
export type Harness = Readonly<HarnessDefinition & { kind: 'harness' }>;
export type Target = Readonly<TargetDefinition & { kind: 'target' }>;
export type Connector = Readonly<ConnectorDefinition & { kind: 'connector' }>;
export function defineHarness(options: HarnessDefinition): Harness;
export function defineTarget(options: TargetDefinition): Target;
export function defineConnector(options: ConnectorDefinition): Connector;
export function serve(plugin: Harness | NativeHarness | Target | Connector): Promise<void>;
export interface InitializeResult extends PluginIdentity { protocol: 1 }
export type ControlMethod = 'initialize' | 'health' | 'harness.plan' | 'target.capabilities' | 'target.placement' | 'target.completed' | `target.${TargetOperation}` | 'connector.evidence' | 'shutdown';
export type ControlRequest = { jsonrpc: '2.0'; id: number } & (
  { method: 'initialize'; params: { protocol: 1 } } |
  { method: `target.${TargetOperation}`; params: TargetJob } |
  { method: 'target.placement'; params: TargetPlacement } |
  { method: 'target.completed'; params: TargetCompleted } |
  { method: Exclude<ControlMethod, 'initialize' | 'target.placement' | 'target.completed' | `target.${TargetOperation}`>; params: Record<string, never> }
);
export type ControlResponse<T> = { jsonrpc: '2.0'; id: number } & ({ result: T; error?: never } | { error: { code: number; message: string }; result?: never });
export interface PluginManifest extends PluginIdentity {
  protocol: { min: number; max: number };
  executable: string;
  args: string[];
  capabilities: ('harness.run' | 'target.execute' | 'connector.evidence')[];
  integrity?: { root: string; sha256: string };
  native?: NativeMetadata;
}

/** Native protocol is opt-in through content-pinned manifest metadata. */
export interface NativeMetadata {
  executable: string; root_environment: string[];
  fingerprint: { domain: string; files: string[]; roots: [string, string][] };
}
export interface NativeCallContext {
  binary?: string | null;
  environment: Record<string, string>;
  /** Explicitly selected credential availability only. Values never cross IPC. */
  credentials: Record<string, boolean>;
}
export interface NativeCapability {
  capability: string;
  support: 'native' | 'fabric-managed' | 'unsupported' | 'requires-configuration';
  reason: { code: string; message: string };
  minimum_version: string | null;
}
export interface NativeCapabilities { harness_id: string; exact_version: string; supported_versions: string; capabilities: NativeCapability[] }
export interface NativeProfile {
  harness_id: string; harness_version: string;
  native_config_refs: { kind: string; path: string }[];
  model_policy: { kind: 'native-default' } | { kind: 'exact'; model: string };
  skills: string[]; plugins: string[]; mcp: string[];
  connection_refs: { kind: string; name: string }[];
}
export interface NativeRequest {
  profile?: NativeProfile | null; model?: string | null; required_capabilities?: string[];
  placement?: string; access?: 'edit' | 'inspect' | 'publish';
  hard_read_only?: boolean; resume?: string | null; max_cost_usd?: number | null;
}
export interface ProfileEvidence {
  digest: string; native_config_fingerprint: string; portable: boolean;
  diagnostics: { code: string; message: string }[];
}
export interface NativePlan {
  plan: { request: NativeRequest; manifest: NativeCapabilities; model_resolved: string | null; profile: ProfileEvidence };
  supports_cost_limit: boolean;
}
export interface NativeCommand {
  program: string;
  args: (string | { prompt: true })[];
  /** Selected credentials use the literal fabric-credential-available sentinel. */
  environment: [string, string][];
}
export type NativeEvent =
  | { type: 'output'; stream: 'stdout' | 'stderr'; text: string }
  | { type: 'tool'; name: string; phase: 'start' | 'update' | 'end' }
  | { type: 'usage'; input_tokens?: number | null; output_tokens?: number | null; cached_input_tokens?: number | null; cost_usd?: number | null }
  | { type: 'failure'; code: string; message: string };
export interface NativeHarnessDefinition extends Identity {
  probe(params: { context: NativeCallContext }): Result<{ version: string }>;
  nativeContext(params: { context: NativeCallContext }): Result<{ profile: NativeProfile; credential_paths: string[]; credential_available: boolean; exposure: string }>;
  capabilities(params: { version: string }): Result<NativeCapabilities>;
  plan(params: { request: NativeRequest; version: string; context: NativeCallContext }): Result<NativePlan>;
  command(params: { request: NativeRequest; profile: ProfileEvidence; prompt_file: string; context: NativeCallContext }): Result<NativeCommand>;
  normalize(params: { event: Json } | { text: string }): Result<{ event: NativeEvent | null }>;
}
export type NativeHarness = Readonly<NativeHarnessDefinition & { kind: 'harness'; native: true }>;
export function defineNativeHarness(options: NativeHarnessDefinition): NativeHarness;
export function rejectNative(kind: 'unsupported_capability' | 'unsupported_version' | 'model_unsupported' | 'model_unverified' | 'unsupported_placement' | 'invalid_profile' | 'authorization_required' | 'invalid_arguments'): never;
export type NativeControlRequest = { jsonrpc: '2.0'; id: number } & (
  { method: 'initialize'; params: { protocol: 1 } } |
  { method: 'health' | 'shutdown'; params: Record<string, never> } |
  { [K in keyof Pick<NativeHarnessDefinition, 'probe' | 'nativeContext' | 'capabilities' | 'plan' | 'command' | 'normalize'>]:
    { method: `harness.${K extends 'nativeContext' ? 'native_context' : K}`; params: Parameters<NativeHarnessDefinition[K]>[0] }
  }[keyof Pick<NativeHarnessDefinition, 'probe' | 'nativeContext' | 'capabilities' | 'plan' | 'command' | 'normalize'>]
);
