import { createHash, randomUUID, timingSafeEqual } from "node:crypto";
import { githubEvent } from "./github.mjs";

export const PREFIX = "/v1/projects/satansdeer/fabric/automations/issues";
const KEY = "satansdeer/fabric/issues";
const BODY_LIMIT = 128 * 1024;
const DAY = 86400000;
const LEASE = 120000;
const STATES = ["queued", "delivering", "running", "completed", "failed", "expired"];
const ACTIVE = new Set(["queued", "delivering", "running"]);
const SAFE_ID = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,199}$/;
const WORKER_ID = /^[A-Za-z0-9_-]{8,100}$/;
const HEX = /^[0-9a-f]{64}$/;
const iso = time => new Date(time).toISOString();
const object = value => value !== null && typeof value === "object" && !Array.isArray(value);
const safeID = value => typeof value === "string" && SAFE_ID.test(value);
const fail = status => { throw Object.assign(new Error("Request rejected"), { status }); };
const reply = (body, status = 200) => new Response(JSON.stringify(body), {
  status, headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" },
});

async function readBody(request) {
  const declared = request.headers.get("content-length");
  if (declared && (!/^\d+$/.test(declared) || Number(declared) > BODY_LIMIT)) fail(413);
  if (!request.body) return Buffer.alloc(0);
  const reader = request.body.getReader();
  const chunks = [];
  let length = 0;
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > BODY_LIMIT) { await reader.cancel(); fail(413); }
      chunks.push(value);
    }
  } finally { reader.releaseLock(); }
  return Buffer.concat(chunks, length);
}

async function readJSON(request) {
  const raw = await readBody(request);
  let body;
  try { body = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(raw)); }
  catch { fail(400); }
  if (!object(body)) fail(400);
  return body;
}

function initial(id) { return { schema_version: 1, worker_id: id, events: [], rate: [], worker_last_seen_at: null }; }
function validState(state) {
  return object(state) && state.schema_version === 1 && WORKER_ID.test(state.worker_id || "") && Array.isArray(state.events) &&
    state.events.length <= 256 && Array.isArray(state.rate) && state.rate.length <= 60 &&
    state.rate.every(Number.isFinite) &&
    (state.worker_last_seen_at === null || Number.isFinite(Date.parse(state.worker_last_seen_at))) &&
    state.events.every(event => object(event) && HEX.test(event.event_id) &&
      STATES.includes(event.status) && Number.isFinite(Date.parse(event.received_at)) &&
      (ACTIVE.has(event.status) || Number.isFinite(Date.parse(event.terminal_at)))) &&
    Buffer.byteLength(JSON.stringify(state)) <= 512 * 1024;
}

function maintain(state, time) {
  let changed = false;
  for (const event of state.events) {
    if ((event.status === "queued" || event.status === "delivering") &&
        time >= Date.parse(event.received_at) + DAY) {
      event.status = "expired";
      event.terminal_at = iso(Date.parse(event.received_at) + DAY);
      delete event.lease_id;
      delete event.lease_expires_at;
      changed = true;
    } else if (event.status === "delivering" && time >= Date.parse(event.lease_expires_at)) {
      event.status = event.attempts >= 5 ? "failed" : "queued";
      if (event.status === "failed") {
        event.terminal_at = iso(time);
        event.reason = "delivery_exhausted";
      } else event.next_attempt_at = iso(Date.parse(event.lease_expires_at) + Math.min(60000, 1000 * 2 ** (event.attempts - 1)));
      delete event.lease_id;
      delete event.lease_expires_at;
      changed = true;
    }
  }
  const retained = state.events.filter(event => ACTIVE.has(event.status) || time < Date.parse(event.terminal_at) + 7 * DAY);
  if (retained.length !== state.events.length) { state.events = retained; changed = true; }
  const rate = state.rate.filter(stamp => stamp > time - 60000);
  if (rate.length !== state.rate.length) { state.rate = rate; changed = true; }
  return changed;
}

function publicEvent(event) {
  const { last_ack, ...safe } = event;
  return safe;
}
function worker(state, id, time) {
  return { id, last_seen_at: state.worker_last_seen_at,
    online: state.worker_last_seen_at !== null && time - Date.parse(state.worker_last_seen_at) <= 60000 };
}
function status(state, id, time) {
  const counts = Object.fromEntries(STATES.map(name => [name, 0]));
  for (const event of state.events) counts[event.status]++;
  return { schema_version: 1, owner: "satansdeer", project: "fabric", automation: "issues",
    worker: worker(state, id, time), events: state.events.map(publicEvent), counts };
}

function ackEvent(event, body, time) {
  if (!event || event.lease_id !== body.lease_id) fail(409);
  const ack = { status: body.status, lease_id: body.lease_id };
  for (const key of ["batch_id", "run_id", "artifact_manifest_id", "reason"]) {
    if (body[key] !== undefined) ack[key] = body[key];
  }
  if (JSON.stringify(event.last_ack) === JSON.stringify(ack)) return false;
  if (!["delivering", "running"].includes(event.status)) fail(409);
  if (event.status === "delivering" && body.status === "failed") {
    if (!["approval_mismatch", "issue_rejected"].includes(body.reason) ||
        body.batch_id !== undefined || body.run_id !== undefined || body.artifact_manifest_id !== undefined) fail(400);
    event.reason = body.reason;
  } else {
    if (!safeID(body.batch_id) || !safeID(body.run_id) ||
        body.reason !== undefined ||
        (body.artifact_manifest_id !== undefined && !safeID(body.artifact_manifest_id))) fail(400);
    if (event.status === "delivering") {
      if (body.status !== "running") fail(409);
    } else if (event.status === "running") {
      if (!["completed", "failed"].includes(body.status) ||
          body.batch_id !== event.batch_id || body.run_id !== event.run_id ||
          (event.artifact_manifest_id !== undefined && body.artifact_manifest_id !== event.artifact_manifest_id)) fail(409);
    } else fail(409);
    event.batch_id = body.batch_id;
    event.run_id = body.run_id;
    if (body.artifact_manifest_id !== undefined) event.artifact_manifest_id = body.artifact_manifest_id;
  }
  event.status = body.status;
  event.last_ack = ack;
  if (body.status !== "running") event.terminal_at = iso(time);
  delete event.lease_expires_at;
  return true;
}

// ponytail: one bounded CAS route blob; move to a transactional DB for higher throughput/multiple workers.
export function createHandler({ store, env, now = Date.now }) {
  async function transaction(operation) {
    for (let attempt = 0; attempt < 8; attempt++) {
      const stored = await store.getWithMetadata(KEY, { type: "json", consistency: "strong" });
      if (stored && (!stored.etag || !validState(stored.data) || stored.data.worker_id !== env.FABRIC_CONNECT_WORKER_ID)) fail(503);
      const state = stored ? stored.data : initial(env.FABRIC_CONNECT_WORKER_ID);
      const time = Number(now());
      const changed = maintain(state, time);
      const result = operation(state, time);
      if (!changed && !result.changed) return reply(result.body, result.status);
      const written = await store.setJSON(KEY, state, stored ? { onlyIfMatch: stored.etag } : { onlyIfNew: true });
      if (written.modified) return reply(result.body, result.status);
    }
    fail(503);
  }

  return async request => {
    try {
      const path = new URL(request.url).pathname;
      if (request.method === "GET" && path === "/health") return reply({ service: "fabric-connect", schema_version: 1 });
      const ingress = request.method === "POST" && path === `${PREFIX}/github`;
      const view = request.method === "GET" && path === `${PREFIX}/status`;
      const poll = request.method === "POST" && path === `${PREFIX}/poll`;
      const ack = request.method === "POST" && path.match(new RegExp(`^${PREFIX}/events/([0-9a-f]{64})/ack$`));
      if (!ingress && !view && !poll && !ack) return reply({ error: "Not found" }, 404);
      const id = env.FABRIC_CONNECT_WORKER_ID;
      if (!WORKER_ID.test(id || "") || !HEX.test(env.FABRIC_CONNECT_WORKER_TOKEN_SHA256 || "") ||
          !/^[0-9a-f]{64}$/.test(env.FABRIC_CONNECT_WEBHOOK_SECRET || "")) fail(503);
      if (ingress) {
        const parsed = githubEvent(request.headers, await readBody(request), env.FABRIC_CONNECT_WEBHOOK_SECRET, Number(now()));
        if (parsed.error) fail(parsed.error);
        if (parsed.reply) return reply(parsed.reply);
        return await transaction((state, time) => {
          const existing = state.events.find(event => event.event_id === parsed.event.event_id);
          if (existing) return { body: { event_id: existing.event_id, status: existing.status, duplicate: true }, changed: false };
          if (state.events.length >= 256 || state.events.filter(event => ACTIVE.has(event.status)).length >= 64 || state.rate.length >= 60) fail(429);
          state.events.push({ ...parsed.event, status: "queued", received_at: iso(time), attempts: 0 });
          state.rate.push(time);
          return { body: { event_id: parsed.event.event_id, status: "queued" }, status: 202, changed: true };
        });
      }
      const authorization = request.headers.get("authorization") || "";
      const bearer = /^Bearer ([^\s]{1,1024})$/.exec(authorization);
      const supplied = createHash("sha256").update(bearer?.[1] || "").digest();
      if (!bearer || !timingSafeEqual(supplied, Buffer.from(env.FABRIC_CONNECT_WORKER_TOKEN_SHA256, "hex"))) fail(401);
      if (view) return await transaction((state, time) => ({ body: status(state, id, time), changed: false }));
      const body = await readJSON(request);
      if (body.worker_id !== id) fail(403);
      if (poll && (!Number.isSafeInteger(body.issue_number) || body.issue_number < 1)) fail(400);
      if (ack && (typeof body.lease_id !== "string" || !SAFE_ID.test(body.lease_id) || !["running", "completed", "failed"].includes(body.status))) fail(400);
      return await transaction((state, time) => {
        if (poll) {
          const matches = state.events.filter(event => event.issue_number === body.issue_number);
          let event = matches.find(event => event.status === "running") || matches.find(event => event.status === "delivering");
          if (!event) {
            event = matches.find(event => event.status === "queued" && (!event.next_attempt_at || time >= Date.parse(event.next_attempt_at)));
            if (event) {
              event.status = "delivering";
              event.attempts++;
              event.lease_id = randomUUID();
              delete event.next_attempt_at;
            }
          }
          if (event?.status === "delivering") event.lease_expires_at = iso(time + LEASE);
          state.worker_last_seen_at = iso(time);
          return { body: { event: event ? publicEvent(event) : null, worker: worker(state, id, time) }, changed: true };
        }
        const event = state.events.find(event => event.event_id === ack[1]);
        ackEvent(event, body, time);
        state.worker_last_seen_at = iso(time);
        return { body: { event: publicEvent(event), worker: worker(state, id, time) }, changed: true };
      });
    } catch (error) {
      const code = [400, 401, 403, 409, 413, 429, 503].includes(error?.status) ? error.status : 503;
      return reply({ error: code === 503 ? "Service unavailable" : "Request rejected" }, code);
    }
  };
}
