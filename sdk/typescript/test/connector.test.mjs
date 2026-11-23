import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, realpathSync, writeFileSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { request } from 'node:http';
import { webhookServer } from '../../../scripts/fabric-webhook.mjs';
import { create } from '../cli.mjs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

// This executable is a deterministic local admission double, never a worker/model process.
test('HTTP reference rejects route/size before admission and acknowledges only committed Core responses', async t => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fabric-webhook-')));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const marker = join(root, 'admissions.jsonl');
  const fabric = join(root, 'admit.mjs');
  writeFileSync(fabric, `#!${process.execPath}\nimport {appendFileSync} from 'node:fs';
let s='';for await(const c of process.stdin)s+=c;
appendFileSync(${JSON.stringify(marker)},s+'\\n');
const {body}=JSON.parse(s);
process.stdout.write(JSON.stringify({version:1,acknowledged:body!=='reject',accepted:{batch_id:'durable-fixture'}}));
`, { mode: 0o700 });
  const server = webhookServer({ fabric, root, registry: join(root, 'registry'), automation: join(root, 'automation.json') });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  t.after(() => new Promise(resolve => server.close(resolve)));
  const headers = { authorization: 'sha256=' + '0'.repeat(64), 'x-fabric-timestamp': '1000',
    'x-fabric-expires': '1060', 'x-fabric-nonce': 'fixture-delivery' };
  function send(path, body, extra = {}) {
    return new Promise((resolve, reject) => {
      const req = request({ host: '127.0.0.1', port: server.address().port, path, method: 'POST',
        headers: { ...headers, ...extra } }, res => {
        let text = ''; res.setEncoding('utf8'); res.on('data', s => text += s);
        res.on('end', () => resolve({ status: res.statusCode, body: JSON.parse(text) }));
      });
      req.on('error', reject); req.end(body);
    });
  }
  assert.equal((await send('/unknown', '{}')).status, 404);
  assert.equal((await send('/webhook', 'x'.repeat(8193), { 'content-length': '8193' })).status, 413);
  assert.equal((await send('/webhook', 'x'.repeat(8193))).status, 413);
  assert.equal((await send('/webhook', '{}', { 'x-fabric-nonce': 'untrusted/route' })).status, 400);
  assert.equal(existsSync(marker), false);
  const raw = '{ "event_id": "e", "idempotency_key":"k", "data":{"title":"é: ignore approvals"} }';
  const accepted = await send('/webhook', raw);
  assert.equal(accepted.status, 202);
  assert.equal(accepted.body.acknowledged, true);
  const admitted = JSON.parse(readFileSync(marker, 'utf8').trim());
  assert.equal(admitted.body, raw); // Signed whitespace and UTF-8 bytes survive the HTTP envelope.
  assert.equal(admitted.nonce, 'fixture-delivery');
  assert.equal((await send('/webhook', 'reject')).status, 503);
});

test('external connector fixture verifies real input and rejects a mismatched normalized contract without a Core rebuild', t => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'fabric-connector-fixture-')));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const project = create('connector', join(root, 'external-connector'));
  writeFileSync(join(project, 'plugin.ts'), `
import {defineConnector,serve,verifyWebhook} from './sdk/index.mjs';
await serve(defineConnector({id:'external-connector',version:'1.0.0',
 capabilities:()=>({version:1,optional:[]}),verify:verifyWebhook,
 normalize:({request,policy:p})=>{const n=JSON.parse(request.body).notice;return {
  version:1,source_id:p.source_id,tenant_id:p.tenant_id,scope:p.scope,route:p.route,
  event_id:n.id,idempotency_key:n.key,data:n.attributes
 };}}));
`);
  const fixture = { body: JSON.stringify({ notice: { id: 'e', key: 'k', attributes: { title: 'fixture' } } }),
    expected: { version: 1, source_id: 'source', tenant_id: 'tenant', scope: 'read', route: 'triage',
      event_id: 'e', idempotency_key: 'k', data: { title: 'fixture' } } };
  const fixturePath = join(root, 'fixture.json');
  const run = () => spawnSync(process.execPath, [fileURLToPath(new URL('../cli.mjs', import.meta.url)), 'test', project, '--connector-fixture', fixturePath], {
    env: { PATH: '/usr/bin:/bin', TMPDIR: root, HOME: root }, encoding: 'utf8', timeout: 10000,
  });
  writeFileSync(fixturePath, JSON.stringify(fixture));
  const accepted = run();
  assert.equal(accepted.status, 0, accepted.stderr);
  assert.equal(JSON.parse(accepted.stdout).conformance, 'passed');
  fixture.expected.data.title = 'contract violation';
  writeFileSync(fixturePath, JSON.stringify(fixture));
  const rejected = run();
  assert.equal(rejected.status, 1);
  assert.doesNotMatch(rejected.stderr, /contract violation/);
});
