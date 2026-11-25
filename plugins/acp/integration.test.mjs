import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync, readdirSync, chmodSync, rmSync, realpathSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { prepare } from './package.mjs';
import { fileHash, sha256, VERSION } from './config.mjs';
import { create, install } from '../../sdk/typescript/cli.mjs';
import { directory, pack, archive } from '../../sdk/typescript/package.mjs';

const source = realpathSync(join(dirname(fileURLToPath(import.meta.url)), '../..'));
const host = process.env.FABRIC_ACP_TEST_HOST ?? join(source, 'target/debug/fabric');
const node = realpathSync(process.execPath);
function remove(path) {
  if (!existsSync(path)) return;
  chmodSync(path, 0o700);
  for (const entry of readdirSync(path, { withFileTypes: true })) if (entry.isDirectory()) remove(join(path, entry.name));
  rmSync(path, { recursive: true, force: true });
}
const lines = path => existsSync(path) ? readFileSync(path, 'utf8').trim().split('\n').filter(Boolean).map(line => JSON.parse(line)) : [];
function reaped(pid) {
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' }, `owned child ${pid} still alive`);
}

test('installed ACP package through the real Fabric host (no providers)', { timeout: 360_000 }, async t => {
  assert.ok(existsSync(host), 'Real Fabric host required; set FABRIC_ACP_TEST_HOST to an absolute built host. Never skipped.');
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fabric-acp-')));
  const registry = join(root, 'registry'), home = join(root, 'native-home'), repo = join(root, 'repo');
  const log = join(root, 'children.jsonl'), storage = join(root, 'storage');
  const children = new Set();
  t.after(async () => {
    await Promise.all([...children].map(child => new Promise(resolve => {
      child.once('close', resolve); child.kill('SIGTERM');
    })));
    for (const record of lines(log)) if (record.pid) {
      const command = spawnSync('/bin/ps', ['-o', 'command=', '-p', String(record.pid)], { encoding: 'utf8' }).stdout;
      if (command.includes(log)) {
        try { process.kill(record.pid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
      }
    }
    remove(root);
  });
  mkdirSync(registry, { mode: 0o700 }); mkdirSync(home, { mode: 0o700 });
  const agent = join(root, 'turn-agent.mjs');
  writeFileSync(agent, readFileSync(join(source, 'plugins/acp/fixtures/turn-agent.mjs')));
  const config = { executable: { path: node, sha256: fileHash(node) }, args: [agent, log],
    nativeHome: home, cwd: 'fabric-workspace', pins: [{ path: agent, sha256: fileHash(agent) }], requestTimeoutMs: 2000 };
  const pkg = join(root, 'package');
  const snapshot = prepare(config, pkg);
  const second = prepare(config, join(root, 'second-package'));
  assert.equal(snapshot.hash, second.hash);
  assert.deepEqual(pack(snapshot), pack(second));
  assert.equal(archive(pack(snapshot)).hash, snapshot.hash);
  assert.throws(() => prepare({ ...config, env: { NODE_OPTIONS: 'unsafe' } }, join(root, 'invalid')));
  assert.throws(() => prepare({ ...config, executable: { path: node, sha256: '0'.repeat(64) } }, join(root, 'changed')));
  install(pkg, { registry, fabric: host, version: '0.1.0', sha256: snapshot.hash, trust: true });
  const bridgeDir = join(registry, 'packages', snapshot.hash);
  // Core's real local worker/target primitives, not an SDK-only scheduler mock.
  const target = create('target', join(root, 'local'));
  install(target, { registry, fabric: host, version: '1.0.0', sha256: directory(target).hash, trust: true });
  const env = { HOME: root, TMPDIR: root, LC_ALL: 'C', PATH: `${bridgeDir}:${dirname(node)}:/usr/bin:/bin`,
    FABRIC_PLUGIN_REGISTRY: registry, FABRIC_ACP_HOME: home,
    UNSELECTED_SECRET: 'SECRET_ENV_CANARY', NODE_OPTIONS: '--require=/SECRET_LOADER_CANARY',
    GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null' };
  function invoke(program, args, { cwd = root, success = true } = {}) {
    const result = spawnSync(program, args, { cwd, env, encoding: 'utf8', timeout: 60_000, maxBuffer: 2_097_152 });
    assert.ifError(result.error);
    if (success) assert.equal(result.status, 0, result.stdout + result.stderr);
    else assert.notEqual(result.status, 0, result.stdout + result.stderr);
    return result;
  }
  const args = values => ['--root', storage, ...values];
  const fabric = (values, options) => invoke(host, args(values), options);
  assert.deepEqual(JSON.parse(fabric(['plugins', '--registry', registry, 'check', 'acp']).stdout), { compatible: true, healthy: true });
  const doctor = JSON.parse(fabric(['harnesses', 'doctor', 'acp', '--json']).stdout);
  assert.equal(doctor.version, VERSION);
  assert.equal(doctor.manifest.capabilities.find(c => c.capability === 'usage.report').support, 'unsupported');
  assert.equal(doctor.manifest.capabilities.find(c => c.capability === 'model.override').support, 'unsupported');
  // Reuse the existing local commit: no commits, hooks, remotes or user storage are mutated.
  invoke('/usr/bin/git', ['clone', '--quiet', '--local', '--no-hardlinks', source, repo]);
  const original = readFileSync(join(repo, 'README.md'), 'utf8');
  const context = JSON.parse(fabric(['init', '--harness', 'acp', '--check', '--json'], { cwd: repo }).stdout).native_context;
  assert.equal(context.credential_available, false);
  assert.deepEqual(context.credential_paths, []);
  assert.deepEqual(context.profile.model_policy, { kind: 'native-default' });
  const connect = fabric(['harness', 'connect', 'acp', '--yes', '--json'], { success: false });
  assert.match(connect.stderr + connect.stdout, /unavailable or changed/);
  // Explicit reviewed profiles are an existing Core API; do not forge readiness to connect.
  const profile = join(root, 'profile.json');
  writeFileSync(profile, JSON.stringify(context.profile));
  fabric(['project', 'add', '--name', 'fixture', '--path', repo]);
  const recipe = join(root, 'recipe.toml'), promptFile = join(root, 'prompt.txt');
  writeFileSync(recipe, "version=1\n[[steps]]\nargv=['/usr/bin/true']\n");
  assert.deepEqual(lines(log), [], 'normal plugin inspection must not launch an ACP peer');
  const inspection = spawnSync(join(bridgeDir, 'fabric-acp-bridge'), ['--inspect', '--cwd', repo],
    { cwd: root, env: { HOME: root, LC_ALL: 'C' }, encoding: 'utf8', timeout: 15_000 });
  assert.ifError(inspection.error);
  assert.equal(inspection.status, 0, inspection.stdout + inspection.stderr);
  assert.equal(inspection.stderr, '');
  const initialized = JSON.parse(inspection.stdout);
  assert.equal(initialized.stage, 'negotiated');
  assert.equal(initialized.protocolVersion, 1);
  assert.equal(initialized.cleanup, 'reaped');
  assert.equal(initialized.launch.config_sha256, sha256(readFileSync(join(bridgeDir, 'launch.json'))));
  assert.doesNotMatch(inspection.stdout, /SECRET_|UNTRUSTED_/);
  assert.deepEqual(lines(log).filter(record => Object.hasOwn(record, 'method')).map(record => record.method), ['initialize']);
  for (const record of lines(log)) if (record.stopped) reaped(record.stopped);
  fabric(['project', 'prepare', 'fixture', '--recipe', recipe, '--json']);
  const runArgs = ['run', 'fixture', '--harness', 'acp', '--placement', 'local', '--profile', profile,
    '--prompt-file', promptFile, '--copy-limit-bytes', '67108864', '--timeout-seconds', '30', '--json'];
  function evidence(result, expected, state = expected ? 'FAILED' : 'SUCCEEDED') {
    const report = JSON.parse(result.stdout);
    assert.equal(report.state, state, result.stdout + result.stderr);
    if (expected !== undefined) assert.equal(report.capture.native_failure, expected);
    assert.equal(report.cleanup.action, 'reclaimed');
    assert.equal(existsSync(report.workspace_path), false);
    const { artifacts } = JSON.parse(fabric(['runs', 'artifacts', report.run_id, '--json']).stdout);
    const contents = new Map();
    for (const artifact of artifacts) {
      assert.equal(statSync(join(storage, artifact.relative_path)).mode & 0o777, 0o400);
      const bytes = readFileSync(join(storage, artifact.relative_path));
      assert.equal(bytes.length, artifact.size_bytes);
      assert.equal(sha256(bytes), artifact.sha256);
      contents.set(artifact.kind, bytes.toString('utf8'));
    }
    const events = contents.get('events').trim().split('\n').map(line => JSON.parse(line)).filter(record => record.event).map(record => record.event);
    assert.equal(events.some(event => event.type === 'usage'), false);
    for (const event of events.filter(event => event.type === 'tool')) {
      assert.match(event.name, /^acp\.tool\.[1-9][0-9]{0,2}\.(pending|in_progress|completed|failed|unknown)$/);
      assert.ok(['start', 'update', 'end'].includes(event.phase));
    }
    assert.doesNotMatch(result.stdout + result.stderr + contents.get('stdout') + contents.get('stderr') + contents.get('events'), /SECRET_|UNTRUSTED_/);
    const extensions = contents.get('stderr').trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
    assert.ok(extensions.length <= 32);
    assert.ok(Buffer.byteLength(contents.get('stderr')) <= 32_768);
    for (const [index, record] of extensions.entries()) {
      assert.equal(record.type, 'acp.extension');
      assert.equal(record.source, 'untrusted_acp_peer');
      assert.equal(record.phase, 'prompt');
      assert.equal(record.sequence, index + 1);
      assert.match(record.sha256, /^[a-f0-9]{64}$/);
      assert.ok(Buffer.byteLength(JSON.stringify(record) + '\n') <= 4096);
      const bytes = Buffer.from(record.json_base64, 'base64');
      assert.ok(bytes.length <= 2048);
      assert.equal(bytes.length, Math.min(record.notification_bytes, 2048));
      assert.equal(record.truncated, record.notification_bytes > 2048);
      if (!record.truncated) assert.equal(sha256(bytes), record.sha256);
      assert.equal((result.stdout + result.stderr + contents.get('stdout') + contents.get('events')).includes(record.json_base64), false);
    }
    t.diagnostic(JSON.stringify({ host, package_sha256: snapshot.hash, run_id: report.run_id, state: report.state,
      native_failure: report.capture.native_failure, events_sha256: sha256(contents.get('events')) }));
    return { report, events, contents, extensions };
  }
  const prompt = `literal $(touch ${root}/MUST_NOT_EXIST); \"quoted\"\nUnicode: λ 😀`;
  writeFileSync(promptFile, prompt);
  const complete = evidence(fabric(runArgs), null);
  assert.deepEqual(complete.events.filter(event => event.type === 'output'), [{ type: 'output', stream: 'stdout', text: prompt }]);
  assert.match(complete.contents.get('tracked_patch'), /ACP fixture worker/);
  assert.equal(existsSync(join(root, 'MUST_NOT_EXIST')), false);
  assert.equal(readFileSync(join(repo, 'README.md'), 'utf8'), original);
  writeFileSync(promptFile, 'fixture:slow');
  const slow = evidence(fabric(runArgs), null);
  assert.deepEqual(slow.events.filter(event => event.type === 'output'),
    [{ type: 'output', stream: 'stdout', text: 'fixture:slow' }]);
  for (const mode of ['extensions', 'large-extension', 'count-limit']) {
    writeFileSync(promptFile, `fixture:${mode}`);
    const proof = evidence(fabric(runArgs), null);
    assert.deepEqual(proof.events.filter(event => event.type === 'output'),
      [{ type: 'output', stream: 'stdout', text: `fixture:${mode}` }]);
    assert.match(proof.contents.get('tracked_patch'), /ACP fixture worker/);
    if (mode === 'extensions') {
      const notifications = proof.extensions.map(record => JSON.parse(Buffer.from(record.json_base64, 'base64')));
      assert.deepEqual(notifications.map(value => value.method), ['cursor/task', 'cursor/update_todos', 'cursor/generate_image']);
      assert.ok(notifications.every(value => value.params.data === 'PRIVATE_EXTENSION_CANARY' &&
        value.params.sessionId === 'untrusted-other-session'));
      assert.doesNotMatch(proof.contents.get('stdout') + proof.contents.get('events') + JSON.stringify(proof.report), /PRIVATE_EXTENSION_CANARY/);
    } else if (mode === 'large-extension') {
      const bytes = Buffer.from(JSON.stringify({ jsonrpc: '2.0', method: `vendor/${'x'.repeat(3000)}`,
        params: { sessionId: 'untrusted-other-session', data: '😀'.repeat(2000), marker: mode } }));
      assert.equal(proof.extensions.length, 1);
      assert.equal(proof.extensions[0].sha256, sha256(bytes));
      assert.equal(proof.extensions[0].json_base64, bytes.subarray(0, 2048).toString('base64'));
      assert.equal(proof.extensions[0].truncated, true);
    } else assert.equal(proof.extensions.length, 32);
  }
  const toolEvent = (call, status, phase) => ({ type: 'tool', name: `acp.tool.${call}.${status}`, phase });
  writeFileSync(promptFile, 'fixture:tool');
  const toolProof = evidence(fabric(runArgs), null);
  assert.deepEqual(toolProof.events.filter(event => ['tool', 'output'].includes(event.type)), [
    toolEvent(1, 'pending', 'start'), toolEvent(1, 'pending', 'update'), toolEvent(1, 'pending', 'update'),
    toolEvent(1, 'in_progress', 'update'), { type: 'output', stream: 'stdout', text: 'fixture:tool' },
    toolEvent(1, 'completed', 'end'), toolEvent(1, 'completed', 'update'), toolEvent(1, 'in_progress', 'update'),
    toolEvent(1, 'failed', 'end'), toolEvent(2, 'unknown', 'update'), toolEvent(2, 'completed', 'end'),
    toolEvent(3, 'failed', 'end'), toolEvent(4, 'completed', 'end'), toolEvent(5, 'in_progress', 'start'),
  ]);
  assert.equal(toolProof.contents.get('tracked_patch'), '');
  assert.equal(toolProof.contents.get('stderr'), '');
  assert.equal(existsSync(`${log}.MUST_NOT_EXIST`), false);
  const toolPeer = lines(log).find(record => record.mode === 'tool');
  assert.equal(existsSync(join(toolPeer.cwd, 'MUST_NOT_EXIST')), false);
  writeFileSync(promptFile, 'fixture:tool-limit');
  const limit = evidence(fabric(runArgs), null);
  assert.deepEqual(limit.events.filter(event => event.type === 'tool'),
    Array.from({ length: 256 }, (_, index) => toolEvent(index + 1, 'pending', 'start')));
  const initial = { sessionUpdate: 'tool_call', toolCallId: 'SECRET_ID_CANARY', title: 'SECRET_TITLE_CANARY' };
  const delta = { sessionUpdate: 'tool_call_update', toolCallId: initial.toolCallId };
  const malformedTools = {
    'missing-id': { sessionUpdate: 'tool_call', title: '' },
    'numeric-id': { ...initial, toolCallId: 1 },
    'missing-title': { sessionUpdate: 'tool_call', toolCallId: '' },
    'null-title': { ...initial, title: null },
    'null-status': { ...initial, status: null },
    'null-kind': { ...initial, kind: null },
    'null-content': { ...initial, content: null },
    'null-locations': { ...initial, locations: null },
    'bad-name': { ...delta, name: {} },
    'bad-title': { ...delta, title: 1 },
    'bad-status': { ...delta, status: 'SECRET_STATUS_CANARY' },
    'bad-kind': { ...delta, kind: 'SECRET_KIND_CANARY' },
    'bad-meta': { ...delta, _meta: [] },
    'extra-field': { ...delta, unexpected: true },
    'missing-delta-id': { sessionUpdate: 'tool_call_update' },
    'bad-content': { ...delta, content: {} },
    'bad-text': { ...delta, content: [{ type: 'content', content: { type: 'text', text: 3 } }] },
    'bad-annotation': { ...delta, content: [{ type: 'content', content: { type: 'text', text: '', annotations: [] } }] },
    'bad-diff': { ...delta, content: [{ type: 'diff', path: '/SECRET_PATH_CANARY' }] },
    'bad-terminal': { ...delta, content: [{ type: 'terminal', terminalId: 1 }] },
    'bad-image': { ...delta, content: [{ type: 'content', content: { type: 'image', data: '' } }] },
    'bad-resource': { ...delta, content: [{ type: 'content', content: { type: 'resource', resource: { uri: '', text: 1 } } }] },
    'bad-link': { ...delta, content: [{ type: 'content', content: { type: 'resource_link', uri: '' } }] },
    'unsupported-session': { ...delta, content: [{ type: 'session', sessionId: 'SECRET_SESSION_CANARY' }] },
    'bad-locations': { ...delta, locations: {} },
    'bad-path': { ...delta, locations: [{ path: 1 }] },
    'bad-line': { ...delta, locations: [{ path: '', line: -1 }] },
    'large-line': { ...delta, locations: [{ path: '', line: 4294967296 }] },
    'fractional-line': { ...delta, locations: [{ path: '', line: 1.5 }] },
  };
  const validationCwd = join(storage, 'tool-validation');
  mkdirSync(validationCwd);
  for (const [name, update] of Object.entries(malformedTools)) {
    const mode = `tool-invalid-${name}`;
    const result = spawnSync(join(bridgeDir, 'fabric-acp-bridge'), ['--turn', `fixture:${mode}\n${JSON.stringify(update)}`],
      { cwd: validationCwd, env: { HOME: root, LC_ALL: 'C' }, encoding: 'utf8', timeout: 15_000 });
    assert.ifError(result.error);
    assert.equal(result.status, 1, mode);
    assert.deepEqual(JSON.parse(result.stdout), { type: 'acp.failure', code: 'acp_invalid_update' }, mode);
    assert.equal(result.stderr, '', mode);
    reaped(lines(log).find(record => record.mode === mode).pid);
  }
  for (const mode of ['tool-early-initialize', 'tool-early-session']) {
    const early = join(root, mode);
    prepare({ ...config, args: [agent, log, mode] }, early);
    const result = spawnSync(join(early, 'fabric-acp-bridge'), ['--turn', 'unused'],
      { cwd: repo, env: { HOME: root, LC_ALL: 'C' }, encoding: 'utf8', timeout: 15_000 });
    assert.ifError(result.error);
    assert.notEqual(result.status, 0);
    assert.deepEqual(JSON.parse(result.stdout), { type: 'acp.failure', code: 'acp_invalid_update' });
    assert.equal(result.stderr, '');
    reaped(lines(log).find(record => record.early === mode).earlyPid);
  }
  const failures = { malformed: 'acp_invalid_update', mismatch: 'acp_invalid_update',
    image: 'acp_invalid_update', request: 'acp_unsupported_request', failure: 'acp_remote_error',
    stop: 'acp_invalid_stop', disconnect: 'acp_disconnected', late: 'acp_invalid_update',
    'vendor-request': 'acp_unsupported_request', 'extension-request': 'acp_unsupported_request',
    'core-session': 'acp_unsupported_update', 'core-fs': 'acp_unsupported_update',
    'core-terminal': 'acp_unsupported_update', 'core-method': 'acp_unsupported_update',
    'core-rpc': 'acp_unsupported_update',
    'tool-mismatch': 'acp_invalid_update', 'tool-late': 'acp_invalid_update', 'tool-overflow': 'acp_tool_overflow',
    'tool-request': 'acp_unsupported_request', 'tool-fs-request': 'acp_unsupported_request',
    'tool-terminal-request': 'acp_unsupported_request',
    'count-flood': 'acp_extension_overflow', 'byte-flood': 'acp_extension_overflow' };
  for (const [mode, code] of Object.entries(failures)) {
    writeFileSync(promptFile, `fixture:${mode}`);
    const proof = evidence(fabric(runArgs, { success: false }), code);
    assert.deepEqual(proof.events.filter(event => event.type === 'failure'), [{ type: 'failure', code,
      message: code === 'acp_unsupported_request' ? 'Unsupported blocking ACP peer request; no input or approval supplied.' : code }]);
    if (code === 'acp_unsupported_request') {
      assert.deepEqual(lines(log).filter(record => record.response).at(-1)?.response, {
        jsonrpc: '2.0', id: mode === 'extension-request' ? 0 : 'peer-request',
        error: { code: -32601, message: 'Method not found' },
      });
    }
    if (mode === 'count-flood') assert.equal(proof.extensions.length, 32);
    else if (mode === 'tool-overflow') assert.equal(proof.events.filter(event => event.type === 'tool').length, 256);
    else if (mode === 'byte-flood') {
      assert.ok(proof.extensions.length > 0 && proof.extensions.length < 32);
      const next = { ...proof.extensions.at(-1), sequence: proof.extensions.length + 1 };
      assert.ok(Buffer.byteLength(proof.contents.get('stderr')) + Buffer.byteLength(JSON.stringify(next) + '\n') > 32_768);
    } else assert.deepEqual(proof.extensions, []);
  }
  const cancelledPrompt = `fixture:cancel\ncancel-only $(touch ${root}/MUST_NOT_EXIST); \"λ\"`;
  const survivorPrompt = `fixture:concurrent\nsurvivor-only $(touch ${root}/MUST_NOT_EXIST); \"😀\"`;
  function start(prompt, name) {
    const path = join(root, `${name}.txt`);
    writeFileSync(path, prompt);
    const values = runArgs.map(value => value === promptFile ? path : value);
    const child = spawn(host, args(values), { cwd: root, env, stdio: ['ignore', 'pipe', 'pipe'] });
    children.add(child);
    const run = { child, stdout: '', stderr: '' };
    child.stdout.on('data', bytes => { run.stdout += bytes; });
    child.stderr.on('data', bytes => { run.stderr += bytes; });
    run.closed = new Promise((resolve, reject) => {
      child.on('error', reject);
      child.on('close', status => { children.delete(child); resolve(status); });
    });
    return run;
  }
  const cancelling = start(cancelledPrompt, 'cancel'), surviving = start(survivorPrompt, 'survivor');
  const until = Date.now() + 20_000;
  while (!['cancel', 'concurrent'].every(mode => lines(log).some(record => record.extensionsSent === mode))) {
    assert.ok(Date.now() < until && cancelling.child.exitCode === null && surviving.child.exitCode === null,
      'Both installed-plugin fixture turns must reach pending prompts concurrently');
    await delay(20);
  }
  const [cancelPeer, survivorPeer] = ['cancel', 'concurrent'].map(mode => lines(log).find(record => record.mode === mode));
  assert.notEqual(cancelPeer.pid, survivorPeer.pid);
  assert.notEqual(cancelPeer.parent, survivorPeer.parent);
  assert.notEqual(cancelPeer.cwd, survivorPeer.cwd);
  assert.equal(cancelPeer.prompt, cancelledPrompt); assert.equal(survivorPeer.prompt, survivorPrompt);
  assert.ok(existsSync(cancelPeer.cwd) && existsSync(survivorPeer.cwd));
  process.kill(cancelPeer.pid, 0); process.kill(survivorPeer.pid, 0);
  // Peer writes are not proof of host delivery. Cancel only after both captures
  // contain the ordered tool deltas, not after an arbitrary delay.
  function delivered(peer) {
    const records = readFileSync(join(dirname(peer.cwd), 'events.jsonl'), 'utf8').split('\n').slice(0, -1).map(line => JSON.parse(line));
    return records.filter(record => record.event?.type === 'tool').length === 2 &&
      readFileSync(join(dirname(peer.cwd), 'stderr.log'), 'utf8').endsWith('\n');
  }
  while (![cancelPeer, survivorPeer].every(delivered)) {
    assert.ok(Date.now() < until && cancelling.child.exitCode === null && surviving.child.exitCode === null,
      'Both active turns must capture tool notifications before cancellation');
    await delay(20);
  }
  cancelling.child.kill('SIGINT');
  assert.notEqual(await cancelling.closed, 0);
  const cancelled = JSON.parse(cancelling.stdout);
  assert.equal(cancelled.state, 'CANCELLED', cancelling.stdout + cancelling.stderr);
  assert.equal(cancelled.cleanup.action, 'reclaimed');
  assert.equal(cancelled.workspace_path, cancelPeer.cwd);
  assert.equal(existsSync(cancelPeer.cwd), false);
  reaped(cancelPeer.pid);
  const cancelledProof = evidence(cancelling, undefined, 'CANCELLED');
  assert.deepEqual(cancelledProof.events.filter(event => event.type === 'tool'),
    [toolEvent(1, 'pending', 'start'), toolEvent(1, 'in_progress', 'update')]);
  assert.equal(cancelledProof.extensions.length, 1);
  assert.equal(JSON.parse(Buffer.from(cancelledProof.extensions[0].json_base64, 'base64')).params.marker, 'cancel');
  assert.equal(surviving.child.exitCode, null, 'cancelling one host run stopped the other');
  process.kill(survivorPeer.pid, 0);
  assert.ok(existsSync(survivorPeer.cwd));
  writeFileSync(`${log}.release`, 'release the independently owned surviving turn');
  assert.equal(await surviving.closed, 0, surviving.stdout + surviving.stderr);
  const survivor = evidence(surviving, null);
  assert.deepEqual(survivor.events.filter(event => event.type === 'tool'),
    [toolEvent(1, 'pending', 'start'), toolEvent(1, 'in_progress', 'update'), toolEvent(1, 'completed', 'end')]);
  assert.notEqual(survivor.report.run_id, cancelled.run_id);
  assert.equal(survivor.report.workspace_path, survivorPeer.cwd);
  assert.deepEqual(survivor.events.filter(event => event.type === 'output'),
    [{ type: 'output', stream: 'stdout', text: survivorPrompt }]);
  assert.ok(survivor.contents.get('tracked_patch').includes('survivor-only'));
  assert.equal(survivor.contents.get('tracked_patch').includes('cancel-only'), false);
  assert.equal(survivor.extensions.length, 1);
  assert.equal(JSON.parse(Buffer.from(survivor.extensions[0].json_base64, 'base64')).params.marker, 'concurrent');
  assert.notEqual(survivor.extensions[0].json_base64, cancelledProof.extensions[0].json_base64);
  assert.equal(survivor.contents.get('stderr').includes(cancelledProof.extensions[0].json_base64), false);
  assert.equal(cancelledProof.contents.get('stderr').includes(survivor.extensions[0].json_base64), false);
  assert.equal(existsSync(join(root, 'MUST_NOT_EXIST')), false);
  assert.equal(readFileSync(join(repo, 'README.md'), 'utf8'), original);
  reaped(survivorPeer.pid);
  t.diagnostic(JSON.stringify({ cancelled_run: cancelled.run_id, survivor_run: survivor.report.run_id,
    cancellation: 'SIGINT with both independent installed-plugin turns active', cleanup: 'both reclaimed' }));
  for (const record of lines(log).filter(record => record.pid)) {
    assert.equal(record.group, record.parentGroup, 'ACP child escaped bridge process group');
    assert.equal(record.home, home);
    // Darwin adds this key even to `env -i ... node`; it is not inherited consent.
    const environment = record.environment.filter(key => process.platform !== 'darwin' || key !== '__CF_USER_TEXT_ENCODING');
    assert.deepEqual(environment, ['HOME', 'LC_ALL']);
    assert.notEqual(record.cwd, repo);
    assert.ok(record.cwd.startsWith(storage + '/'));
    reaped(record.pid);
    assert.equal(lines(log).filter(value => value.mode === record.mode).length, 1, 'turn was replayed');
  }
  // Revalidates content identity before another launch, without ever running a replacement.
  const count = lines(log).filter(record => record.pid).length;
  writeFileSync(agent, 'throw new Error(\"must never execute replacement\");');
  const changed = fabric(['harnesses', 'doctor', 'acp', '--json'], { success: false });
  assert.doesNotMatch(changed.stdout + changed.stderr, /must never execute replacement/);
  assert.equal(lines(log).filter(record => record.pid).length, count);
});
