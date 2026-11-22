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
    assert.ok(!serialized.includes(ENV.FABRIC_CONNECT_WEBHOOK_SECRET));
    if (!path.startsWith("/__fixture/")) assert.equal(response.headers.get("cache-control"), "no-store");
    return { code: response.status, data };
  };
  const payload = number => JSON.stringify({
    action: "opened", repository: { full_name: "satansdeer/fabric" }, sender: { login: "satansdeer" },
    issue: { number, html_url: `https://github.com/satansdeer/fabric/issues/${number}`,
      user: { login: "satansdeer" }, state: "open", labels: [{ name: "ready" }],
      updated_at: new Date(time).toISOString(), title: "private issue title", body: "untrusted private issue body" },
  });
  const webhook = raw => send(`${PREFIX}/github`, { method: "POST", body: raw, auth: false, headers: {
    "x-github-event": "issues", "x-github-delivery": "12345678-1234-4234-8234-123456789abc",
    "x-hub-signature-256": `sha256=${createHmac("sha256", ENV.FABRIC_CONNECT_WEBHOOK_SECRET).update(raw).digest("hex")}`,
  } });
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
