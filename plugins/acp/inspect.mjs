import { realpathSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { readBounded } from './sdk/protocol.mjs';
import { absolute, validate, VERSION, sha256, fail } from './config.mjs';
import { launchAcp } from './transport.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const codes = new Set([
  'acp_inspect_arguments', 'acp_unsafe_path', 'acp_unsafe_file', 'acp_changed_file',
  'acp_invalid_config', 'acp_unsafe_executable', 'acp_launch_changed', 'acp_runtime_mismatch',
  'acp_spawn_failed', 'acp_invalid_initialize', 'acp_unsupported_protocol', 'acp_malformed_frame',
  'acp_truncated_frame', 'acp_frame_too_large', 'acp_invalid_envelope', 'acp_invalid_response_id',
  'acp_callback_overflow', 'acp_write_overflow', 'acp_deadline', 'acp_aborted',
  'acp_disconnected', 'acp_initialize_failed', 'acp_unsupported_request',
]);

/** Explicit installed-package inspection; never called by the plugin control plane. */
export async function main(args) {
  let connection, failure, stage = 'input';
  const result = { stage: 'failed', code: null, launch: null, cleanup: 'not_started',
    auth: 'unverified', model: 'unverified', peer_identity: 'untrusted' };
  const controller = new AbortController();
  const abort = () => { failure ??= 'acp_aborted'; controller.abort(); };
  process.on('SIGINT', abort); process.on('SIGTERM', abort);
  try {
    if (args.length !== 2 || args[0] !== '--cwd') fail('acp_inspect_arguments');
    const cwd = absolute(args[1]), directory = statSync(cwd);
    if (!directory.isDirectory() || directory.uid !== process.getuid()) fail('acp_unsafe_path');
    stage = 'launch_validation';
    const bytes = readBounded(join(here, 'launch.json'), 32_768);
    const config = validate(JSON.parse(bytes));
    if (realpathSync(process.execPath) !== config.runtime.path) fail('acp_runtime_mismatch');
    result.launch = { bridge: VERSION, config_sha256: sha256(bytes),
      executable_sha256: config.executable.sha256, runtime_sha256: config.runtime.sha256,
      resource_sha256: config.pins.map(pin => pin.sha256) };
    stage = 'initialize';
    connection = launchAcp({
      executable: config.executable.path, args: config.args, cwd,
      env: { HOME: config.nativeHome, LC_ALL: 'C' }, signal: controller.signal,
      initializeTimeoutMs: Math.min(config.requestTimeoutMs, 10_000),
      maxRecordBytes: 65_536, maxPendingRequests: 1, maxNotificationCallbacks: 16,
      failOnPeerRequest: true,
      onNotification() { /* Untrusted notifications are deliberately discarded, never retained or displayed. */ },
    });
    const metadata = await connection.initialize();
    result.protocolVersion = metadata.protocolVersion;
    stage = 'cleanup';
  } catch (error) {
    failure ??= codes.has(error.code) ? error.code : 'acp_inspection_failed';
  } finally {
    if (connection) {
      const closed = await connection.close();
      result.cleanup = 'reaped';
      if (closed.code !== 'acp_closed') failure ??= codes.has(closed.code) ? closed.code : 'acp_inspection_failed';
    }
    process.removeListener('SIGINT', abort); process.removeListener('SIGTERM', abort);
  }
  if (failure) { result.failed_stage = stage; result.code = failure; process.exitCode = 1; }
  else { result.stage = 'negotiated'; result.code = 'acp_initialized'; }
  // Only fixed labels and validated hashes/version; never peer metadata, paths or argv.
  process.stdout.on('error', () => { process.exitCode = 1; });
  process.stdout.write(JSON.stringify(result) + '\n');
}
