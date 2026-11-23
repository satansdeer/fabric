import assert from "node:assert/strict";
import { test } from "node:test";
import { createHash, createHmac } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { Miniflare } from "miniflare";
import { PREFIX } from "../service.mjs";

const ROOT = dirname(fileURLToPath(import.meta.url));
const TOKEN = "fixture-worker-token-not-a-credential";
const ENV = {
  FABRIC_CONNECT_WEBHOOK_SECRET: "a1".repeat(32),
  FABRIC_CONNECT_WORKER_TOKEN_SHA256: createHash("sha256").update(TOKEN).digest("hex"),
  FABRIC_CONNECT_WORKER_ID: "fixture_worker_01",
};
const START = Date.parse("2026-11-22T12:00:00.000Z");

// These hooks exist only in the in-memory test bundle, never in worker.mjs.
// The inherited adapter and its actual SQLite storage are unchanged; only time is controlled.
const FIXTURE = `
import worker, { ConnectInbox as Inbox } from "./worker.mjs";
import { createHandler } from "../service.mjs";
export class ConnectInbox extends Inbox {
  constructor(ctx, env) {
    super(ctx, env);
    this.time = ${START};
    this.handle = createHandler({ store: this.store, env, now: () => this.time });
  }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === "/__fixture/time") {
      this.time = (await request.json()).now;
      return Response.json({});
    }
    if (path === "/__fixture/seed") {
      const { key, data } = await request.json();
      return Response.json(this.store.setJSON(key, data, { onlyIfNew: true }));
    }
    if (path === "/__fixture/cas") {
      const { data, condition } = await request.json();
      return Response.json(this.store.setJSON("cas-probe", data, condition));
    }
    if (path === "/__fixture/read") {
      return Response.json(this.store.getWithMetadata(
        new URL(request.url).searchParams.get("key"), { type: "json", consistency: "strong" },
      ));
    }
    return super.fetch(request);
  }
}
export default worker;
`;

test("real SQLite inbox: auth, signed admission, CAS/leases, replay and large-state restart", { timeout: 120000 }, async () => {
  // Consume the deployment's binding/migration/compatibility settings rather than a second convention.
  const config = JSON.parse((await readFile(join(ROOT, "wrangler.jsonc"), "utf8")).replace(/^\s*\/\/.*$/gm, ""));
  assert.equal(config.workers_dev, true);
  assert.equal(config.main, "worker.mjs");
  const binding = config.durable_objects.bindings[0];
  assert.deepEqual(binding, { name: "CONNECT_INBOX", class_name: "ConnectInbox" });
  assert.deepEqual(config.migrations, [{ tag: "v1", new_sqlite_classes: [binding.class_name] }]);
  const bundled = await build({
    stdin: { contents: FIXTURE, resolveDir: ROOT, sourcefile: "fixture.mjs" },
    bundle: true, write: false, format: "esm", platform: "neutral", target: "es2022",
    external: ["node:*", "cloudflare:*"],
  });
  const disk = await mkdtemp(join(tmpdir(), "fabric-connect-cloudflare-"));
  const options = {
    modules: true, script: bundled.outputFiles[0].text, scriptPath: join(ROOT, "fixture.mjs"),
    compatibilityDate: config.compatibility_date, compatibilityFlags: config.compatibility_flags,
    durableObjects: { [binding.name]: { className: binding.class_name, useSQLite: true } },
    durableObjectsPersist: disk, bindings: ENV,
  };
  let runtime;
  let time = START;
  const send = async (path, { method = "GET", body, auth = true, headers = {} } = {}) => {
    const response = await runtime.dispatchFetch(`https://fixture.invalid${path}`, {
      method, headers: { ...(auth ? { authorization: `Bearer ${TOKEN}` } : {}), ...headers },
      ...(body !== undefined ? { body: typeof body === "string" ? body : JSON.stringify(body) } : {}),
    });
    const data = await response.json();
    const serialized = JSON.stringify(data);
    assert.ok(!serialized.includes(TOKEN));
    if (!path.startsWith("/__fixture/")) {
      assert.ok(!serialized.includes(ENV.FABRIC_CONNECT_WEBHOOK_SECRET));
      assert.equal(response.headers.get("cache-control"), "no-store");
    }
    return { code: response.status, data };
  };
  const payload = number => JSON.stringify({
    action: "opened", repository: { full_name: "satansdeer/fabric" }, sender: { login: "satansdeer" },
    issue: { number, html_url: `https://github.com/satansdeer/fabric/issues/${number}`,
      user: { login: "satansdeer" }, state: "open", labels: [{ name: "ready" }],
      updated_at: new Date(time).toISOString(), title: "private issue title", body: "untrusted private issue body" },
  });
  const webhook = raw => {
    const digest = createHash("sha256").update(raw).digest("hex");
    const delivery = `${digest.slice(0, 8)}-${digest.slice(8, 12)}-${digest.slice(12, 16)}-${digest.slice(16, 20)}-${digest.slice(20, 32)}`;
    return send(`${PREFIX}/github`, { method: "POST", body: raw, auth: false, headers: {
      "x-github-event": "issues", "x-github-delivery": delivery,
      "x-hub-signature-256": `sha256=${createHmac("sha256", ENV.FABRIC_CONNECT_WEBHOOK_SECRET).update(raw).digest("hex")}`,
    } });
  };
  const poll = issue_number => send(`${PREFIX}/poll`, {
    method: "POST", body: { worker_id: ENV.FABRIC_CONNECT_WORKER_ID, issue_number },
  });
  const handles = {
    batch_id: `batch-${"b".repeat(194)}`, run_id: `run-${"r".repeat(196)}`,
    artifact_manifest_id: `artifact-${"a".repeat(191)}`,
  };
  const ack = (event, status) => send(`${PREFIX}/events/${event.event_id}/ack`, {
    method: "POST", body: { worker_id: ENV.FABRIC_CONNECT_WORKER_ID, lease_id: event.lease_id, status, ...handles },
  });
  const read = key => send(`/__fixture/read?key=${encodeURIComponent(key)}`);
  const cas = (data, condition) => send("/__fixture/cas", { method: "POST", body: { data, condition } });
  const state = () => send(`${PREFIX}/status`);
  const finish = async number => {
    assert.equal((await webhook(payload(number))).code, 202);
    const leased = await poll(number);
    assert.equal(leased.code, 200);
    assert.equal((await ack(leased.data.event, "running")).code, 200);
    assert.equal((await ack(leased.data.event, "completed")).code, 200);
  };
  try {
    runtime = new Miniflare(options);
    assert.equal((await send("/health", { auth: false })).code, 200);
    assert.equal((await send(`${PREFIX}/status`, { auth: false })).code, 401);
    assert.equal((await send(`${PREFIX}/poll`, { method: "POST", body: {}, auth: false })).code, 401);
    assert.equal((await send(`${PREFIX}/github`, { method: "POST", body: payload(1), auth: false })).code, 401);
    assert.equal((await send(`${PREFIX}/github`, {
      method: "POST", auth: false, body: "x".repeat(128 * 1024 + 1),
    })).code, 413);

    // Actual competing SQLite conditional writes, not a storage or SQL mock.
    const creates = await Promise.all([cas({ winner: 1 }, { onlyIfNew: true }), cas({ winner: 2 }, { onlyIfNew: true })]);
    assert.deepEqual(creates.map(result => result.data.modified).sort(), [false, true]);
    const original = (await read("cas-probe")).data;
    const updates = await Promise.all([cas({ winner: 3 }, { onlyIfMatch: original.etag }), cas({ winner: 4 }, { onlyIfMatch: original.etag })]);
    assert.deepEqual(updates.map(result => result.data.modified).sort(), [false, true]);
    const winner = (await read("cas-probe")).data;
    assert.notEqual(winner.etag, original.etag);
    assert.equal(winner.etag, updates.find(result => result.data.modified).data.etag);
    assert.equal((await cas({ winner: 5 }, { onlyIfMatch: original.etag })).data.modified, false);

    const raw = payload(1);
    const duplicates = await Promise.all([webhook(raw), webhook(raw)]);
    assert.deepEqual(duplicates.map(result => result.code).sort(), [200, 202]);
    const distinct = await Promise.all([webhook(payload(2)), webhook(payload(3))]);
    assert.ok(distinct.every(result => result.code === 202));
    assert.deepEqual((await state()).data.events.map(event => event.issue_number).sort(), [1, 2, 3]);
    const polls = await Promise.all([poll(1), poll(1)]);
    assert.ok(polls.every(result => result.code === 200));
    const event = polls[0].data.event;
    assert.equal(event.lease_id, polls[1].data.event.lease_id);
    assert.equal(event.attempts, 1);
    assert.equal((await ack(event, "running")).code, 200);
    assert.equal((await ack(event, "completed")).code, 200);
    assert.equal((await ack(event, "completed")).code, 200);
    assert.equal((await webhook(raw)).data.duplicate, true);
    assert.equal((await poll(1)).data.event, null);

    const competing = (await poll(2)).data.event;
    assert.equal((await ack(competing, "running")).code, 200);
    const terminal = await Promise.all([ack(competing, "completed"), ack(competing, "failed")]);
    assert.deepEqual(terminal.map(result => result.code).sort(), [200, 409]);
    const outcome = terminal.find(result => result.code === 200).data.event.status;
    assert.equal((await state()).data.events.find(item => item.event_id === competing.event_id).status, outcome);
    const running = (await poll(3)).data.event;
    assert.equal((await ack(running, "running")).code, 200);

    // Grow only through signed ingress and real lifecycle acknowledgements. Advance the
    // fixture clock once to honor the 60/minute admission limit without a minute-long sleep.
    for (let number = 4; number <= 100; number++) {
      if (number === 50) {
        time += 61000;
        await send("/__fixture/time", { method: "POST", body: { now: time } });
      }
      await finish(number);
    }
    const persisted = (await read("satansdeer/fabric/issues")).data;
    const bytes = Buffer.byteLength(JSON.stringify(persisted.data));
    assert.ok(bytes > 128 * 1024 && bytes <= 512 * 1024, `expected bounded large state, got ${bytes} bytes`);
    assert.equal(persisted.data.events.length, 100);
    assert.ok(!JSON.stringify(persisted.data).includes("private issue"));

    await runtime.dispose();
    runtime = new Miniflare(options);
    await send("/__fixture/time", { method: "POST", body: { now: time } });
    assert.deepEqual((await read("satansdeer/fabric/issues")).data, persisted);
    assert.deepEqual((await read("cas-probe")).data, winner);
    const restored = await state();
    assert.equal(restored.code, 200);
    assert.equal(restored.data.events.length, 100);
    assert.equal(restored.data.events.find(item => item.event_id === event.event_id).status, "completed");
    assert.equal((await ack(event, "completed")).code, 200);
    assert.equal((await webhook(raw)).data.duplicate, true);
    const restoredRunning = await poll(3);
    assert.equal(restoredRunning.data.event.status, "running");
    assert.equal(restoredRunning.data.event.lease_id, running.lease_id);
    assert.equal(restoredRunning.data.event.run_id, handles.run_id);
    assert.equal((await ack(running, "completed")).code, 200);
    assert.equal((await ack(running, "completed")).code, 200);
    assert.equal((await send(`${PREFIX}/status`, { auth: false })).code, 401);
  } finally {
    await runtime?.dispose();
    await rm(disk, { recursive: true, force: true });
  }
});

test("claimed SQLite routes: scoped ownership, migration, offline retry/expiry and disk restart", { timeout: 120000 }, async () => {
  const config = JSON.parse((await readFile(join(ROOT, "wrangler.jsonc"), "utf8")).replace(/^\s*\/\/.*$/gm, ""));
  const bundled = await build({
    stdin: { contents: FIXTURE, resolveDir: ROOT, sourcefile: "fixture.mjs" },
    bundle: true, write: false, format: "esm", platform: "neutral", target: "es2022",
    external: ["node:*", "cloudflare:*"],
  });
  const disk = await mkdtemp(join(tmpdir(), "fabric-connect-claims-"));
  const hash = value => createHash("sha256").update(value).digest("hex");
  const alice = "fixture-alice-token-not-a-credential";
  const bob = "fixture-bob-token-not-a-credential";
  const intruder = "fixture-intruder-token-not-a-credential";
  const reader = "fixture-reader-token-not-a-credential";
  const policy = { repo: "team/project", author: "trusted", sender: "trusted", label: "ready" };
  const secret = "b2".repeat(32), otherSecret = "c3".repeat(32), siblingSecret = "d4".repeat(32);
  const base = "/v1/projects/alice/project/automations/triage";
  const other = "/v1/projects/bob/project/automations/triage";
  const workerID = "fixture_claimed_worker";
  const grants = [
    { project: null, automation: null, permissions: ["claim"] },
    { project: "project", automation: null, permissions: ["claim"] },
    { project: "project", automation: "triage", permissions: ["claim", "read", "poll", "ack"] },
    { project: "project", automation: "other", permissions: ["claim", "read", "poll", "ack"] },
  ];
  const provisioned = [
    { id: "alice", account: "alice", token_sha256: hash(alice), grants },
    { id: "bob", account: "bob", token_sha256: hash(bob), grants },
    { id: "intruder", account: "alice", token_sha256: hash(intruder), grants },
    { id: "alice", account: "alice", token_sha256: hash(reader),
      grants: [{ project: "project", automation: "triage", permissions: ["read"] }] },
  ];
  const options = {
    modules: true, script: bundled.outputFiles[0].text, scriptPath: join(ROOT, "fixture.mjs"),
    compatibilityDate: config.compatibility_date, compatibilityFlags: config.compatibility_flags,
    durableObjects: { CONNECT_INBOX: { className: "ConnectInbox", useSQLite: true } },
    durableObjectsPersist: disk,
    bindings: { ...ENV, FABRIC_CONNECT_PRINCIPALS: JSON.stringify(provisioned) },
  };
  let runtime, time = START;
  const send = async (path, { token = alice, body, headers = {} } = {}) => {
    const response = await runtime.dispatchFetch(`https://fixture.invalid${path}`, {
      method: body === undefined ? "GET" : "POST",
      headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...headers },
      ...(body === undefined ? {} : { body: typeof body === "string" ? body : JSON.stringify(body) }),
    });
    const data = await response.json();
    if (!path.startsWith("/__fixture/")) {
      const text = JSON.stringify(data);
      for (const privateValue of [alice, bob, intruder, reader, secret, otherSecret, TOKEN, ENV.FABRIC_CONNECT_WEBHOOK_SECRET, "private issue body"]) {
        assert.ok(!text.includes(privateValue));
      }
    }
    return { code: response.status, data };
  };
  const advance = async delta => {
    time += delta;
    await send("/__fixture/time", { body: { now: time } });
  };
  const payload = (number, repo = policy.repo, author = policy.author) => JSON.stringify({
    action: "opened", repository: { full_name: repo }, sender: { login: author },
    issue: { number, html_url: `https://github.com/${repo}/issues/${number}`, user: { login: author },
      state: "open", labels: [{ name: "ready" }], updated_at: new Date(time).toISOString(),
      body: "private issue body" },
  });
  const webhook = (path, raw, key = secret, delivery = null) => {
    const digest = hash(raw);
    return send(path + "/github", { token: null, body: raw, headers: {
      "x-github-event": "issues",
      "x-github-delivery": delivery || `${digest.slice(0, 8)}-${digest.slice(8, 12)}-${digest.slice(12, 16)}-${digest.slice(16, 20)}-${digest.slice(20, 32)}`,
      "x-hub-signature-256": `sha256=${createHmac("sha256", key).update(raw).digest("hex")}`,
    } });
  };
  const claim = (path, token, key) => send(path + "/claim", {
    token, body: { worker_id: workerID, webhook_secret: key, policy },
  });
  const poll = (path, number, token = alice, id = workerID) => send(path + "/poll", {
    token, body: { worker_id: id, issue_number: number },
  });
  const handles = { batch_id: "batch-real-local-handle", run_id: "run-real-local-handle" };
  const ack = (path, event, status, token = alice, id = workerID) => send(path + `/events/${event.event_id}/ack`, {
    token, body: { worker_id: id, lease_id: event.lease_id, status, ...handles },
  });
  const read = key => send(`/__fixture/read?key=${encodeURIComponent(key)}`);
  try {
    runtime = new Miniflare(options);
    assert.equal((await claim(base, alice, secret)).code, 409); // Parents must be claimed first.
    assert.equal((await send("/v1/accounts/bob/claim", { body: {} })).code, 403);
    for (const [account, token] of [["alice", alice], ["bob", bob]]) {
      assert.equal((await send(`/v1/accounts/${account}/claim`, { token, body: {} })).code, 200);
      assert.equal((await send(`/v1/projects/${account}/project/claim`, { token, body: {} })).code, 200);
    }
    assert.equal((await send("/v1/accounts/alice/claim", { token: intruder, body: {} })).code, 403);
    assert.equal((await send("/v1/projects/alice/project/claim", { token: intruder, body: {} })).code, 403);
    assert.equal((await claim(base, intruder, secret)).code, 403);
    const claims = await Promise.all([claim(base, alice, secret), claim(base, alice, secret)]);
    assert.deepEqual(claims.map(result => result.code), [200, 200]);
    const claimed = claims[0];
    assert.equal(claimed.data.url, `https://fixture.invalid${base}`);
    assert.equal((await claim(base, alice, secret)).code, 200);
    assert.equal((await claim(base, alice, otherSecret)).code, 409);
    assert.equal((await claim(other, bob, otherSecret)).code, 200);
    assert.equal((await claim(base, bob, otherSecret)).code, 403);
    const sibling = base.replace("/triage", "/other");
    assert.equal((await claim(sibling, alice, siblingSecret)).code, 200);
    assert.equal((await send(sibling + "/status", { token: reader })).code, 403);
    assert.equal((await poll(sibling, 1, reader)).code, 403);
    for (const token of [bob, intruder]) {
      assert.equal((await send(base + "/status", { token })).code, 403);
      assert.equal((await poll(base, 1, token)).code, 403);
    }
    assert.equal((await send(base + "/status", { token: reader })).code, 200);
    assert.equal((await poll(base, 1, reader)).code, 403);
    assert.equal((await claim(base, reader, secret)).code, 403);
    assert.equal((await send(base + "/status", { token: null })).code, 401);
    assert.equal((await send(base + "/github", { token: null, body: payload(1) })).code, 401);
    assert.equal((await webhook(base, payload(1), otherSecret)).code, 401);
    assert.equal((await webhook(base, "{")).code, 400);
    assert.equal((await webhook(base, "x".repeat(128 * 1024 + 1))).code, 413);
    assert.equal((await webhook(base, payload(1, "other/repo"))).code, 400);
    const raw = payload(1);
    assert.equal((await webhook(base, raw)).code, 202);
    assert.equal((await webhook(base, raw)).data.duplicate, true);
    const delivery = (await send(base + "/status")).data.events[0].delivery_id;
    assert.equal((await webhook(base, payload(2), secret, delivery)).code, 409);
    assert.equal((await webhook(other, raw, otherSecret)).code, 202);
    assert.equal((await send(base + "/status")).data.worker.online, false);
    const beforeOffline = (await send(base + "/status")).data.url;

    // Stop the actual Workers runtime and reopen its on-disk SQLite state, not an in-memory clone.
    await runtime.dispose();
    runtime = new Miniflare(options);
    assert.equal((await send(base + "/status")).data.url, beforeOffline);
    assert.equal((await send(base + "/status")).data.events[0].status, "queued");
    assert.equal((await webhook(base, raw)).data.duplicate, true);
    const event = (await poll(base, 1)).data.event;
    assert.equal(event.status, "delivering");
    assert.equal((await ack(base, event, "completed")).code, 409);
    for (const token of [bob, intruder, reader]) assert.equal((await ack(base, event, "running", token)).code, 403);
    assert.equal((await ack(sibling, event, "running", reader)).code, 403);
    assert.equal((await ack(other, event, "running", bob)).code, 409); // Same event hash, different route lease.
    assert.equal((await ack(base, event, "running")).data.event.status, "running");
    await advance(86400000);
    assert.equal((await poll(base, 1)).data.event.status, "running"); // Running is never reassigned on disconnect.
    assert.equal((await webhook(base, raw)).data.duplicate, true); // Retained replay survives provider freshness window.
    assert.equal((await send(other + "/status", { token: bob })).data.events[0].status, "expired");
    assert.equal((await ack(base, event, "completed")).code, 200);
    assert.equal((await ack(base, event, "completed")).code, 200);

    assert.equal((await webhook(base, payload(2))).code, 202);
    let lease = (await poll(base, 2)).data.event;
    for (let attempt = 1; attempt <= 5; attempt++) {
      assert.equal(lease.attempts, attempt);
      await advance(120000);
      assert.equal((await poll(base, 2)).data.event, null);
      assert.equal((await ack(base, lease, "running")).code, 409);
      if (attempt < 5) {
        await advance(1000 * 2 ** (attempt - 1));
        const next = (await poll(base, 2)).data.event;
        assert.notEqual(next.lease_id, lease.lease_id);
        lease = next;
      }
    }
    const exhausted = (await send(base + "/status")).data.events.find(item => item.issue_number === 2);
    assert.equal(exhausted.status, "failed");
    assert.equal(exhausted.reason, "delivery_exhausted");

    // Seed the deployed v1 record, including a queued receipt and running acknowledgement.
    const legacyRaw = payload(9, "satansdeer/fabric", "satansdeer");
    const legacyQueued = {
      schema_version: 1, source: "github", event_id: hash(legacyRaw), repo: "satansdeer/fabric",
      issue_number: 9, action: "opened", delivery_id: "12345678-1234-4234-8234-123456789abc",
      status: "queued", received_at: new Date(time).toISOString(), attempts: 0,
    };
    const legacyRunning = { ...legacyQueued, event_id: "d".repeat(64), issue_number: 10,
      status: "running", attempts: 1, lease_id: "legacy-running-lease", ...handles,
      last_ack: { status: "running", lease_id: "legacy-running-lease", ...handles } };
    const legacy = { schema_version: 1, worker_id: ENV.FABRIC_CONNECT_WORKER_ID,
      events: [legacyQueued, legacyRunning], rate: [time], worker_last_seen_at: null };
    assert.equal((await send("/__fixture/seed", { body: { key: "satansdeer/fabric/issues", data: legacy } })).data.modified, true);
    assert.equal((await send(PREFIX + "/status", { token: null })).code, 401);
    assert.equal((await read("satansdeer/fabric/issues")).data.data.schema_version, 1);
    const migrated = await send(PREFIX + "/status", { token: TOKEN });
    assert.equal(migrated.code, 200);
    assert.equal(migrated.data.url, `https://fixture.invalid${PREFIX}`);
    const migrationState = (await read("satansdeer/fabric/issues")).data;
    assert.equal(migrationState.data.schema_version, 2);
    assert.deepEqual(migrationState.data.events, legacy.events);
    assert.equal((await webhook(PREFIX, legacyRaw, ENV.FABRIC_CONNECT_WEBHOOK_SECRET)).data.duplicate, true);
    assert.equal((await poll(PREFIX, 10, TOKEN, ENV.FABRIC_CONNECT_WORKER_ID)).data.event.lease_id, legacyRunning.lease_id);
    const persisted = (await read("satansdeer/fabric/issues")).data;
    assert.equal((await read("account:satansdeer")).data.data.principal_id, "satansdeer");
    assert.equal((await read("project:satansdeer/fabric")).data.data.principal_id, "satansdeer");
    assert.equal((await send(PREFIX + "/status", { token: alice })).code, 403);
    const claimedSnapshot = (await read("alice/project/triage")).data;

    await runtime.dispose();
    // The migrated credential remains valid without the old environment bootstrap bindings.
    runtime = new Miniflare({ ...options, bindings: { FABRIC_CONNECT_PRINCIPALS: JSON.stringify(provisioned) } });
    await send("/__fixture/time", { body: { now: time } });
    assert.deepEqual((await read("satansdeer/fabric/issues")).data, persisted);
    assert.deepEqual((await read("alice/project/triage")).data, claimedSnapshot);
    assert.equal((await send(PREFIX + "/status", { token: TOKEN })).code, 200);
    const queued = (await poll(PREFIX, 9, TOKEN, ENV.FABRIC_CONNECT_WORKER_ID)).data.event;
    assert.equal(queued.status, "delivering");
    assert.equal((await ack(PREFIX, queued, "running", TOKEN, ENV.FABRIC_CONNECT_WORKER_ID)).code, 200);
    assert.equal((await ack(PREFIX, legacyRunning, "completed", TOKEN, ENV.FABRIC_CONNECT_WORKER_ID)).code, 200);
    assert.equal((await ack(base, event, "completed")).code, 200);
    assert.equal((await send(base + "/status")).data.url, beforeOffline);
  } finally {
    await runtime?.dispose();
    await rm(disk, { recursive: true, force: true });
  }
});
