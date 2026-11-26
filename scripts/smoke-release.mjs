#!/usr/bin/env node
// Provider-free install proof. Does not start a manager or read the caller's credentials.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, isAbsolute, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const args = process.argv.slice(2);
const local = args[0] === '--local';
const fromArchive = local || args[0] === '--archive';
if (fromArchive) args.shift();
assert.equal(args.length, 4, 'usage: node smoke-release.mjs [--local|--archive] TAG_OR_ARCHIVE EXPECTED_SHA256 FULL_SOURCE_SHA NEW_INSTALL_DIRECTORY');
const [input, expectedHash, source, destinationArg] = args;
assert.match(expectedHash, /^[a-f0-9]{64}$/);
assert.match(source, /^[a-f0-9]{40}$/);
assert.equal(process.platform, 'darwin');
assert.equal(process.arch, 'arm64');
const destination = resolve(destinationArg);
assert.ok(isAbsolute(destinationArg) && !existsSync(destination), 'never overwrite an installation');
assert.equal(realpathSync(dirname(destination)), dirname(destination), 'installation parent must be canonical');
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const fixture = mkdtempSync(join(realpathSync(tmpdir()), 'fabric-release-smoke-'));
const env = {
  PATH: `${fixture}/tools:/usr/bin:/bin`, HOME: join(fixture, 'home'), TMPDIR: fixture,
  XDG_DATA_HOME: join(fixture, 'data'), XDG_CONFIG_HOME: join(fixture, 'config'),
  PI_CODING_AGENT_DIR: join(fixture, 'omp-agent'), CODEX_HOME: join(fixture, 'codex'),
  FABRIC_PLUGIN_REGISTRY: join(fixture, 'registry'),
  GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null',
  GIT_AUTHOR_NAME: 'Release smoke', GIT_AUTHOR_EMAIL: 'smoke@example.invalid',
  GIT_COMMITTER_NAME: 'Release smoke', GIT_COMMITTER_EMAIL: 'smoke@example.invalid',
};
function run(command, argv, cwd = fixture, environment = env) {
  const result = spawnSync(command, argv, { cwd, env: environment, encoding: 'utf8', timeout: 180_000, maxBuffer: 8 * 1024 * 1024 });
  assert.ifError(result.error);
  assert.equal(result.status, 0, `${command} ${argv.join(' ')}\n${result.stderr}`);
  return result.stdout.trim();
}
function download(url, path) {
  run('/usr/bin/curl', ['--fail', '--location', '--silent', '--show-error', '--proto', '=https', '--tlsv1.2', '--max-time', '180', url, '--output', path]);
}
function removePrivate(path) {
  // Prepared snapshots are read-only; only relax permissions inside this newly created fixture.
  const stat = lstatSync(path);
  if (stat.isSymbolicLink()) return;
  chmodSync(path, stat.isDirectory() ? 0o700 : 0o600);
  if (stat.isDirectory()) for (const child of readdirSync(path)) removePrivate(join(path, child));
}
try {
  for (const key of ['HOME', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'PI_CODING_AGENT_DIR', 'CODEX_HOME', 'FABRIC_PLUGIN_REGISTRY']) mkdirSync(env[key], { mode: 0o700 });
  mkdirSync(join(fixture, 'tools'), { mode: 0o700 });
  run('/bin/sh', ['-c', '! command -v cargo && ! command -v rustc']);
  let archive;
  if (fromArchive) archive = resolve(input);
  else {
    assert.match(input, /^v\d+\.\d+\.\d+$/);
    const name = `fabric-${input}-macos-arm64.tar.gz`;
    const url = `https://github.com/satansdeer/fabric/releases/download/${input}/${name}`;
    archive = join(fixture, name);
    download(url, archive);
    download(`${url}.sha256`, `${archive}.sha256`);
    assert.equal(readFileSync(`${archive}.sha256`, 'utf8'), `${expectedHash}  ${name}\n`, 'release checksum must match the independently recorded pin');
  }
  assert.equal(hash(readFileSync(archive)), expectedHash, 'archive checksum mismatch; do not extract');
  const names = run('/usr/bin/tar', ['-tzf', archive]).split('\n');
  const root = names[0].replace(/\/$/, '');
  assert.match(root, /^fabric-v\d+\.\d+\.\d+(?:-local\.[a-f0-9]{12})?-macos-arm64$/);
  for (const name of names) assert.ok(name.startsWith(`${root}/`) && name.split('/').every(part => part !== '..' && part !== '.'), 'unsafe archive path');
  mkdirSync(destination, { mode: 0o700 });
  run('/usr/bin/tar', ['-xzf', archive, '-C', destination, '--no-same-owner']);
  const installed = join(destination, root);
  const manifest = JSON.parse(readFileSync(join(installed, 'release.json'), 'utf8'));
  assert.equal(manifest.source_sha, source);
  assert.equal(manifest.build_kind, local ? 'local-unreviewed' : 'release');
  assert.equal(manifest.platform, 'macos-arm64');
  assert.equal(manifest.archive, basename(archive));
  if (!fromArchive) assert.equal(manifest.tag, input);
  for (const [name, pin] of Object.entries(manifest.files)) {
    assert.ok(!isAbsolute(name) && name.split('/').every(part => part !== '..' && part !== '.' && part !== ''));
    const path = join(installed, name);
    assert.ok(lstatSync(path).isFile() && realpathSync(path) === path, 'payload must contain regular, non-symlink files');
    assert.equal(hash(readFileSync(path)), pin, name);
  }
  const { directory } = await import(pathToFileURL(join(installed, 'sdk/package.mjs')));
  assert.deepEqual(manifest.plugins.map(p => p.id), ['omp', 'codex']);
  assert.deepEqual(manifest.target_plugins.map(p => p.id), ['local']);
  for (const plugin of [...manifest.plugins, ...manifest.target_plugins]) {
    const pkg = directory(join(installed, 'plugins', plugin.id));
    assert.equal(pkg.hash, plugin.sha256);
    assert.equal(pkg.info.version, plugin.version);
  }
  // Required on every candidate: absence is a failure, never a skipped ACP proof.
  const kit = manifest.acp_profile_kit;
  assert.equal(kit.id, 'fabric-acp');
  assert.equal(kit.version, '0.1.0');
  assert.equal(kit.protocol, 1);
  assert.equal(kit.path, 'plugins/acp');
  assert.equal(kit.sdk_path, 'sdk');
  assert.equal(kit.purpose, 'offline-profile-authoring-only');
  assert.equal(kit.installable, false);
  assert.equal(kit.fixture.path, 'plugins/acp/fixtures/turn-agent.mjs');
  for (const file of [
    ...['package.mjs', 'bridge.mjs', 'inspect.mjs', 'config.mjs', 'transport.mjs', 'permission.mjs', 'permission-client.mjs', 'README.md', 'fixtures/turn-agent.mjs'].map(file => `${kit.path}/${file}`),
    ...['cli.mjs', 'package.mjs', 'protocol.mjs', 'index.mjs', 'webhook.mjs'].map(file => `${kit.sdk_path}/${file}`),
    'scripts/scripted-provider.mjs', 'docs/LIVE_SMOKES.md',
    ...['claude', 'cline'].flatMap(native => ['fixture-launcher.mjs', 'scripted-provider-smoke.mjs'].map(file => `integrations/${native}/${file}`)),
  ]) assert.match(manifest.files[file], /^[a-f0-9]{64}$/, `required pinned ACP kit/SDK payload: ${file}`);
  assert.equal(existsSync(join(installed, kit.path, 'launch.json')), false, 'kit must not bind a native profile');
  assert.equal(existsSync(join(installed, kit.path, 'package.json')), false, 'kit is not an installable plugin');
  const binary = join(installed, 'bin/fabric');
  assert.equal(run(binary, ['--version']), manifest.cli_version);
  assert.equal(JSON.parse(run(binary, ['doctor', '--json'])).platform.supported, true);
  assert.equal(hash(readFileSync(join(installed, manifest.fixture.path))), manifest.fixture.sha256);
  assert.equal(hash(readFileSync(join(installed, manifest.omp_extension.path))), manifest.omp_extension.sha256);
  // Only the reviewed official OMP binary is executed; no auth, model request or native-context consent.
  assert.equal(manifest.omp.sha256, 'dca5643669ed5e553f707bd065499babd0fe3030696bbb24c9da09c798af5abb');
  assert.equal(manifest.omp.url, 'https://github.com/can1357/oh-my-pi/releases/download/v19.1.0/omp-darwin-arm64');
  const omp = join(fixture, 'tools/omp');
  download(manifest.omp.url, omp);
  assert.equal(hash(readFileSync(omp)), manifest.omp.sha256);
  chmodSync(omp, 0o700);
  assert.equal(run(omp, ['--version']), manifest.omp.version);
  for (const plugin of [...manifest.plugins, ...manifest.target_plugins]) run(process.execPath, [join(installed, 'sdk/cli.mjs'), 'install', join(installed, 'plugins', plugin.id), '--registry', env.FABRIC_PLUGIN_REGISTRY, '--fabric', binary, '--version', plugin.version, '--sha256', plugin.sha256, '--trust']);
  const generated = join(fixture, 'generated-harness');
  run(process.execPath, [join(installed, 'sdk/cli.mjs'), 'create', 'harness', generated, '--id', 'release-sdk-proof']);
  const sdkProof = JSON.parse(run(process.execPath, [join(installed, 'sdk/cli.mjs'), 'test', generated]));
  assert.equal(sdkProof.id, 'release-sdk-proof');
  assert.equal(sdkProof.conformance, 'passed');
  assert.deepEqual(JSON.parse(run(binary, ['plugins', '--registry', env.FABRIC_PLUGIN_REGISTRY, 'check', 'local'])), { compatible: true, healthy: true });
  assert.deepEqual(JSON.parse(run(binary, ['plugins', '--registry', env.FABRIC_PLUGIN_REGISTRY, 'check', 'omp'])), { compatible: true, healthy: true });
  const native = JSON.parse(run(binary, ['harnesses', '--json', 'doctor', 'omp']));
  assert.equal(native.harness_id, 'omp');
  const repo = join(fixture, 'project');
  mkdirSync(repo, { mode: 0o700 });
  for (const argv of [['init', '--quiet'], ['config', 'core.hooksPath', '/dev/null']]) run('/usr/bin/git', argv, repo);
  writeFileSync(join(repo, 'tracked'), 'release fixture\n');
  writeFileSync(join(repo, 'README.md'), 'release source fixture\n');
  run('/usr/bin/git', ['add', 'tracked', 'README.md'], repo);
  run('/usr/bin/git', ['-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'fixture'], repo);
  const projectSource = run('/usr/bin/git', ['rev-parse', 'HEAD'], repo);
  const cli = argv => JSON.parse(run(binary, ['--root', join(fixture, 'fabric'), 'project', ...argv, '--json'], repo));
  cli(['add', '--name', 'release-smoke', '--path', repo]);
  const status = cli(['status', 'release-smoke']);
  assert.equal(status.source_commit_sha, projectSource);
  const recipe = join(fixture, 'prepare.toml');
  writeFileSync(recipe, "version=1\n[[steps]]\nargv=['/usr/bin/true']\n");
  const prepared = cli(['prepare', 'release-smoke', '--recipe', recipe]);
  assert.ok(prepared.state_id, 'preparation must produce an immutable ready state');
  const prompt = join(fixture, 'prompt.json');
  writeFileSync(prompt, JSON.stringify({ writes: [{ path: 'target-proof', content: 'installed local target\\n' }] }));
  const worker = JSON.parse(run(binary, ['--root', join(fixture, 'fabric'), 'run', 'release-smoke', '--harness', 'fake', '--fake-binary', join(installed, manifest.fixture.path), '--prompt-file', prompt, '--copy-limit-bytes', '10485760', '--json'], repo));
  assert.equal(worker.state, 'SUCCEEDED');
  assert.equal(worker.cleanup.action, 'reclaimed');
  // Offline extracted-kit proof. The only ACP executable is Node running the
  // shipped, explicitly fake peer copied into this owned temporary profile.
  run('/bin/sh', ['-c', '! command -v cargo && ! command -v rustc && ! command -v bun && ! command -v npm']);
  const acp = join(fixture, 'acp');
  const acpHome = join(acp, 'native-home'), acpRegistry = join(acp, 'registry');
  mkdirSync(acp, { mode: 0o700 });
  mkdirSync(acpHome, { mode: 0o700 });
  mkdirSync(acpRegistry, { mode: 0o700 });
  const peer = join(acp, 'deterministic-fake-acp-peer.mjs'), log = join(acp, 'peer.jsonl');
  writeFileSync(peer, readFileSync(join(installed, kit.fixture.path)), { mode: 0o600, flag: 'wx' });
  const node = realpathSync(process.execPath);
  const launchInput = join(acp, 'launch-input.json'), archivePath = join(acp, 'profile.tgz');
  writeFileSync(launchInput, JSON.stringify({
    executable: { path: node, sha256: hash(readFileSync(node)) }, args: [peer, log],
    nativeHome: acpHome, cwd: 'fabric-workspace', pins: [{ path: peer, sha256: hash(readFileSync(peer)) }],
    requestTimeoutMs: 5000,
  }), { mode: 0o600, flag: 'wx' });
  const packaged = JSON.parse(run(node, [join(installed, kit.path, 'package.mjs'), launchInput, join(acp, 'package'), archivePath]));
  assert.equal(directory(packaged.package).hash, packaged.sha256);
  const sdk = join(installed, kit.sdk_path, 'cli.mjs');
  const registered = JSON.parse(run(node, [sdk, 'install', archivePath, '--registry', acpRegistry,
    '--fabric', binary, '--version', kit.version, '--sha256', packaged.sha256, '--trust']));
  assert.equal(registered.sha256, packaged.sha256);
  const bridgeDir = join(acpRegistry, 'packages', packaged.sha256);
  const bridge = join(bridgeDir, 'fabric-acp-bridge');
  // The local target is the existing official package, not another SDK/runtime.
  const localTarget = manifest.target_plugins.find(plugin => plugin.id === 'local');
  run(node, [sdk, 'install', join(installed, 'plugins/local'), '--registry', acpRegistry, '--fabric', binary,
    '--version', localTarget.version, '--sha256', localTarget.sha256, '--trust']);
  const acpEnv = { ...env, PATH: `${bridgeDir}:/usr/bin:/bin`, FABRIC_PLUGIN_REGISTRY: acpRegistry, FABRIC_ACP_HOME: acpHome };
  const host = argv => JSON.parse(run(binary, ['--root', join(acp, 'storage'), ...argv, '--json'], repo, acpEnv));
  const context = host(['init', '--harness', 'acp', '--check']).native_context;
  assert.equal(context.credential_available, false);
  assert.deepEqual(context.credential_paths, []);
  assert.deepEqual(context.profile.model_policy, { kind: 'native-default' });
  assert.equal(existsSync(log), false, 'offline packaging/install/context must not launch the peer');
  const inspection = JSON.parse(run(bridge, ['--inspect', '--cwd', repo], acp, { HOME: acpHome, LC_ALL: 'C' }));
  assert.equal(inspection.stage, 'negotiated');
  assert.equal(inspection.code, 'acp_initialized');
  assert.equal(inspection.protocolVersion, 1);
  assert.equal(inspection.cleanup, 'reaped');
  assert.equal(inspection.auth, 'unverified');
  assert.equal(inspection.model, 'unverified');
  assert.equal(inspection.peer_identity, 'untrusted');
  assert.equal(inspection.launch.config_sha256, hash(readFileSync(join(bridgeDir, 'launch.json'))));
  const records = () => readFileSync(log, 'utf8').trim().split('\n').map(line => JSON.parse(line));
  assert.deepEqual(records().filter(record => record.method).map(record => record.method), ['initialize']);
  const profile = join(acp, 'profile.json'), acpPrompt = join(acp, 'prompt.txt');
  const text = 'deterministic fake ACP release proof';
  writeFileSync(profile, JSON.stringify(context.profile), { mode: 0o600, flag: 'wx' });
  writeFileSync(acpPrompt, text, { mode: 0o600, flag: 'wx' });
  host(['project', 'add', '--name', 'acp-release-smoke', '--path', repo]);
  host(['project', 'prepare', 'acp-release-smoke', '--recipe', recipe]);
  const turn = host(['run', 'acp-release-smoke', '--harness', 'acp', '--placement', 'local',
    '--profile', profile, '--prompt-file', acpPrompt, '--copy-limit-bytes', '10485760', '--timeout-seconds', '30']);
  assert.equal(turn.state, 'SUCCEEDED');
  assert.equal(turn.capture.native_failure, null);
  assert.equal(turn.cleanup.action, 'reclaimed');
  assert.equal(existsSync(turn.workspace_path), false);
  assert.equal(readFileSync(join(repo, 'README.md'), 'utf8'), 'release source fixture\n', 'source checkout must remain unchanged');
  const { artifacts } = host(['runs', 'artifacts', turn.run_id]);
  const contents = new Map();
  for (const artifact of artifacts) {
    const bytes = readFileSync(join(acp, 'storage', artifact.relative_path));
    assert.equal(bytes.length, artifact.size_bytes);
    assert.equal(hash(bytes), artifact.sha256);
    contents.set(artifact.kind, bytes.toString('utf8'));
  }
  assert.match(contents.get('tracked_patch'), /-release source fixture\n\+ACP fixture worker\n/);
  assert.match(contents.get('tracked_patch'), /^diff --git a\/README\.md b\/README\.md\n/);
  const events = contents.get('events').trim().split('\n').map(line => JSON.parse(line)).filter(record => record.event).map(record => record.event);
  assert.deepEqual(events.filter(event => event.type === 'output'), [{ type: 'output', stream: 'stdout', text }]);
  assert.equal(events.some(event => ['failure', 'tool', 'usage'].includes(event.type)), false);
  assert.deepEqual(records().filter(record => record.method).map(record => record.method),
    ['initialize', 'initialize', 'session/new', 'session/prompt']);
  const child = records().find(record => record.pid);
  assert.equal(child.cwd, turn.workspace_path);
  assert.equal(child.home, acpHome);
  assert.equal(child.prompt, text);
  assert.deepEqual(child.environment.filter(key => key !== '__CF_USER_TEXT_ENCODING'), ['HOME', 'LC_ALL']);
  for (const pid of new Set(records().flatMap(record => [record.pid, record.stopped].filter(Boolean)))) {
    assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' }, 'owned fake ACP child must be reaped');
  }
  const acpProof = { kind: 'DETERMINISTIC_FAKE_ACP_ONLY', kit, package_sha256: packaged.sha256,
    inspection, run_id: turn.run_id, state: turn.state, cleanup: turn.cleanup.action, artifacts };
  console.log(JSON.stringify({ proof: fromArchive ? (local ? 'LOCAL_PACKAGE_ONLY' : 'RELEASE_CANDIDATE_LOCAL_PACKAGE') : 'DOWNLOADED_RELEASE', source_sha: source, archive: manifest.archive, sha256: expectedHash, installed, cli_version: manifest.cli_version, executable_sha256: manifest.files['bin/fabric'], plugins: manifest.plugins, target_plugins: manifest.target_plugins, acp_profile_kit: acpProof, local_target_worker: { run_id: worker.run_id, state: worker.state, cleanup: worker.cleanup.action }, omp_extension: manifest.omp_extension, fixture: manifest.fixture, native_discovery: native, project_source_sha: projectSource, prepared_state: prepared.state_id, runtime_path: env.PATH, cargo: 'absent', rustc: 'absent', providers: 'not invoked', manager: 'not started', codex_discovery: 'not tested', host_swarm: 'not tested' }, null, 2));
} finally {
  removePrivate(fixture);
  rmSync(fixture, { recursive: true, force: true });
}
