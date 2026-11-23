#!/usr/bin/env node
// Narrow, provider-free consumer proof against the committed public release pins.
// With no --archive, download that existing release once (not the workflow's new candidate).
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const distribution = join(root, 'distribution');
const launcher = join(distribution, 'launcher.mjs');
const manifest = JSON.parse(readFileSync(join(distribution, 'fabric-v0.1.2-macos-arm64.json'), 'utf8'));
const checksum = readFileSync(join(distribution, `${manifest.archive}.sha256`), 'utf8');
const expectedHash = '81f06df7a50f0e513fb64e409286f06835b0cf29f00eb991c1bfb42f671ec3d6';
assert.equal(checksum, `${expectedHash}  ${manifest.archive}\n`);
assert.equal(manifest.source_sha, '14f2d44b40f8489d38537e31eda41dd8752463df');
const pkg = JSON.parse(readFileSync(join(distribution, 'package.json'), 'utf8'));
assert.equal(pkg.private, true);
assert.equal(pkg.name, '@fabric-distribution-unpublished/cli');
assert.equal(pkg.bin['fabric-native'], 'launcher.mjs');
assert.equal(pkg.engines.node, '>=22.18.0');
for (const key of ['scripts', 'dependencies', 'optionalDependencies', 'devDependencies']) assert.equal(pkg[key], undefined, `no ${key}`);
for (const name of ['launcher.mjs', 'install.rb', 'fabric.rb', 'fabric-v0.1.2-macos-arm64.json', 'fabric-v0.1.2-macos-arm64.tar.gz.sha256']) {
  assert.ok(pkg.files.includes(name), `${name} must ship in the optional package`);
  assert.ok(lstatSync(join(distribution, name)).isFile());
}
assert.equal(process.platform, 'darwin', 'released consumer proof requires native macOS arm64; no environment platform override is accepted');
assert.equal(process.arch, 'arm64');
const args = process.argv.slice(2);
assert.ok(args.length === 0 || (args.length === 2 && args[0] === '--archive' && isAbsolute(args[1])), 'usage: node scripts/test-distribution.mjs [--archive /absolute/public-release.tar.gz]');
const fixture = mkdtempSync(join(realpathSync(tmpdir()), 'fabric-distribution-'));
const env = {
  PATH: '/usr/bin:/bin', HOME: join(fixture, 'home'), TMPDIR: fixture,
  XDG_DATA_HOME: join(fixture, 'data'), XDG_CONFIG_HOME: join(fixture, 'config'),
  PI_CODING_AGENT_DIR: join(fixture, 'omp'), CODEX_HOME: join(fixture, 'codex'),
  FABRIC_PLUGIN_REGISTRY: join(fixture, 'unregistered'),
  GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null',
  GIT_AUTHOR_NAME: 'Distribution proof', GIT_AUTHOR_EMAIL: 'proof@example.invalid',
  GIT_COMMITTER_NAME: 'Distribution proof', GIT_COMMITTER_EMAIL: 'proof@example.invalid',
};
function run(binary, argv, { status = 0, cwd = fixture, environment = env } = {}) {
  const result = spawnSync(binary, argv, { cwd, env: environment, encoding: 'utf8', timeout: 180_000, maxBuffer: 8 * 1024 * 1024 });
  process.stderr.write(`${JSON.stringify({ command: [binary, ...argv], cwd, exit: result.status, signal: result.signal })}\nstdout:\n${result.stdout ?? ''}\nstderr:\n${result.stderr ?? ''}\n`);
  assert.ifError(result.error);
  assert.equal(result.status, status, `${binary} ${argv.join(' ')}\nstdout: ${result.stdout}\nstderr: ${result.stderr}`);
  return result;
}
function removePrivate(path) {
  const stat = lstatSync(path);
  if (stat.isSymbolicLink()) return;
  chmodSync(path, stat.isDirectory() ? 0o700 : 0o600);
  if (stat.isDirectory()) for (const name of readdirSync(path)) removePrivate(join(path, name));
}
try {
  for (const key of ['HOME', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'PI_CODING_AGENT_DIR', 'CODEX_HOME', 'FABRIC_PLUGIN_REGISTRY']) mkdirSync(env[key], { mode: 0o700 });
  const archive = args.length ? args[1] : join(fixture, manifest.archive);
  if (!args.length) run('/usr/bin/curl', ['--fail', '--location', '--silent', '--show-error', '--proto', '=https', '--tlsv1.2', '--max-time', '180', `https://github.com/satansdeer/fabric/releases/download/v0.1.2/${manifest.archive}`, '--output', archive]);
  assert.equal(createHash('sha256').update(readFileSync(archive)).digest('hex'), expectedHash, 'refuse any different archive before invoking installer');
  run('/usr/bin/ruby', [join(distribution, 'test-install.rb'), archive]);
  const selectors = prefix => ['--version', '0.1.2', '--platform', 'macos-arm64', '--prefix', prefix, '--archive', archive];
  const launch = (argv, options) => run(process.execPath, [launcher, ...argv], options);
  const rejected = join(fixture, 'rejected');
  for (const argv of [
    [], ['install'], ['install', ...selectors(rejected), '--unknown'],
    ['install', ...selectors(rejected), '--prefix', rejected],
    ['install', ...selectors(rejected), '--', '--version'],
    ['exec', ...selectors(rejected)], ['exec', ...selectors(rejected), '--'],
    ['install', ...selectors(rejected), '--trust-plugins=yes'],
    ['install', '--version', '999.0.0', '--platform', 'macos-arm64', '--prefix', rejected],
    ['install', '--version', '0.1.2', '--platform', 'linux-x64', '--prefix', rejected],
    ['install', '--version', '0.1.2', '--platform', 'macos-arm64', '--prefix', 'relative'],
    ['install', ...selectors(rejected).slice(0, -2), '--archive', join(fixture, 'offline-missing.tar.gz')],
  ]) {
    const result = launch(argv, { status: 1 });
    assert.ok(result.stderr.trim(), 'rejection explains the problem');
    assert.equal(existsSync(rejected), false, 'invalid/offline inputs do not create an installation');
  }
  const installed = join(fixture, 'install with spaces');
  launch(['install', ...selectors(installed)]);
  assert.deepEqual(JSON.parse(readFileSync(join(installed, 'release.json'), 'utf8')), manifest);
  assert.equal(existsSync(join(installed, 'registry')), false, 'no implicit plugin registration consent');
  const original = readFileSync(join(installed, 'bin/fabric'));
  launch(['install', ...selectors(installed)], { status: 1 });
  assert.deepEqual(readFileSync(join(installed, 'bin/fabric')), original, 'existing prefix is unchanged');
  const version = launch(['exec', ...selectors(join(fixture, 'exec version')), '--', '--version']);
  assert.equal(version.stdout.trim(), manifest.cli_version);
  const doctor = launch(['exec', ...selectors(join(fixture, 'exec doctor')), '--', '--root', join(fixture, 'native root ; no shell'), 'doctor', '--json']);
  assert.equal(JSON.parse(doctor.stdout).platform.supported, true);
  const invalidNative = run(join(installed, 'bin/fabric'), ['--not-a-real-native-flag'], { status: 2 });
  const invalidForward = launch(['exec', ...selectors(join(fixture, 'exec invalid')), '--', '--not-a-real-native-flag'], { status: 2 });
  assert.ok(invalidNative.stderr.includes('--not-a-real-native-flag'));
  assert.ok(invalidForward.stderr.includes('--not-a-real-native-flag'), 'native error/status are forwarded, not replaced');
  const repo = join(fixture, 'project');
  mkdirSync(repo, { mode: 0o700 });
  run('/usr/bin/git', ['init', '--quiet'], { cwd: repo });
  writeFileSync(join(repo, 'tracked'), 'distribution proof\n');
  run('/usr/bin/git', ['add', 'tracked'], { cwd: repo });
  run('/usr/bin/git', ['-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'fixture'], { cwd: repo });
  const upPrefix = join(fixture, 'up');
  const up = launch(['up', ...selectors(upPrefix), '--', '--check', '--json', '--root', join(fixture, 'up state')], { cwd: repo });
  assert.equal(JSON.parse(up.stdout).source, repo, 'up honestly invokes released init onboarding');
  assert.equal(existsSync(join(upPrefix, 'registry')), false, 'up execution is not plugin-registration consent');
  const trusted = join(fixture, 'explicit trust');
  launch(['install', ...selectors(trusted), '--trust-plugins']);
  const binary = join(trusted, 'bin/fabric');
  for (const plugin of [...manifest.plugins, ...manifest.target_plugins]) {
    const check = run(binary, ['plugins', '--registry', join(trusted, 'registry'), 'check', plugin.id], { status: plugin.id === 'local' ? 0 : 1, environment: { ...env, FABRIC_PLUGIN_REGISTRY: join(trusted, 'registry') } });
    if (plugin.id === 'local') assert.deepEqual(JSON.parse(check.stdout), { compatible: true, healthy: true });
    else {
      const diagnostic = JSON.parse(check.stderr).error;
      assert.equal(diagnostic.kind, 'output');
      assert.ok(diagnostic.message.includes(plugin.id) && diagnostic.message.includes('PATH'), 'unbundled harness CLI needs actionable discovery diagnostics');
    }
  }
  const cancelledLauncher = join(fixture, 'cancellation');
  mkdirSync(cancelledLauncher, { mode: 0o700 });
  writeFileSync(join(cancelledLauncher, 'launcher.mjs'), readFileSync(launcher));
  // Controlled blocking installer: cancellation must reach its child, even if Ruby exits 0.
  writeFileSync(join(cancelledLauncher, 'install.rb'), `
prefix = ARGV[ARGV.index('--prefix') + 1]
child = fork { sleep 30 }
%w[INT TERM HUP].each do |signal|
  trap(signal) { Process.wait(child); File.write(prefix + '.cancelled', signal); exit 0 }
end
File.write(prefix + '.ready', "#{Process.pid} #{child}")
sleep 30
File.write(prefix + '.continued', 'unexpected post-cancellation work')
Process.wait(child)
`);
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    const prefix = join(cancelledLauncher, signal);
    const child = spawn(process.execPath, [join(cancelledLauncher, 'launcher.mjs'), 'up', ...selectors(prefix)], { cwd: fixture, env, detached: true, stdio: 'ignore' });
    const closed = new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('exit', (code, received) => resolve({ code, signal: received }));
    });
    let installerPids = [];
    try {
      const deadline = Date.now() + 5000;
      while (!existsSync(prefix + '.ready') && Date.now() < deadline) await delay(10);
      assert.ok(existsSync(prefix + '.ready'), 'blocking installer and child must be ready before cancellation');
      installerPids = readFileSync(prefix + '.ready', 'utf8').split(' ').map(Number);
      process.kill(child.pid, signal); // Deliberately target only the launcher PID, not its group.
      const result = await Promise.race([closed, delay(5000, null, { ref: false }).then(() => { throw new Error('launcher did not finish cancellation'); })]);
      assert.ok(result.signal === signal || result.code === 128 + ({ SIGINT: 2, SIGTERM: 15, SIGHUP: 1 })[signal], 'launcher must preserve cancellation, not execute native up after a successful installer trap');
      assert.equal(readFileSync(prefix + '.cancelled', 'utf8'), signal.slice(3), 'installer reaped its cancelled child');
      assert.equal(existsSync(prefix + '.continued'), false, 'cancelled installation must not continue');
    } finally {
      if (!existsSync(prefix + '.cancelled')) {
        for (const pid of [child.pid, ...installerPids]) {
          for (const target of [-pid, pid]) {
            try { process.kill(target, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
          }
        }
        await closed;
      }
    }
  }
  console.log(JSON.stringify({ proof: 'PINNED_PUBLIC_DISTRIBUTION_CONSUMER', version: manifest.version, source_sha: manifest.source_sha, sha256: expectedHash, executable_sha256: manifest.files['bin/fabric'], ruby: '/usr/bin/ruby', launcher: 'actual native install/exec/init', launcher_cancellation: ['SIGINT', 'SIGTERM', 'SIGHUP'], no_implicit_plugin_consent: true, no_overwrite: true, native_error_status: 2, providers: 'not invoked', manager: 'not started', namespace: 'private unpublished unowned candidate' }));
} finally {
  removePrivate(fixture);
  rmSync(fixture, { recursive: true, force: true });
}
