#!/usr/bin/env node
// Build tooling only. Installation never invokes Cargo or executes a source checkout.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { reference } from '../sdk/typescript/cli.mjs';
import { directory, sha256 } from '../sdk/typescript/package.mjs';

const args = process.argv.slice(2);
const local = args[0] === '--local';
if (local) args.shift();
assert.equal(args.length, 2, 'usage: node scripts/package-release.mjs [--local] FULL_SOURCE_SHA NEW_OUTPUT_DIRECTORY');
const [source, outputArg] = args;
assert.match(source, /^[a-f0-9]{40}$/);
const repo = realpathSync(join(dirname(fileURLToPath(import.meta.url)), '..'));
const output = resolve(outputArg);
assert.ok(isAbsolute(outputArg) && !existsSync(output), 'output must be a new absolute directory');
assert.equal(realpathSync(dirname(output)), dirname(output), 'output parent must be canonical');
assert.ok(!output.startsWith(`${repo}/`), 'keep release/build output outside the checkout');
function run(command, argv, options = {}) {
  const result = spawnSync(command, argv, { cwd: repo, encoding: 'utf8', ...options });
  assert.ifError(result.error);
  assert.equal(result.status, 0, `${command} ${argv.join(' ')}\n${result.stderr || ''}`);
  return result.stdout?.trim();
}
assert.equal(process.platform, 'darwin', 'only macOS arm64 is supported');
assert.equal(process.arch, 'arm64');
assert.equal(run('/usr/bin/git', ['rev-parse', 'HEAD']), source, 'never silently build current HEAD');
const metadata = JSON.parse(run('cargo', ['metadata', '--locked', '--offline', '--no-deps', '--format-version', '1']));
const version = metadata.packages.find(pkg => pkg.name === 'fabric-cli').version;
assert.match(version, /^\d+\.\d+\.\d+$/, 'bootstrap publishes stable workspace versions only');
const tag = `v${version}`;
if (!local) {
  assert.equal(run('/usr/bin/git', ['status', '--porcelain', '--untracked-files=normal']), '', 'release source must be clean');
  assert.equal(run('/usr/bin/git', ['rev-parse', `refs/tags/${tag}^{commit}`]), source, 'version tag must pin the reviewed source');
  run('/usr/bin/git', ['merge-base', '--is-ancestor', source, 'refs/remotes/origin/main']);
}
const buildKind = local ? 'local-unreviewed' : 'release';
const artifactVersion = local ? `${version}-local.${source.slice(0, 12)}` : version;
const name = `fabric-v${artifactVersion}-macos-arm64`;
const cliVersion = `${version} (source ${source}; macos-arm64; ${buildKind})`;
const build = mkdtempSync(join(realpathSync(tmpdir()), 'fabric-release-build-'));
mkdirSync(output, { mode: 0o700 });
const stage = join(output, name);
mkdirSync(stage, { mode: 0o700 });
try {
  run('cargo', ['build', '--locked', '--release', '--target', 'aarch64-apple-darwin', '-p', 'fabric-cli', '-p', 'fabric-reference-harnesses', '--bins'], {
    stdio: 'inherit', env: { ...process.env, CARGO_TARGET_DIR: build, FABRIC_BUILD_VERSION: cliVersion },
  });
  const files = {};
  function copy(from, to, executable = false, transform) {
    const destination = join(stage, to);
    mkdirSync(dirname(destination), { recursive: true, mode: 0o700 });
    if (transform) writeFileSync(destination, transform(readFileSync(from, 'utf8')));
    else copyFileSync(from, destination);
    chmodSync(destination, executable ? 0o700 : 0o600);
    files[to] = sha256(readFileSync(destination));
  }
  for (const binary of ['fabric', 'fabric-fake-harness', 'fabric-local-target', 'fabric-remote-agent']) copy(join(build, 'aarch64-apple-darwin/release', binary), `bin/${binary}`, true);
  const plugins = [];
  const targetPlugins = [];
  mkdirSync(join(stage, 'plugins'), { mode: 0o700 });
  for (const id of ['omp', 'codex', 'local']) {
    const path = join(stage, 'plugins', id);
    reference(id, path, join(build, 'aarch64-apple-darwin/release', id === 'local' ? 'fabric-local-target' : 'fabric-reference-harnesses'));
    const pkg = directory(path);
    for (const [file, bytes] of pkg.files) files[`plugins/${id}/${file}`] = sha256(bytes);
    if (id === 'local') targetPlugins.push({ id, version: pkg.info.version, protocol: 1, sha256: pkg.hash });
    else plugins.push({ id, version: pkg.info.version, protocol: 1, sha256: pkg.hash, supported_cli: id === 'omp' ? 'OMP print JSON CLI contract; observed version bound per run' : 'codex-cli 0.171.0' });
  }
  // SDK installer/scaffolder runtime, types and samples; no npm lifecycle hooks.
  for (const file of ['cli.mjs', 'package.mjs', 'protocol.mjs', 'conformance.mjs', 'index.mjs', 'index.d.mts', 'webhook.mjs']) copy(join(repo, 'sdk/typescript', file), `sdk/${file}`);
  for (const kind of ['connector', 'harness', 'target']) copy(join(repo, 'sdk/typescript/samples', `${kind}.ts`), `sdk/samples/${kind}.ts`);
  for (const file of ['fabric.ts', 'host-smoke.ts', 'up-smoke.ts']) copy(join(repo, 'integrations/omp', file), `integrations/omp/${file}`);
  for (const file of ['smoke-release.mjs', 'fabric-webhook.mjs']) copy(join(repo, 'scripts', file), `scripts/${file}`);
  for (const file of ['bridge.mjs', 'connector.mjs', 'plugin.mjs']) {
    copy(join(repo, 'integrations/github', file), `integrations/github/${file}`, file === 'bridge.mjs',
      text => text.replaceAll('../../sdk/typescript/', '../../sdk/'));
  }
  copy(join(repo, 'scripts/check-fabric-github.mjs'), 'scripts/check-fabric-github.mjs', true);
  copy(join(repo, 'docs/GITHUB_CONNECTOR.md'), 'docs/GITHUB_CONNECTOR.md', false,
    text => text.replaceAll('sdk/typescript/', 'sdk/'));
  copy(join(repo, 'docs/CONTROL_SURFACES.md'), 'docs/CONTROL_SURFACES.md');
  // The smoke's SDK imports are dynamic and resolve inside the extracted bundle.
  const manifest = {
    version, tag, source_sha: source, build_kind: buildKind, platform: 'macos-arm64',
    archive: `${name}.tar.gz`, cli_version: `fabric ${cliVersion}`,
    source_url: `https://github.com/satansdeer/fabric/tree/${source}`,
    build: { rustc: run('rustc', ['--version']), cargo: run('cargo', ['--version']), cargo_lock_sha256: sha256(readFileSync(join(repo, 'Cargo.lock'))) },
    plugins,
    omp: { version: 'omp/19.1.0', url: 'https://github.com/can1357/oh-my-pi/releases/download/v19.1.0/omp-darwin-arm64', sha256: 'dca5643669ed5e553f707bd065499babd0fe3030696bbb24c9da09c798af5abb', bundled: false },
    omp_extension: { path: 'integrations/omp/fabric.ts', sha256: files['integrations/omp/fabric.ts'], source_url: `https://github.com/satansdeer/fabric/blob/${source}/integrations/omp/fabric.ts` },
    fixture: { path: 'bin/fabric-fake-harness', sha256: files['bin/fabric-fake-harness'], purpose: 'provider-free fake-a/fake-b only; not a native model worker' },
    target_plugins: targetPlugins, files,
  };
  const json = JSON.stringify(manifest, null, 2) + '\n';
  writeFileSync(join(stage, 'release.json'), json, { mode: 0o600, flag: 'wx' });
  writeFileSync(join(output, `${name}.json`), json, { mode: 0o600, flag: 'wx' });
  run('/usr/bin/tar', ['-czf', join(output, manifest.archive), '-C', output, name], { env: { PATH: '/usr/bin:/bin', COPYFILE_DISABLE: '1' } });
  const hash = sha256(readFileSync(join(output, manifest.archive)));
  writeFileSync(join(output, `${manifest.archive}.sha256`), `${hash}  ${manifest.archive}\n`, { mode: 0o600, flag: 'wx' });
  const notes = `macOS arm64 native bootstrap ${tag}\n\nSource: ${manifest.source_url}\nBuild kind: ${buildKind}\nArchive: ${manifest.archive}\nSHA256: ${hash}\n\nOfficial plugin content pins (protocol 1):\n${plugins.map(p => `- ${p.id} ${p.version}: ${p.sha256}; supports ${p.supported_cli}`).join('\n')}\n${targetPlugins.map(p => `- target ${p.id} ${p.version}: ${p.sha256}`).join('\n')}\n\nOMP extension: ${manifest.omp_extension.source_url}\nExtension SHA256: ${manifest.omp_extension.sha256}\nProvider-free fixture: ${manifest.fixture.path} (${manifest.fixture.sha256})\nTrusted-local target bundled as executable and installable package. OMP/Codex CLIs are separately installed, not bundled.\n\nNo cloud, PTY, persistent-environment, Apple signing/notarization, Intel/Linux, npm/npx/Homebrew, provider-backed or full swarm-parity claim. #7 stays open. Publication is not downloaded-install proof; link the later smoke evidence in #38 and #7 before release dogfooding.\n`;
  writeFileSync(join(output, 'release-notes.md'), notes + '\nResult contract: results/workers results retain canonical relative_path, size_bytes and sha256 and add local_read_path only after full sealed-artifact verification. Read local_read_path directly from any parent cwd on the manager host. Invalid registered artifacts fail retrieval; runs artifacts stays metadata-only. See docs/CONTROL_SURFACES.md. This is not native model-backed parity evidence.\n', { mode: 0o600, flag: 'wx' });
  console.log(JSON.stringify({ archive: join(output, manifest.archive), sha256: hash, source_sha: source, build_kind: buildKind, plugins }, null, 2));
} finally {
  rmSync(build, { recursive: true, force: true });
}
