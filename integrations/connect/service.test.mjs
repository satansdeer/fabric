import assert from "node:assert/strict";
import { test } from "node:test";
import { createHash, createHmac } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHandler, PREFIX } from "./service.mjs";
import { FileStore } from "./test-server.mjs";

const DAY = 86400000;
const TOKEN = "fixture-worker-token-not-a-credential";
const ENV = {
  FABRIC_CONNECT_WEBHOOK_SECRET: "a1".repeat(32),
  FABRIC_CONNECT_WORKER_TOKEN_SHA256: createHash("sha256").update(TOKEN).digest("hex"),
  FABRIC_CONNECT_WORKER_ID: "fixture_worker_01",
};
const clone = value => structuredClone(value);
class MemoryStore {
  data = null;
  version = 0;
  writes = 0;
  failures = 0;
  async getWithMetadata(key, options) {
    assert.equal(key, "satansdeer/fabric/issues");
    assert.deepEqual(options, { type: "json", consistency: "strong" });
    return this.data && { data: clone(this.data), etag: String(this.version), metadata: {} };
  }
  async setJSON(key, data, options) {
    assert.equal(key, "satansdeer/fabric/issues");
    this.writes++;
    assert.equal(Object.keys(options).length, 1);
    assert.ok(options.onlyIfNew === true || typeof options.onlyIfMatch === "string");
    if (this.failures-- > 0 || (this.data ? options.onlyIfMatch !== String(this.version) : !options.onlyIfNew)) return { modified: false };
    this.data = clone(data);
    return { modified: true, etag: String(++this.version) };
  }
}
function fixture(store = new MemoryStore()) {
  let time = Date.parse("2026-11-22T12:00:00.000Z");
  const handler = createHandler({ store, env: ENV, now: () => time });
  async function send(path, { method = "GET", body, headers = {}, auth = true } = {}) {
    const response = await handler(new Request(`https://fixture.invalid${path}`, {
      method, headers: { ...(auth ? { authorization: `Bearer ${TOKEN}` } : {}), ...headers },
      ...(body !== undefined ? { body: typeof body === "string" ? body : JSON.stringify(body) } : {}),
    }));
    assert.equal(response.headers.get("cache-control"), "no-store");
    const data = await response.json();
    assert.ok(!JSON.stringify(data).includes(TOKEN));
    assert.ok(!JSON.stringify(data).includes(ENV.FABRIC_CONNECT_WEBHOOK_SECRET));
    return { code: response.status, data };
  }
  function payload(number = 1, overrides = {}) {
    return {
      action: "opened", repository: { full_name: "satansdeer/fabric" }, sender: { login: "satansdeer" },
      issue: { number, html_url: `https://github.com/satansdeer/fabric/issues/${number}`,
        user: { login: "satansdeer" }, state: "open", labels: [{ name: "ready" }],
        updated_at: new Date(time).toISOString(), title: "private issue title", body: "untrusted private issue body" },
      ...overrides,
    };
  }
  function webhook(body = payload(), headers = {}) {
    const raw = typeof body === "string" ? body : JSON.stringify(body);
    return send(`${PREFIX}/github`, { method: "POST", body: raw, auth: false, headers: {
      "x-github-event": "issues", "x-github-delivery": "12345678-1234-4234-8234-123456789abc",
      "x-hub-signature-256": `sha256=${createHmac("sha256", ENV.FABRIC_CONNECT_WEBHOOK_SECRET).update(raw).digest("hex")}`,
      ...headers,
    } });
  }
  const poll = (issue_number = 1, extra = {}) => send(`${PREFIX}/poll`, {
    method: "POST", body: { worker_id: ENV.FABRIC_CONNECT_WORKER_ID, issue_number, ...extra },
  });
  const ack = (event, status, extra = {}) => send(`${PREFIX}/events/${event.event_id}/ack`, {
    method: "POST", body: { worker_id: ENV.FABRIC_CONNECT_WORKER_ID, lease_id: event.lease_id, status, ...extra },
  });
  return { store, handler, send, payload, webhook, poll, ack,
    status: () => send(`${PREFIX}/status`), advance: delta => { time += delta; } };
}
const HANDLES = { batch_id: "batch-1", run_id: "run-1" };

// No network, credentials, models, or SDK installation are needed for these checks.
test("public health only; worker authentication and provisioned identity gate every mutation", async () => {
  const f = fixture();
  assert.deepEqual((await f.send("/health", { auth: false })).data, { service: "fabric-connect", schema_version: 1 });
  assert.equal((await f.send("/v1/other/poll", { method: "POST", auth: false })).code, 404);
  assert.equal((await f.send(`${PREFIX}/status`, { auth: false })).code, 401);
  assert.equal((await f.send(`${PREFIX}/poll`, { method: "POST", auth: false, body: {} })).code, 401);
  assert.equal((await f.send(`${PREFIX}/events/${"a".repeat(64)}/ack`, { method: "POST", auth: false, body: {} })).code, 401);
  assert.equal((await f.send(`${PREFIX}/status`, { headers: { authorization: "Bearer wrong" } })).code, 401);
  assert.equal((await f.poll(1, { worker_id: "unprovisioned_worker" })).code, 403);
  assert.equal(f.store.writes, 0);
});

test("raw HMAC and bounded bytes precede parsing; errors never echo inputs", async () => {
  const f = fixture();
  assert.equal((await f.webhook(f.payload(), { "x-hub-signature-256": "sha256=" + "0".repeat(64) })).code, 401);
  assert.equal((await f.webhook("not json")).code, 400);
  assert.equal((await f.webhook("x".repeat(128 * 1024 + 1))).code, 413);
  assert.equal((await f.webhook(f.payload(), { "content-length": String(128 * 1024 + 1) })).code, 413);
  assert.equal((await f.webhook(f.payload(), { "x-github-delivery": "unsigned-not-uuid" })).code, 400);
  assert.deepEqual((await f.webhook({}, { "x-github-event": "ping" })).data, { pong: true });
  assert.equal(f.store.writes, 0);
});

test("connector isolates exact repository, issue URL, author, sender, readiness and freshness", async () => {
  const f = fixture();
  for (const mutate of [
    p => { p.repository.full_name = "other/fabric"; }, p => { p.sender.login = "attacker"; },
    p => { p.issue.user.login = "attacker"; }, p => { p.issue.html_url += "/../2"; },
    p => { p.issue.number = 1.5; }, p => { p.issue.updated_at = "2026-11-21T11:59:59Z"; },
    p => { p.issue.updated_at = "2026-11-22T12:05:01Z"; },
  ]) {
    const p = f.payload(); mutate(p);
    assert.equal((await f.webhook(p)).code, 400);
  }
  for (const mutate of [
    p => { p.action = "closed"; }, p => { p.issue.state = "closed"; },
    p => { p.issue.labels = []; }, p => { p.issue.pull_request = {}; },
    p => { p.action = "labeled"; p.label = { name: "other" }; },
  ]) {
    const p = f.payload(); mutate(p);
    assert.deepEqual((await f.webhook(p)).data, { ignored: true });
  }
  assert.deepEqual((await f.webhook({}, { "x-github-event": "push" })).data, { ignored: true });
  assert.equal(f.store.writes, 0);
});

test("persist-before-ack, bounded CAS failures and duplicate header replay", async () => {
  const f = fixture();
  f.store.failures = 8;
  assert.equal((await f.webhook()).code, 503);
  assert.equal(f.store.writes, 8);
  assert.equal(f.store.data, null);
  const accepted = await f.webhook();
  assert.equal(accepted.code, 202);
  const duplicate = await f.webhook(f.payload(), { "x-github-delivery": "87654321-4321-4321-8321-cba987654321" });
  assert.equal(duplicate.code, 200);
  assert.equal(duplicate.data.event_id, accepted.data.event_id);
  assert.equal(duplicate.data.duplicate, true);
  assert.equal(f.store.data.events.length, 1);
  assert.equal(f.store.data.rate.length, 1);
  assert.ok(!JSON.stringify(f.store.data).includes("private issue"));
});

test("concurrent duplicate admission and concurrent polling have a single durable lease", async () => {
  const f = fixture();
  const accepted = await Promise.all([f.webhook(), f.webhook()]);
  assert.deepEqual(accepted.map(result => result.code).sort(), [200, 202]);
  assert.equal(f.store.data.events.length, 1);
  const leases = await Promise.all([f.poll(), f.poll()]);
  assert.ok(leases.every(result => result.code === 200));
  assert.equal(leases[0].data.event.lease_id, leases[1].data.event.lease_id);
  assert.equal(f.store.data.events[0].attempts, 1);
});

test("concurrent distinct events are not lost across CAS retry", async () => {
  const f = fixture();
  const results = await Promise.all([f.webhook(f.payload(1)), f.webhook(f.payload(2))]);
  assert.ok(results.every(result => result.code === 202));
  assert.deepEqual(f.store.data.events.map(event => event.issue_number).sort(), [1, 2]);
});

test("poll is issue-scoped; heartbeat and live lease renewal do not increase attempts", async () => {
  const f = fixture();
  await f.webhook(f.payload(2));
  assert.equal((await f.status()).data.worker.online, false);
  assert.equal((await f.poll(1)).data.event, null);
  const first = (await f.poll(2)).data.event;
  assert.equal(first.status, "delivering");
  f.advance(60000);
  const second = (await f.poll(2)).data.event;
  assert.equal(second.lease_id, first.lease_id);
  assert.equal(second.attempts, 1);
  assert.ok(second.lease_expires_at > first.lease_expires_at);
  f.advance(60001);
  assert.equal((await f.status()).data.worker.online, false);
});

test("expired leases back off, reject stale ack, and stop after five deliveries", async () => {
  const f = fixture();
  await f.webhook();
  let event = (await f.poll()).data.event;
  for (let attempt = 1; attempt <= 5; attempt++) {
    assert.equal(event.attempts, attempt);
    f.advance(120000);
    const result = await f.poll();
    assert.equal(result.data.event, null);
    assert.equal((await f.ack(event, "running", HANDLES)).code, 409);
    if (attempt < 5) {
      f.advance(1000 * 2 ** (attempt - 1));
      const next = (await f.poll()).data.event;
      assert.notEqual(next.lease_id, event.lease_id);
      event = next;
    }
  }
  const record = (await f.status()).data.events[0];
  assert.equal(record.status, "failed");
  assert.equal(record.reason, "delivery_exhausted");
});

test("admission is running, never completion; terminal transitions require immutable real handles", async () => {
  const f = fixture();
  await f.webhook();
  const event = (await f.poll()).data.event;
  assert.equal((await f.ack(event, "running")).code, 400);
  assert.equal((await f.ack(event, "running", { batch_id: ["batch"], run_id: "run" })).code, 400);
  assert.equal((await f.ack(event, "running", { batch_id: "../batch", run_id: "run" })).code, 400);
  assert.equal((await f.ack(event, "completed", HANDLES)).code, 409);
  assert.equal((await f.ack({ ...event, lease_id: "different" }, "running", HANDLES)).code, 409);
  assert.equal((await f.ack(event, "running", HANDLES)).data.event.status, "running");
  assert.equal((await f.ack(event, "running", HANDLES)).code, 200);
  f.advance(30 * DAY);
  const resumed = (await f.poll()).data.event;
  assert.equal(resumed.status, "running");
  assert.equal(resumed.lease_id, event.lease_id);
  assert.equal(resumed.run_id, HANDLES.run_id);
  assert.equal((await f.ack(event, "completed", { ...HANDLES, run_id: "different" })).code, 409);
  assert.equal((await f.ack(event, "completed", { ...HANDLES, artifact_manifest_id: "manifest-1" })).code, 200);
  assert.equal((await f.ack(event, "completed", { ...HANDLES, artifact_manifest_id: "manifest-1" })).code, 200);
  assert.equal((await f.ack(event, "failed", HANDLES)).code, 409);
  const status = (await f.status()).data;
  assert.equal(status.counts.completed, 1);
  assert.equal(status.counts.running, 0);
});

test("only explicit pre-admission rejection can fail an unadmitted delivery", async () => {
  const f = fixture();
  await f.webhook();
  const event = (await f.poll()).data.event;
  assert.equal((await f.ack(event, "failed")).code, 400);
  assert.equal((await f.ack(event, "failed", { reason: "network_error" })).code, 400);
  assert.equal((await f.ack(event, "failed", { reason: "issue_rejected", ...HANDLES })).code, 400);
  assert.equal((await f.ack(event, "failed", { reason: "approval_mismatch" })).code, 200);
  assert.equal((await f.ack(event, "failed", { reason: "approval_mismatch" })).code, 200);
  assert.equal((await f.ack(event, "failed", { reason: "issue_rejected" })).code, 409);
});

test("new events are atomically rate limited and duplicates do not consume the budget", async () => {
  const f = fixture();
  const first = f.payload(1);
  for (let number = 1; number <= 60; number++) assert.equal((await f.webhook(f.payload(number))).code, 202);
  assert.equal((await f.webhook(first)).code, 200);
  assert.equal((await f.webhook(f.payload(61))).code, 429);
  assert.equal(f.store.data.events.length, 60);
  f.advance(60000);
  assert.equal((await f.webhook(f.payload(61))).code, 202);
  for (let number = 62; number <= 64; number++) assert.equal((await f.webhook(f.payload(number))).code, 202);
  assert.equal((await f.webhook(f.payload(65))).code, 429);
  assert.equal(f.store.data.events.length, 64);
});

test("expiry preserves seven-day dedup tombstones, retained capacity never evicts history", async () => {
  const f = fixture();
  await f.webhook();
  f.advance(DAY);
  assert.equal((await f.status()).data.events[0].status, "expired");
  const tombstone = clone(f.store.data.events[0]);
  f.store.data.events = Array.from({ length: 256 }, (_, index) => ({ ...tombstone, event_id: index.toString(16).padStart(64, "0") }));
  assert.equal((await f.webhook(f.payload(2))).code, 429);
  assert.equal(f.store.data.events.length, 256);
  f.advance(7 * DAY - 1);
  assert.equal((await f.status()).data.events.length, 256);
  f.advance(1);
  assert.equal((await f.status()).data.events.length, 0);
  assert.equal((await f.webhook(f.payload(2))).code, 202);
});

test("corrupt durable state fails closed without replacement", async () => {
  const f = fixture();
  f.store.data = { schema_version: 99, events: [] };
  assert.equal((await f.webhook()).code, 503);
  assert.equal((await f.status()).code, 503);
  assert.equal(f.store.writes, 0);
});

test("file-backed fixture persists acknowledged events and running mappings across restart", async () => {
  const directory = await mkdtemp(join(tmpdir(), "fabric-connect-store-"));
  try {
    const path = join(directory, "route.json");
    const first = fixture(new FileStore(path));
    assert.equal((await first.webhook()).code, 202);
    const event = (await first.poll()).data.event;
    assert.equal((await first.ack(event, "running", HANDLES)).code, 200);
    const restarted = fixture(new FileStore(path));
    const resumed = (await restarted.poll()).data.event;
    assert.equal(resumed.status, "running");
    assert.equal(resumed.lease_id, event.lease_id);
    assert.equal(resumed.run_id, HANDLES.run_id);
    assert.equal((await restarted.webhook()).data.duplicate, true);
    const persisted = await readFile(path, "utf8");
    assert.ok(!persisted.includes(TOKEN));
    assert.ok(!persisted.includes("private issue"));
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test("configured worker rotation cannot silently reassign an existing running mapping", async () => {
  const f = fixture();
  await f.webhook();
  const event = (await f.poll()).data.event;
  await f.ack(event, "running", HANDLES);
  const handler = createHandler({ store: f.store, env: { ...ENV, FABRIC_CONNECT_WORKER_ID: "replacement_worker" } });
  const response = await handler(new Request(`https://fixture.invalid${PREFIX}/poll`, {
    method: "POST", headers: { authorization: `Bearer ${TOKEN}` },
    body: JSON.stringify({ worker_id: "replacement_worker", issue_number: 1 }),
  }));
  assert.equal(response.status, 503);
  assert.equal(f.store.data.events[0].run_id, HANDLES.run_id);
  assert.equal(f.store.data.worker_id, ENV.FABRIC_CONNECT_WORKER_ID);
});

test("late maintenance frees fully elapsed expiry tombstones without extending retention", async () => {
  const f = fixture();
  await f.webhook();
  f.advance(8 * DAY);
  assert.equal((await f.status()).data.events.length, 0);
  assert.equal((await f.webhook(f.payload(2))).code, 202);
});

test("actual failed runs preserve handles and terminal evidence on reconciliation", async () => {
  const f = fixture();
  await f.webhook();
  const event = (await f.poll()).data.event;
  await f.ack(event, "running", HANDLES);
  assert.equal((await f.ack(event, "failed", HANDLES)).data.event.status, "failed");
  assert.equal((await f.ack(event, "failed", HANDLES)).code, 200);
  assert.equal((await f.ack(event, "completed", HANDLES)).code, 409);
  assert.equal((await f.poll()).data.event, null);
  assert.equal((await f.status()).data.events[0].run_id, HANDLES.run_id);
});

test("all supported ready actions normalize without retaining issue text", async () => {
  const f = fixture();
  for (const action of ["opened", "edited", "reopened", "labeled"]) {
    const payload = f.payload(1, { action, label: { name: "ready" } });
    assert.equal((await f.webhook(payload)).code, 202);
  }
  assert.equal(f.store.data.events.length, 4);
  assert.ok(f.store.data.events.every(event => event.schema_version === 1 && event.source === "github" && event.repo === "satansdeer/fabric"));
  assert.ok(!JSON.stringify(f.store.data).includes("untrusted private issue body"));
});

test("competing terminal acknowledgements cannot overwrite the winning outcome", async () => {
  const f = fixture();
  await f.webhook();
  const event = (await f.poll()).data.event;
  await f.ack(event, "running", HANDLES);
  const results = await Promise.all([f.ack(event, "completed", HANDLES), f.ack(event, "failed", HANDLES)]);
  assert.deepEqual(results.map(result => result.code).sort(), [200, 409]);
  const accepted = results.find(result => result.code === 200).data.event.status;
  assert.equal((await f.status()).data.events[0].status, accepted);
});
