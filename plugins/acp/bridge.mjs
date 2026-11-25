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
  'acp_turn_cancelled', 'acp_turn_incomplete', 'acp_output_overflow', 'acp_bridge_failed',
]);
const clean = text => text.replace(/[\x00-\x08\x0b-\x1f\x7f-\x9f]/g, '');
const all = ['headless.run', 'events.structured', 'model.override', 'session.resume', 'session.followup',
  'profile.import', 'skills.import', 'mcp.configure', 'execution.cancel', 'usage.report',
  'tool.approval.noninteractive', 'workspace.external', 'filesystem.patch.capture'];
const managed = ['execution.cancel', 'workspace.external', 'filesystem.patch.capture'];
const native = ['headless.run', 'events.structured'];

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
      if (fields(event, ['type', 'code']) && event.type === 'acp.failure' && codes.has(event.code)) {
        return { event: { type: 'failure', code: event.code, message: event.code } };
      }
      if (fields(event, ['type', 'reason']) && event.type === 'acp.stop' && event.reason === 'end_turn') return { event: null };
      rejectNative('invalid_arguments');
    },
  });
}

async function turn(config, prompt) {
  let connection, sessionId, pending = false, firstFailure;
  const controller = new AbortController();
  function emit(event) {
    const line = JSON.stringify(event) + '\n';
    if (process.stdout.writableLength + Buffer.byteLength(line) > 1_048_576) fail('acp_output_overflow');
    process.stdout.write(line);
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
  try {
    validate(config);
    if (realpathSync(process.execPath) !== config.runtime.path) fail('acp_bridge_failed');
    const cwd = realpathSync(process.cwd());
    connection = launchAcp({
      executable: config.executable.path, args: config.args, cwd,
      env: { HOME: config.nativeHome, LC_ALL: 'C' }, failOnPeerRequest: true,
      requestTimeoutMs: config.requestTimeoutMs,
      onNotification(message) {
        try {
          const params = message.params;
          if (message.method !== 'session/update' || !pending ||
              !fields(params, ['sessionId', 'update', '_meta']) || params.sessionId !== sessionId) fail('acp_invalid_update');
          const update = params.update;
          if (!object(update) || update.sessionUpdate !== 'agent_message_chunk') fail('acp_unsupported_update');
          if (!fields(update, ['sessionUpdate', 'content', '_meta']) ||
              !fields(update.content, ['type', 'text', '_meta']) || update.content.type !== 'text' ||
              typeof update.content.text !== 'string' || Buffer.byteLength(update.content.text) > 32_768) fail('acp_invalid_update');
          emit({ type: 'acp.text', text: clean(update.content.text) });
        } catch (error) { stop(codes.has(error.code) ? error.code : 'acp_bridge_failed'); }
      },
    });
    await connection.initialize(); // Metadata never grants consent, auth or model identity.
    const session = await connection.request('session/new', { cwd, mcpServers: [] });
    if (!fields(session, ['sessionId', 'modes', 'models', 'configOptions', '_meta']) ||
        typeof session.sessionId !== 'string' || !session.sessionId.length || Buffer.byteLength(session.sessionId) > 1024) fail('acp_invalid_session');
    sessionId = session.sessionId;
    pending = true;
    const result = await connection.request('session/prompt', { sessionId, prompt: [{ type: 'text', text: prompt }] },
      { signal: controller.signal, timeoutMs: null });
    pending = false;
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
