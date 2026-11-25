#!/usr/bin/env node
// Deterministic, provider/network-free regression check. No GitHub mutations.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { fabricPin, failureKey, reportFailure, stableRelease, verifyDigest } from './omp-canary.mjs';

const bytes = Buffer.from('independently authenticated release bytes');
const digest = `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
assert.equal(verifyDigest(bytes, digest), digest.slice(7));
assert.throws(() => verifyDigest(Buffer.from('changed bytes'), digest), /SHA256 mismatch/);
for (const invalid of [undefined, null, '', 'sha256:1234', `sha512:${'a'.repeat(64)}`]) {
  assert.throws(() => verifyDigest(bytes, invalid), /upstream SHA256 digest/);
}
const latest = { id: 123, tag_name: 'v99.3.2', draft: false, prerelease: false };
assert.equal(stableRelease(latest), latest, 'compatible future releases are not version-allowlisted');
for (const invalid of [{ prerelease: true }, { draft: true }, { tag_name: 'v99.3.3-rc.1' }, { id: undefined }]) {
  assert.throws(() => stableRelease({ ...latest, ...invalid }));
}

const result = { status: 'FAIL', category: 'probe', stage: 'native-up-typed-tools', fabric_pin: fabricPin,
  omp: { id: 123, tag: 'v99.3.2', source_sha: 'a'.repeat(40) }, run_url: 'https://github.com/owner/repo/actions/runs/1', inference: 'NOT_RUN' };
const issues = [{ number: 1, body: 'unrelated issue' }];
const mutations = [];
const api = async (path, options = {}) => {
  assert.ok(path.startsWith('repos/owner/repo/issues'));
  if (!options.method) return issues;
  mutations.push(options);
  if (options.method === 'POST') {
    const issue = { number: 2, ...options.body }; issues.push(issue); return issue;
  }
  assert.equal(path, 'repos/owner/repo/issues/2');
  Object.assign(issues[1], options.body); return issues[1];
};
await reportFailure(api, 'owner/repo', result);
assert.equal(mutations[0].method, 'POST');
assert.ok(issues[1].body.includes(result.run_url));
assert.ok(issues[1].body.includes(result.omp.source_sha));
issues[1].state = 'closed';
const tomorrow = { ...result, run_url: 'https://github.com/owner/repo/actions/runs/2' };
await reportFailure(api, 'owner/repo', tomorrow);
assert.equal(mutations[1].method, 'PATCH');
assert.equal(issues[1].state, 'open');
assert.ok(issues[1].body.includes(tomorrow.run_url));
assert.ok(!issues[1].body.includes('actions/runs/1'), 'replace daily evidence; do not grow issue indefinitely');
assert.equal(failureKey(result), failureKey(tomorrow));
assert.notEqual(failureKey(result), failureKey({ ...result, omp: { ...result.omp, id: 124 } }));
assert.notEqual(failureKey(result), failureKey({ ...result, category: 'digest', stage: 'omp-binary-digest' }));
const unresolved = { ...result, omp: undefined, category: 'infrastructure', stage: 'resolve-omp' };
assert.equal(failureKey(unresolved), failureKey({ ...unresolved, run_url: tomorrow.run_url }));
assert.match(failureKey(unresolved), /^[a-f0-9]{64}$/);

let pages = 0;
await reportFailure(async (path, options) => {
  if (options) { assert.equal(options.method, 'PATCH'); return {}; }
  pages++;
  return pages === 1 ? Array.from({ length: 100 }, (_, number) => ({ number, body: 'unrelated' })) : [issues[1]];
}, 'owner/repo', result);
assert.equal(pages, 2, 'dedup must search beyond the first issue page, including closed issues');
await assert.rejects(reportFailure(async () => { throw new Error('API unavailable'); }, 'owner/repo', result), /API unavailable/);
let writes = 0;
await assert.rejects(reportFailure(async (_path, options) => {
  if (options) writes++;
  return Array.from({ length: 100 }, () => ({ body: 'unrelated' }));
}, 'owner/repo', result), /refusing a potentially duplicate issue/);
assert.equal(writes, 0);
console.log('PASS: authenticated digests, stable-only admission, release/stage failure keys and paginated issue dedup (including unresolved identity/API failure)');
