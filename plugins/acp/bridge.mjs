import { realpathSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { isDeepStrictEqual } from 'node:util';
import { defineNativeHarness, serve, rejectNative } from './sdk/index.mjs';
import { readBounded } from './sdk/protocol.mjs';
import { launchAcp } from './transport.mjs';
import { VERSION, validate, sha256, fail, fields, object } from './config.mjs';
import { main as inspect } from './inspect.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const codes = new Set([
  'acp_invalid_options', 'acp_spawn_failed', 'acp_not_ready', 'acp_invalid_request', 'acp_pending_limit',
  'acp_id_exhausted', 'acp_invalid_initialize', 'acp_unsupported_protocol', 'acp_malformed_frame',
  'acp_truncated_frame', 'acp_frame_too_large', 'acp_invalid_envelope', 'acp_invalid_response_id',
  'acp_callback_failed', 'acp_callback_overflow', 'acp_write_overflow', 'acp_deadline', 'acp_aborted',
  'acp_closed', 'acp_disconnected', 'acp_remote_error', 'acp_initialize_failed', 'acp_unsupported_request',
  'acp_invalid_session', 'acp_invalid_update', 'acp_unsupported_update', 'acp_invalid_stop',
  'acp_turn_cancelled', 'acp_turn_incomplete', 'acp_output_overflow', 'acp_extension_overflow', 'acp_tool_overflow', 'acp_bridge_failed',
]);
const clean = text => text.replace(/[\x00-\x08\x0b-\x1f\x7f-\x9f]/g, '');
const all = ['headless.run', 'events.structured', 'model.override', 'session.resume', 'session.followup',
  'profile.import', 'skills.import', 'mcp.configure', 'execution.cancel', 'usage.report',
  'tool.approval.noninteractive', 'workspace.external', 'filesystem.patch.capture'];
const managed = ['execution.cancel', 'workspace.external', 'filesystem.patch.capture'];
const native = ['headless.run', 'events.structured'];
// Only namespaced extensions, never unknown core methods, enter private evidence.
const extension = method => !method.startsWith('rpc.') &&
  /^[A-Za-z_][A-Za-z0-9_.-]*\/[A-Za-z_][A-Za-z0-9_./-]*$/.test(method) &&
  !['session', 'fs', 'terminal'].includes(method.split('/', 1)[0]);
const extensionLimits = { prefixBytes: 2048, recordBytes: 4096, turnBytes: 32_768, turnCount: 32 };
const toolLimit = 256;
const toolStatuses = ['pending', 'in_progress', 'completed', 'failed'];
const toolKinds = ['read', 'edit', 'delete', 'move', 'search', 'execute', 'think', 'fetch', 'switch_mode', 'other'];
const text = value => typeof value === 'string';
const optional = (value, key, check) => value[key] == null || check(value[key]);
const dataFields = (value, names) => fields(value, [...names, '_meta']) && optional(value, '_meta', object);
const terminal = status => status === 'completed' || status === 'failed';
const uint32 = value => Number.isInteger(value) && value >= 0 && value <= 0xffff_ffff;

function contentBlock(value) {
  if (!object(value) || !optional(value, 'annotations', annotations =>
    dataFields(annotations, ['audience', 'lastModified', 'priority']) &&
    optional(annotations, 'audience', roles => Array.isArray(roles) && roles.every(role => ['user', 'assistant'].includes(role))) &&
    optional(annotations, 'lastModified', text) && optional(annotations, 'priority', priority => typeof priority === 'number'))) return false;
  const common = ['type', 'annotations'];
  switch (value.type) {
    case 'text':
      return dataFields(value, [...common, 'text']) && text(value.text);
    case 'image':
    case 'audio':
      return dataFields(value, [...common, 'data', 'mimeType', ...(value.type === 'image' ? ['uri'] : [])]) &&
        text(value.data) && text(value.mimeType) && optional(value, 'uri', text);
    case 'resource_link':
      return dataFields(value, [...common, 'uri', 'name', 'mimeType', 'title', 'description', 'size']) &&
        text(value.uri) && text(value.name) && ['mimeType', 'title', 'description'].every(key => optional(value, key, text)) &&
        optional(value, 'size', size => Number.isSafeInteger(size) && size >= 0);
    case 'resource': {
      const resource = value.resource;
      return dataFields(value, [...common, 'resource']) && object(resource) && text(resource.uri) &&
        optional(resource, 'mimeType', text) &&
        ((dataFields(resource, ['uri', 'mimeType', 'text']) && text(resource.text)) ||
         (dataFields(resource, ['uri', 'mimeType', 'blob']) && text(resource.blob)));
    }
    default: return false;
  }
}

function toolContent(value) {
  if (!object(value)) return false;
  switch (value.type) {
    case 'content': return dataFields(value, ['type', 'content']) && contentBlock(value.content);
    case 'diff': return dataFields(value, ['type', 'path', 'oldText', 'newText']) &&
      text(value.path) && optional(value, 'oldText', text) && text(value.newText);
    case 'terminal': return dataFields(value, ['type', 'terminalId']) && text(value.terminalId);
    default: return false; // No subagents capability is advertised.
  }
}

function validTool(update) {
  const initial = update.sessionUpdate === 'tool_call';
  if (!dataFields(update, ['sessionUpdate', 'toolCallId', 'title', 'name', 'kind', 'status',
    'content', 'locations', 'rawInput', 'rawOutput']) || !text(update.toolCallId)) return false;
  if (initial && (!text(update.title) ||
      (Object.hasOwn(update, 'kind') && !toolKinds.includes(update.kind)) ||
      (Object.hasOwn(update, 'status') && !toolStatuses.includes(update.status)) ||
      (Object.hasOwn(update, 'content') && !Array.isArray(update.content)) ||
      (Object.hasOwn(update, 'locations') && !Array.isArray(update.locations)))) return false;
  // rawInput/rawOutput are arbitrary JSON, including null; no payload is retained.
  return optional(update, 'title', text) && optional(update, 'name', text) &&
    optional(update, 'kind', kind => toolKinds.includes(kind)) &&
    optional(update, 'status', status => toolStatuses.includes(status)) &&
    optional(update, 'content', content => Array.isArray(content) && content.every(toolContent)) &&
    optional(update, 'locations', locations => Array.isArray(locations) && locations.every(location =>
      dataFields(location, ['path', 'line']) && text(location.path) && optional(location, 'line', uint32)));
}

function plugin(config, configBytes) {
  const profile = {
    harness_id: 'acp', harness_version: VERSION,
    native_config_refs: [{ kind: 'native-home', path: config.nativeHome }],
    model_policy: { kind: 'native-default' }, skills: [], plugins: [], mcp: [], connection_refs: [],
  };
  const fingerprint = sha256(`fabric-acp-v1-${sha256(configBytes)}`);
  function identity(context) {
    try {
      validate(config);
      if (!context.binary || realpathSync(context.binary) !== realpathSync(join(here, 'fabric-acp-bridge')) ||
          realpathSync(process.execPath) !== config.runtime.path) rejectNative('invalid_arguments');
    } catch { rejectNative('invalid_arguments'); }
  }
  function capabilities(version) {
    if (version !== VERSION) rejectNative('unsupported_version');
    return { harness_id: 'acp', exact_version: VERSION, supported_versions: VERSION,
      capabilities: all.map(capability => ({ capability,
        support: managed.includes(capability) ? 'fabric-managed' : native.includes(capability) ? 'native' : 'unsupported',
        reason: { code: 'acp_text_bridge', message: 'Bounded local text bridge only; native provider acceptance and authentication are not asserted.' },
        minimum_version: null,
      })) };
  }
  function evidence(request) {
    if (request.model != null || request.profile?.model_policy?.kind === 'exact') rejectNative('model_unverified');
    if (request.placement !== 'local') rejectNative('unsupported_placement');
    if (request.access !== 'edit' || request.hard_read_only || request.resume != null || request.max_cost_usd != null ||
        request.required_capabilities.some(capability => !native.includes(capability) && !managed.includes(capability))) rejectNative('unsupported_capability');
    if (!isDeepStrictEqual(request.profile, profile)) rejectNative('authorization_required');
    return { digest: sha256(JSON.stringify([profile, 'acp', VERSION, null, fingerprint])),
      native_config_fingerprint: fingerprint, portable: false,
      diagnostics: [{ code: 'acp_auth_unverified', message: 'Native defaults only. Authentication and model identity are unknown; no credential values are inspected.' }] };
  }
  return defineNativeHarness({
    id: 'acp', version: '0.1.0',
    probe({ context }) { identity(context); return { version: VERSION }; },
    nativeContext({ context }) {
      identity(context);
      if (context.environment.FABRIC_ACP_HOME !== config.nativeHome) rejectNative('authorization_required');
      return { profile, credential_paths: [], credential_available: false,
        exposure: 'Explicit native HOME selected. ACP executable, literal argv, runtime and workspace cwd are package-bound. Native stores may be used by the agent; authentication/model readiness are unknown. No credential values are read by the bridge.' };
    },
    capabilities({ version }) { return capabilities(version); },
    plan({ request, version, context }) {
      identity(context);
      return { plan: { request, manifest: capabilities(version), model_resolved: null, profile: evidence(request) }, supports_cost_limit: false };
    },
    command({ request, profile: proof, context }) {
      identity(context);
      if (proof.digest !== evidence(request).digest) rejectNative('invalid_profile');
      return { program: context.binary, args: ['--turn', { prompt: true }], environment: [['HOME', config.nativeHome]] };
    },
    normalize(params) {
      const event = params.event;
      if (fields(event, ['type', 'text']) && event.type === 'acp.text' && typeof event.text === 'string' && Buffer.byteLength(event.text) <= 32_768) {
        return { event: { type: 'output', stream: 'stdout', text: clean(event.text) } };
      }
      if (fields(event, ['type', 'call', 'status', 'phase']) && event.type === 'acp.tool' &&
          Number.isInteger(event.call) && event.call >= 1 && event.call <= toolLimit &&
          [...toolStatuses, 'unknown'].includes(event.status) &&
          (event.phase === 'update' || (event.phase === 'end' && terminal(event.status)) ||
           (event.phase === 'start' && ['pending', 'in_progress'].includes(event.status)))) {
        return { event: { type: 'tool', name: `acp.tool.${event.call}.${event.status}`, phase: event.phase } };
      }
      if (fields(event, ['type', 'code']) && event.type === 'acp.failure' && codes.has(event.code)) {
        return { event: { type: 'failure', code: event.code, message: event.code === 'acp_unsupported_request'
          ? 'Unsupported blocking ACP peer request; no input or approval supplied.' : event.code } };
      }
      if (fields(event, ['type', 'reason']) && event.type === 'acp.stop' && event.reason === 'end_turn') return { event: null };
      rejectNative('invalid_arguments');
    },
  });
}

async function turn(config, prompt) {
  let connection, sessionId, pending = false, firstFailure;
  let phase = 'initialize', extensionCount = 0, extensionBytes = 0;
  const tools = new Map();
  const controller = new AbortController();
  function emit(event) {
    const line = JSON.stringify(event) + '\n';
    if (process.stdout.writableLength + Buffer.byteLength(line) > 1_048_576) fail('acp_output_overflow');
    process.stdout.write(line);
  }
  function retainExtension(message) {
    if (extensionCount >= extensionLimits.turnCount) fail('acp_extension_overflow');
    const bytes = Buffer.from(JSON.stringify(message));
    // Base64 encodes the bounded prefix; it does not redact private peer data.
    const line = JSON.stringify({ type: 'acp.extension', source: 'untrusted_acp_peer', phase,
      sequence: extensionCount + 1, notification_bytes: bytes.length, sha256: sha256(bytes),
      truncated: bytes.length > extensionLimits.prefixBytes,
      json_base64: bytes.subarray(0, extensionLimits.prefixBytes).toString('base64') }) + '\n';
    const size = Buffer.byteLength(line);
    if (size > extensionLimits.recordBytes || extensionBytes + size > extensionLimits.turnBytes) fail('acp_extension_overflow');
    extensionCount++;
    extensionBytes += size;
    process.stderr.write(line);
  }
  function reportTool(update) {
    if (!validTool(update)) fail('acp_invalid_update');
    // Fixed-size keys retain correlation, not peer IDs or metadata. Per turn only.
    const key = sha256(JSON.stringify(update.toolCallId));
    let tool = tools.get(key);
    if (!tool) {
      if (tools.size >= toolLimit) fail('acp_tool_overflow');
      tool = { call: tools.size + 1, status: 'unknown' };
      tools.set(key, tool);
    }
    const initial = update.sessionUpdate === 'tool_call';
    tool.status = update.status ?? (initial ? 'pending' : tool.status);
    // A delta does not imply a missing start; end_turn does not imply tool completion.
    const phase = (initial || update.status != null) && terminal(tool.status) ? 'end' : initial ? 'start' : 'update';
    emit({ type: 'acp.tool', call: tool.call, status: tool.status, phase });
  }
  function stop(code) {
    firstFailure ??= code;
    if (pending && sessionId) {
      try { connection.notify('session/cancel', { sessionId }); } catch { /* Teardown still owns the child. */ }
    }
    controller.abort();
    void connection?.cancel();
  }
  const abort = () => stop('acp_aborted');
  const outputError = () => stop('acp_output_overflow');
  process.on('SIGTERM', abort); process.on('SIGINT', abort);
  process.stdout.on('error', outputError);
  process.stderr.on('error', outputError);
  try {
    validate(config);
    if (realpathSync(process.execPath) !== config.runtime.path) fail('acp_bridge_failed');
    const cwd = realpathSync(process.cwd());
    connection = launchAcp({
      executable: config.executable.path, args: config.args, cwd,
      env: { HOME: config.nativeHome, LC_ALL: 'C' }, failOnPeerRequest: true,
      requestTimeoutMs: config.requestTimeoutMs,
      onNotification(message, _signal, requestPendingAtReceipt) {
        try {
          if (message.method !== 'session/update') {
            if (!extension(message.method)) fail('acp_unsupported_update');
            retainExtension(message);
            return;
          }
          const params = message.params;
          if (!pending || !requestPendingAtReceipt || !dataFields(params, ['sessionId', 'update']) ||
              params.sessionId !== sessionId) fail('acp_invalid_update');
          const update = params.update;
          if (!object(update)) fail('acp_invalid_update');
          if (['tool_call', 'tool_call_update'].includes(update.sessionUpdate)) {
            reportTool(update);
            return;
          }
          if (update.sessionUpdate !== 'agent_message_chunk') fail('acp_unsupported_update');
          if (!fields(update, ['sessionUpdate', 'content', '_meta']) ||
              !fields(update.content, ['type', 'text', '_meta']) || update.content.type !== 'text' ||
              typeof update.content.text !== 'string' || Buffer.byteLength(update.content.text) > 32_768) fail('acp_invalid_update');
          emit({ type: 'acp.text', text: clean(update.content.text) });
        } catch (error) { stop(codes.has(error.code) ? error.code : 'acp_bridge_failed'); }
      },
    });
    await connection.initialize(); // Metadata never grants consent, auth or model identity.
    phase = 'session_new';
    const session = await connection.request('session/new', { cwd, mcpServers: [] });
    if (!fields(session, ['sessionId', 'modes', 'models', 'configOptions', '_meta']) ||
        typeof session.sessionId !== 'string' || !session.sessionId.length || Buffer.byteLength(session.sessionId) > 1024) fail('acp_invalid_session');
    sessionId = session.sessionId;
    pending = true;
    phase = 'prompt';
    const result = await connection.request('session/prompt', { sessionId, prompt: [{ type: 'text', text: prompt }] },
      { signal: controller.signal, timeoutMs: null });
    pending = false;
    phase = 'closing';
    // Drain notifications already queued behind the response before accepting it.
    await new Promise(resolve => setImmediate(resolve));
    if (firstFailure) fail(firstFailure);
    if (!fields(result, ['stopReason', '_meta']) || typeof result.stopReason !== 'string') fail('acp_invalid_stop');
    if (result.stopReason === 'cancelled') fail('acp_turn_cancelled');
    if (['max_tokens', 'max_turn_requests', 'refusal'].includes(result.stopReason)) fail('acp_turn_incomplete');
    if (result.stopReason !== 'end_turn') fail('acp_invalid_stop');
    const closure = await connection.close();
    if (closure.code !== 'acp_closed') fail(closure.code);
    if (firstFailure) fail(firstFailure);
    emit({ type: 'acp.stop', reason: 'end_turn' });
  } catch (error) {
    firstFailure ??= codes.has(error.code) ? error.code : 'acp_bridge_failed';
    try { emit({ type: 'acp.failure', code: firstFailure }); } catch { /* Broken capture cannot accept diagnostics. */ }
  } finally {
    await connection?.close();
    process.removeListener('SIGTERM', abort); process.removeListener('SIGINT', abort);
    process.stdout.removeListener('error', outputError);
    process.stderr.removeListener('error', outputError);
  }
  if (firstFailure) process.exitCode = 1;
}

export async function main() {
  try {
    if (process.argv[2] === '--inspect') { await inspect(process.argv.slice(3)); return; }
    const bytes = readBounded(join(here, 'launch.json'), 32_768);
    const config = JSON.parse(bytes);
    if (process.argv.length === 3 && process.argv[2] === '--fabric-plugin-stdio') await serve(plugin(config, bytes));
    else if (process.argv.length === 4 && process.argv[2] === '--turn') await turn(config, process.argv[3]);
    else fail('acp_bridge_failed');
  } catch {
    // Never print peer errors, launch arguments, paths, environment or native stores.
    console.error('acp_bridge_failed'); process.exitCode = 1;
  }
}
