#!/usr/bin/env node
// Only the API/reporting process receives GH_TOKEN. Native children get an explicit empty context.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const fabricPin = Object.freeze({
  repository: 'satansdeer/fabric', tag: 'v0.1.21',
  source_sha: '8d12ad30e9d8ae5ecc5d3541527c8dbc6037ad32',
  archive_sha256: 'b80336b976c2e688af6ef6fd3e24fd92be9af94a41b2af02a307f8157860429b',
  executable_sha256: '4b4ac89513540a083eae235439c887b946b0f374ab525592541effa7beb7d1bb',
});
const ompRepo = 'can1357/oh-my-pi';
const sourceRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const hash = bytes => createHash('sha256').update(bytes).digest('hex');

export function stableRelease(release) {
  assert.ok(Number.isSafeInteger(release.id) && release.id > 0, 'release ID missing');
  assert.equal(release.draft, false, 'draft release rejected');
  assert.equal(release.prerelease, false, 'prerelease rejected');
  assert.match(release.tag_name, /^v?\d+\.\d+\.\d+$/, 'stable numeric release tag required');
  return release;
}
export function verifyDigest(bytes, digest) {
  assert.equal(typeof digest, 'string', 'authenticated upstream SHA256 digest missing');
  assert.match(digest, /^sha256:[a-f0-9]{64}$/, 'authenticated upstream SHA256 digest unverifiable');
  const actual = hash(bytes);
  assert.equal(actual, digest.slice(7), 'download SHA256 mismatch; refusing execution/extraction');
  return actual;
}
function identity(release) {
  return { id: release.id, tag: release.tag_name, target_commitish: release.target_commitish,
    published_at: release.published_at, assets: release.assets.map(({ id, name, digest, size }) => ({ id, name, digest, size })) };
}
function github(token) {
  assert.ok(token, 'GH_TOKEN required for authenticated GitHub API');
  return async (path, options = {}) => {
    assert.ok(path.startsWith('repos/'), 'only repository API paths permitted');
    const response = await fetch(`https://api.github.com/${path}`, {
      method: options.method || 'GET', redirect: 'follow', signal: AbortSignal.timeout(180_000),
      headers: { Authorization: `Bearer ${token}`, Accept: options.binary ? 'application/octet-stream' : 'application/vnd.github+json',
        'Cache-Control': 'no-cache', 'X-GitHub-Api-Version': '2022-11-28', ...(options.body ? { 'Content-Type': 'application/json' } : {}) },
      ...(options.body ? { body: JSON.stringify(options.body) } : {}),
    });
    assert.ok(response.ok, `GitHub API ${options.method || 'GET'} ${path}: HTTP ${response.status}`);
    return options.binary ? Buffer.from(await response.arrayBuffer()) : response.json();
  };
}
async function sourceSha(api, repository, tag) {
  let object = (await api(`repos/${repository}/git/ref/tags/${encodeURIComponent(tag)}`)).object;
  for (let depth = 0; object.type === 'tag' && depth < 5; depth++) {
    assert.match(object.sha, /^[a-f0-9]{40}$/);
    object = (await api(`repos/${repository}/git/tags/${object.sha}`)).object;
  }
  assert.equal(object.type, 'commit', 'release tag must resolve to a commit');
  assert.match(object.sha, /^[a-f0-9]{40}$/);
  return object.sha;
}

export function failureKey(result) {
  // Missing upstream identity deliberately groups repeated resolution outages, not one issue/day.
  return hash(JSON.stringify([result.fabric_pin, result.omp?.id ?? result.omp?.tag ?? 'unresolved', result.category, result.stage]));
}
export async function reportFailure(api, repository, result) {
  assert.match(repository, /^[\w.-]+\/[\w.-]+$/);
  const marker = `<!-- fabric-omp-canary:${failureKey(result)} -->`;
  const body = `${marker}\n## Daily upstream OMP compatibility canary failed\n\n` +
    `Category: **${result.category}**; failed stage: **${result.stage}**.\n` +
    `Evidence: ${result.run_url}\n\n` +
    '```json\n' + JSON.stringify(result, null, 2) + '\n```\n\n' +
    'Download the isolated evidence artifact from this run. Infrastructure: repair API/download/runner access. ' +
    'Digest: independently verify upstream bytes/metadata; never bypass verification. Probe: reproduce the shared native up-smoke, ' +
    'inspect admission/get_state/tool logs, then review compatibility changes. No model/provider inference ran; do not force a provider/model. ' +
    'Fabric pin changes require independent public downloaded-byte/native acceptance; no automatic promotion.\n';
  for (let page = 1; page <= 100; page++) {
    const issues = await api(`repos/${repository}/issues?state=all&per_page=100&page=${page}`);
    const existing = issues.find(issue => !issue.pull_request && issue.body?.includes(marker));
    if (existing) return api(`repos/${repository}/issues/${existing.number}`, { method: 'PATCH', body: { body, state: 'open' } });
    if (issues.length < 100) return api(`repos/${repository}/issues`, {
      method: 'POST', body: { title: `[OMP canary] ${result.omp?.tag || 'unresolved upstream'}: ${result.category}/${result.stage}`, body },
    });
  }
  throw new Error('Issue scan limit reached; refusing a potentially duplicate issue');
}

async function canary(evidenceArg) {
  const evidence = resolve(evidenceArg);
  mkdirSync(evidence, { recursive: true, mode: 0o700 });
  const result = { status: 'FAIL', category: 'infrastructure', stage: 'startup', fabric_pin: fabricPin,
    probe_source_sha: process.env.GITHUB_SHA || 'local-checkout',
    run_url: process.env.GITHUB_RUN_ID ? `https://github.com/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}` : 'local run',
    inference: 'NOT_RUN' };
  const save = () => writeFileSync(join(evidence, 'result.json'), JSON.stringify(result, null, 2) + '\n');
  const stage = (category, name) => { result.category = category; result.stage = name; save(); };
  save();
  try {
    assert.equal(process.platform, 'darwin', 'native macOS runner required');
    assert.equal(process.arch, 'arm64', 'accepted Fabric public binary is arm64 only');
    const api = github(process.env.GH_TOKEN);
    stage('infrastructure', 'resolve-omp');
    const omp = await api(`repos/${ompRepo}/releases/latest`);
    result.omp = identity(omp); save(); stableRelease(omp);
    result.omp.source_sha = await sourceSha(api, ompRepo, omp.tag_name); save();
    stage('infrastructure', 'resolve-fabric');
    const fabric = await api(`repos/${fabricPin.repository}/releases/tags/${fabricPin.tag}`);
    result.fabric = identity(fabric); save(); stableRelease(fabric);
    stage('infrastructure', 'resolve-fabric-source');
    assert.equal(fabric.tag_name, fabricPin.tag);
    result.fabric.source_sha = await sourceSha(api, fabricPin.repository, fabricPin.tag); save();
    stage('digest', 'fabric-source');
    assert.equal(result.fabric.source_sha, fabricPin.source_sha, 'accepted Fabric source tag changed');

    const scratch = realpathSync(mkdtempSync(join(tmpdir(), 'fabric-omp-canary-')));
    const env = { PATH: '/usr/bin:/bin', HOME: join(scratch, 'home'), TMPDIR: join(scratch, 'tmp'),
      XDG_CONFIG_HOME: join(scratch, 'config'), XDG_DATA_HOME: join(scratch, 'data'),
      PI_CODING_AGENT_DIR: join(scratch, 'agent'), CODEX_HOME: join(scratch, 'codex'),
      FABRIC_SMOKE_NATIVE_AGENT_DIR: join(scratch, 'agent'), FABRIC_SMOKE_EVIDENCE: join(evidence, 'probe'),
      GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null', LC_ALL: 'C' };
    for (const key of ['HOME', 'TMPDIR', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'PI_CODING_AGENT_DIR', 'CODEX_HOME', 'FABRIC_SMOKE_EVIDENCE']) mkdirSync(env[key], { mode: 0o700 });
    const bun = realpathSync(process.env.CANARY_BUN);
    let sequence = 0;
    function run(program, args, cwd = scratch) {
      const output = spawnSync(program, args, { cwd, env, encoding: 'utf8', timeout: 240_000, maxBuffer: 16 * 1024 * 1024 });
      const prefix = join(evidence, `stage-${++sequence}`);
      writeFileSync(`${prefix}.json`, JSON.stringify({ stage: result.stage, program, args, status: output.status, error: output.error?.code }));
      writeFileSync(`${prefix}.stdout`, output.stdout || ''); writeFileSync(`${prefix}.stderr`, output.stderr || '');
      assert.ifError(output.error);
      assert.equal(output.status, 0, `${result.stage} failed; see stage-${sequence} logs`);
      return output.stdout.trim();
    }
    async function download(repository, release, name, label) {
      stage('digest', `${label}-metadata`);
      const matches = release.assets.filter(asset => asset.name === name);
      assert.equal(matches.length, 1, `expected exactly one asset ${name}`);
      const asset = matches[0];
      assert.ok(Number.isSafeInteger(asset.id) && asset.id > 0, 'asset ID missing');
      // Check digest availability before downloading; never use a locally invented checksum as upstream proof.
      assert.match(asset.digest || '', /^sha256:[a-f0-9]{64}$/, `authenticated SHA256 digest missing/unverifiable for ${name}`);
      stage('infrastructure', `${label}-download`);
      const bytes = await api(`repos/${repository}/releases/assets/${asset.id}`, { binary: true });
      stage('digest', `${label}-digest`);
      result.downloads ??= [];
      const observed = { repository, release_id: release.id, asset_id: asset.id, name, sha256: hash(bytes), verified: false };
      result.downloads.push(observed); save();
      verifyDigest(bytes, asset.digest);
      observed.verified = true; save();
      const path = join(scratch, name); writeFileSync(path, bytes, { mode: 0o600, flag: 'wx' });
      return path;
    }
    const archiveName = `fabric-${fabricPin.tag}-macos-arm64.tar.gz`;
    const archive = await download(fabricPin.repository, fabric, archiveName, 'fabric-archive');
    assert.equal(hash(readFileSync(archive)), fabricPin.archive_sha256, 'accepted Fabric archive pin mismatch');
    const inventory = await download(fabricPin.repository, fabric, `fabric-${fabricPin.tag}-macos-arm64.json`, 'fabric-inventory');
    const checksum = await download(fabricPin.repository, fabric, `${archiveName}.sha256`, 'fabric-checksum');
    stage('digest', 'fabric-inventory');
    assert.equal(readFileSync(checksum, 'utf8'), `${fabricPin.archive_sha256}  ${archiveName}\n`);
    const prefix = archiveName.replace(/\.tar\.gz$/, '');
    const names = run('/usr/bin/tar', ['-tzf', archive]).split('\n');
    for (const name of names) assert.ok(name.startsWith(`${prefix}/`) && !name.split('/').some(part => part === '..' || part === '.'), 'unsafe archive path');
    const install = join(scratch, 'install'); mkdirSync(install, { mode: 0o700 });
    run('/usr/bin/tar', ['-xzf', archive, '-C', install, '--no-same-owner']);
    const bundle = join(install, prefix);
    const manifest = JSON.parse(readFileSync(inventory, 'utf8'));
    writeFileSync(join(evidence, 'fabric-inventory.json'), JSON.stringify(manifest, null, 2));
    assert.deepEqual(JSON.parse(readFileSync(join(bundle, 'release.json'), 'utf8')), manifest);
    assert.equal(manifest.tag, fabricPin.tag); assert.equal(manifest.version, fabricPin.tag.slice(1));
    assert.equal(manifest.source_sha, fabricPin.source_sha); assert.equal(manifest.build_kind, 'release');
    assert.equal(manifest.platform, 'macos-arm64'); assert.equal(manifest.archive, archiveName);
    const binary = join(bundle, 'bin/fabric');
    result.fabric.executable_sha256 = hash(readFileSync(binary)); save();
    for (const [name, digest] of Object.entries(manifest.files)) {
      assert.ok(name && !name.startsWith('/') && name.split('/').every(part => part && part !== '..' && part !== '.'));
      const path = join(bundle, name);
      assert.ok(lstatSync(path).isFile() && realpathSync(path) === path, 'payload must be regular and non-symlink');
      assert.equal(hash(readFileSync(path)), digest, `inventory mismatch: ${name}`);
    }
    assert.equal(result.fabric.executable_sha256, fabricPin.executable_sha256);
    assert.equal(manifest.files['bin/fabric'], fabricPin.executable_sha256);
    assert.equal(manifest.files[manifest.omp_extension.path], manifest.omp_extension.sha256);
    assert.equal(manifest.files[manifest.fixture.path], manifest.fixture.sha256);
    const { directory } = await import(pathToFileURL(join(bundle, 'sdk/package.mjs')));
    assert.deepEqual(manifest.plugins.map(p => p.id), ['omp', 'codex']);
    assert.deepEqual(manifest.target_plugins.map(p => p.id), ['local']);
    for (const plugin of [...manifest.plugins, ...manifest.target_plugins]) {
      const pkg = directory(join(bundle, 'plugins', plugin.id));
      assert.equal(pkg.hash, plugin.sha256); assert.equal(pkg.info.version, plugin.version);
    }
    const ompBinary = await download(ompRepo, omp, 'omp-darwin-arm64', 'omp-binary');
    chmodSync(ompBinary, 0o700);
    stage('probe', 'native-versions');
    assert.equal(run(binary, ['--version']), manifest.cli_version);
    result.omp.observed_version = run(ompBinary, ['--version']);
    assert.equal(result.omp.observed_version, `omp/${omp.tag_name.replace(/^v/, '')}`); save();
    stage('infrastructure', 'empty-agent');
    // A real empty SQLite database satisfies the existing metadata-only native-context check.
    // No credential rows, provider/model configuration or dummy API key is installed.
    run(bun, ['-e', 'import { Database } from "bun:sqlite"; const db = new Database(process.argv[1]); db.exec("PRAGMA user_version=0"); db.close();', join(env.PI_CODING_AGENT_DIR, 'agent.db')]);
    stage('probe', 'native-up-typed-tools');
    run(bun, [join(sourceRoot, 'integrations/omp/up-smoke.ts'), binary, ompBinary]);
    result.status = 'PASS'; result.category = 'none'; result.stage = 'complete'; save();
  } catch (error) {
    // API errors never include headers/bodies; child logs originate only from our credential-free fixture.
    result.error = error.message; save(); process.exitCode = 1;
  }
  console.log(JSON.stringify(result, null, 2));
}

async function main() {
  const [mode, evidenceArg] = process.argv.slice(2);
  assert.ok(evidenceArg && ['run', 'report'].includes(mode), 'usage: node scripts/omp-canary.mjs run|report EVIDENCE_DIRECTORY');
  if (mode === 'run') return canary(evidenceArg);
  const path = join(resolve(evidenceArg), 'result.json');
  const result = existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')) : {
    status: 'FAIL', category: 'infrastructure', stage: 'workflow-setup-or-interruption', fabric_pin: fabricPin,
    error: 'Canary result unavailable: inspect setup, cancellation, timeout and artifact steps.', inference: 'NOT_RUN',
    run_url: `https://github.com/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`,
  };
  if (result.status === 'PASS' && process.env.CANARY_JOB_RESULT === 'failure') {
    result.status = 'FAIL'; result.category = 'infrastructure'; result.stage = 'workflow-evidence-or-post-probe';
    result.error = 'Native probe passed but the workflow failed; inspect evidence upload and remaining steps.';
  }
  if (result.status !== 'PASS') await reportFailure(github(process.env.GH_TOKEN), process.env.GITHUB_REPOSITORY, result);
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
