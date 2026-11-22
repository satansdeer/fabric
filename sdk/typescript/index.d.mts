export const SDK_VERSION: '0.1.0';
export const PROTOCOL_VERSION: 1;
export type PluginKind = 'harness' | 'target' | 'connector';
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export interface PluginIdentity { id: string; version: string; kind: PluginKind }
export interface Ready { ready: boolean }
/** Protocol v1 only proposes local placement; this is not worker admission. */
export interface TargetJob { placement: 'local' }
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
export interface TargetDefinition extends Identity { plan(job: TargetJob): Result<TargetJob> }
export interface ConnectorDefinition extends Identity { evidence(params: Record<string, never>): Result<ConnectorPayload> }
export type Harness = Readonly<HarnessDefinition & { kind: 'harness' }>;
export type Target = Readonly<TargetDefinition & { kind: 'target' }>;
export type Connector = Readonly<ConnectorDefinition & { kind: 'connector' }>;
export function defineHarness(options: HarnessDefinition): Harness;
export function defineTarget(options: TargetDefinition): Target;
export function defineConnector(options: ConnectorDefinition): Connector;
export function serve(plugin: Harness | Target | Connector): Promise<void>;
export interface InitializeResult extends PluginIdentity { protocol: 1 }
export type ControlMethod = 'initialize' | 'health' | 'harness.plan' | 'target.plan' | 'connector.evidence' | 'shutdown';
export type ControlRequest = { jsonrpc: '2.0'; id: number } & (
  { method: 'initialize'; params: { protocol: 1 } } |
  { method: 'target.plan'; params: TargetJob } |
  { method: Exclude<ControlMethod, 'initialize' | 'target.plan'>; params: Record<string, never> }
);
export type ControlResponse<T> = { jsonrpc: '2.0'; id: number } & ({ result: T; error?: never } | { error: { code: number; message: string }; result?: never });
export interface PluginManifest extends PluginIdentity {
  protocol: { min: number; max: number };
  executable: string;
  args: string[];
  capabilities: ('harness.run' | 'target.plan' | 'connector.evidence')[];
  integrity?: { root: string; sha256: string };
}
