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
function run(command, argv, cwd = fixture) {
  const result = spawnSync(command, argv, { cwd, env, encoding: 'utf8', timeout: 180_000, maxBuffer: 8 * 1024 * 1024 });
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
  run('/usr/bin/git', ['add', 'tracked'], repo);
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
  console.log(JSON.stringify({ proof: fromArchive ? (local ? 'LOCAL_PACKAGE_ONLY' : 'RELEASE_CANDIDATE_LOCAL_PACKAGE') : 'DOWNLOADED_RELEASE', source_sha: source, archive: manifest.archive, sha256: expectedHash, installed, cli_version: manifest.cli_version, executable_sha256: manifest.files['bin/fabric'], plugins: manifest.plugins, target_plugins: manifest.target_plugins, local_target_worker: { run_id: worker.run_id, state: worker.state, cleanup: worker.cleanup.action }, omp_extension: manifest.omp_extension, fixture: manifest.fixture, native_discovery: native, project_source_sha: projectSource, prepared_state: prepared.state_id, runtime_path: env.PATH, cargo: 'absent', rustc: 'absent', providers: 'not invoked', manager: 'not started', codex_discovery: 'not tested', host_swarm: 'not tested' }, null, 2));
} finally {
  removePrivate(fixture);
  rmSync(fixture, { recursive: true, force: true });
}
