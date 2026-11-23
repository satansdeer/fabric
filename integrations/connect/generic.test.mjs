import test from "node:test";
import assert from "node:assert/strict";
import { createHash, createHmac } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createHandler } from "./service.mjs";
import { FileStore } from "./test-server.mjs";

const TOKEN = "generic-fixture-owner-not-a-credential-123456";
const READER = "generic-fixture-reader-not-a-credential-123456";
const SECRET = "ad".repeat(32);
const PREFIX = "/v1/projects/team/project/automations/jobs";
const POLICY = { kind: "generic", source_id: "webhook", tenant_id: "team", scope: "project", route: "jobs", max_payload_bytes: 8192 };
const LOCAL = "cb".repeat(32);
const HANDLES = { batch_id: "batch-protocol-fixture", run_id: "run-protocol-fixture" };

async function fixture() {
  const directory = await mkdtemp(join(tmpdir(), "fabric-generic-"));
  const path = join(directory, "state.json");
  const hash = token => createHash("sha256").update(token).digest("hex");
  const env = { FABRIC_CONNECT_PRINCIPALS: JSON.stringify([
    { id: "owner", account: "team", token_sha256: hash(TOKEN), grants: [
      { project: null, automation: null, permissions: ["claim"] },
      { project: "project", automation: null, permissions: ["claim"] },
      { project: "project", automation: "jobs", permissions: ["claim", "read", "poll", "ack"] },
    ] },
    { id: "owner", account: "team", token_sha256: hash(READER), grants: [
      { project: "project", automation: "jobs", permissions: ["read"] },
    ] },
  ]) };
  let time = Date.parse("2026-07-01T00:00:00Z");
  let handler;
  const restart = () => { handler = createHandler({ store: new FileStore(path), env, now: () => time }); };
  restart();
  async function send(path, body, token = TOKEN, headers = {}) {
    const response = await handler(new Request("https://fixture.invalid" + path, {
      method: body === undefined ? "GET" : "POST", headers: {
        ...(token === null ? {} : { authorization: "Bearer " + token }), ...headers,
      }, ...(body === undefined ? {} : { body: typeof body === "string" ? body : JSON.stringify(body) }),
    }));
    return { code: response.status, data: await response.json() };
  }
  assert.equal((await send("/v1/accounts/team/claim", {})).code, 200);
  assert.equal((await send("/v1/projects/team/project/claim", {})).code, 200);
  const claim = await send(PREFIX + "/claim", { worker_id: "fixture_worker", webhook_secret: SECRET, policy: POLICY });
  assert.equal(claim.code, 200);
  const body = (id = "one", data = {}) => JSON.stringify({ version: 1, event_id: id, timestamp: time / 1000, data });
  const webhook = raw => send(PREFIX + "/webhook", raw, null, {
    "x-fabric-signature-256": "sha256=" + createHmac("sha256", SECRET).update("team/project/jobs\n").update(raw).digest("hex"),
  });
  const local = (suffix, fields = {}, token = TOKEN) => send(PREFIX + suffix, { worker_id: "fixture_worker", local_id: LOCAL, ...fields }, token);
  return { send, local, body, webhook, restart, url: claim.data.url,
    advance: ms => { time += ms; }, close: () => rm(directory, { recursive: true, force: true }) };
}

test("generic durable offline delivery, installation binding and lost acknowledgements survive restart", async () => {
  const f = await fixture();
  try {
    const raw = f.body("one", { prompt: "do not execute", command: "/bin/false", model: "untrusted" });
    const queued = await f.webhook(raw);
    assert.equal(queued.code, 202);
    f.restart();
    let status = (await f.send(PREFIX + "/status")).data;
    assert.equal(status.url, f.url);
    assert.equal(status.local_status, "local offline");
    assert.equal(status.events[0].status, "queued");
    assert.equal((await f.local("/poll")).code, 409);
    assert.equal((await f.local("/heartbeat", { online: true }, READER)).code, 403);
    assert.equal((await f.local("/heartbeat", { online: true })).code, 200);
    assert.equal((await f.local("/poll", { local_id: "ef".repeat(32) })).code, 403);
    const first = (await f.local("/poll")).data.event;
    assert.equal(first.trigger.route, "jobs");
    assert.equal(first.trigger.tenant_id, "team");
    assert.deepEqual(first.trigger.data, JSON.parse(raw).data);
    assert.equal(first.trigger.idempotency_key, first.event_id);
    // An uncertain admission must not be expired or discarded at the old five-lease ceiling.
    for (let attempt = 0; attempt < 6; attempt++) {
      f.advance(3 * 86400000);
      const retry = (await f.local("/poll", { event_id: first.event_id })).data.event;
      assert.equal(retry.status, "delivering");
    }
    const leased = (await f.local("/poll")).data.event;
    const ack = (state, handles = HANDLES) => f.local("/events/" + first.event_id + "/ack", { ...handles, lease_id: leased.lease_id, status: state });
    assert.equal((await ack("running")).code, 200); // Discard the committed reply, then recreate the service.
    f.restart();
    assert.equal((await ack("running")).code, 200);
    const resumed = (await f.local("/poll")).data.event;
    assert.equal(resumed.batch_id, HANDLES.batch_id);
    assert.equal((await ack("completed", { ...HANDLES, run_id: "different" })).code, 409);
    assert.equal((await ack("completed", { ...HANDLES, artifact_manifest_id: "manifest-fixture" })).code, 200);
    f.restart();
    const terminal = (await f.local("/poll", { event_id: first.event_id })).data.event;
    assert.equal(terminal.artifact_manifest_id, "manifest-fixture");
    assert.equal((await f.webhook(raw)).data.duplicate, true);
    assert.equal((await f.local("/heartbeat", { online: false })).code, 200);
    status = (await f.send(PREFIX + "/status")).data;
    assert.equal(status.local_status, "local offline");
    assert.equal(status.url, f.url);
    assert.equal(status.events[0].status, "completed");
    assert.ok(!JSON.stringify(status).includes(SECRET));
    assert.ok(!JSON.stringify(status).includes(LOCAL));
  } finally { await f.close(); }
});

test("public URL, signed identity overrides, conflicting IDs and bounded payload/queue cannot bypass policy", async () => {
  const f = await fixture();
  try {
    assert.equal((await f.send(PREFIX + "/status", undefined, null)).code, 401);
    assert.equal((await f.send(PREFIX.replace("project/automations", "other/automations") + "/status")).code, 403);
    assert.equal((await f.send(PREFIX + "/webhook", f.body(), null)).code, 401);
    assert.equal((await f.webhook(JSON.stringify({ ...JSON.parse(f.body()), route: "other" }))).code, 400);
    assert.equal((await f.webhook(f.body("large", { content: "x".repeat(8192) }))).code, 413);
    assert.equal((await f.webhook('{"version":1,"event_id":"surrogate","timestamp":1782864000,"data":{"bad":"\\ud800"}}')).code, 400);
    const raw = f.body("one", { value: 1 });
    assert.equal((await f.webhook(raw)).code, 202);
    assert.equal((await f.webhook(f.body("one", { value: 2 }))).code, 409);
    assert.equal((await f.send(PREFIX + "/claim", {
      worker_id: "fixture_worker", webhook_secret: SECRET, policy: { ...POLICY, tenant_id: "other" },
    })).code, 400);
    for (let n = 2; n <= 60; n++) assert.equal((await f.webhook(f.body("event" + n))).code, 202);
    assert.equal((await f.webhook(f.body("event61"))).code, 429);
    assert.equal((await f.webhook(raw)).code, 200);
    f.advance(60000);
    for (let n = 61; n <= 64; n++) assert.equal((await f.webhook(f.body("event" + n))).code, 202);
    assert.equal((await f.webhook(f.body("event65"))).code, 429);
    const status = (await f.send(PREFIX + "/status")).data;
    assert.equal(status.counts.queued, 64);
    f.advance(86400000);
    assert.equal((await f.send(PREFIX + "/status")).data.counts.expired, 64);
  } finally { await f.close(); }
});

test("near-capacity ingress leaves space to bind, lease and retain maximum receipt handles", async () => {
  const f = await fixture();
  try {
    const admitted = [];
    for (let n = 0; n < 64; n++) {
      if (n === 50) f.advance(60000);
      const response = await f.webhook(f.body("capacity" + n, { content: "x".repeat(7400) }));
      if (response.code === 429) break;
      assert.equal(response.code, 202);
      admitted.push(response.data.event_id);
    }
    assert.ok(admitted.length > 1 && admitted.length < 64);
    assert.equal((await f.local("/heartbeat", { online: true })).code, 200);
    const handles = { batch_id: "b".repeat(200), run_id: "r".repeat(200) };
    for (const event_id of admitted) {
      const polled = await f.local("/poll", { event_id });
      assert.equal(polled.code, 200);
      const receipt = { ...handles, lease_id: polled.data.event.lease_id };
      assert.equal((await f.local("/events/" + event_id + "/ack", { ...receipt, status: "running" })).code, 200);
      assert.equal((await f.local("/events/" + event_id + "/ack", {
        ...receipt, status: "completed", artifact_manifest_id: "a".repeat(200),
      })).code, 200);
    }
    assert.equal((await f.send(PREFIX + "/status")).data.counts.completed, admitted.length);
  } finally { await f.close(); }
});
