import { createHash, randomUUID, timingSafeEqual } from "node:crypto";
import { githubEvent, validPolicy } from "./github.mjs";
import { genericEvent, validGenericPolicy } from "./generic.mjs";
const validRoutePolicy = policy => validPolicy(policy) || validGenericPolicy(policy);

export const PREFIX = "/v1/projects/satansdeer/fabric/automations/issues";
const BODY_LIMIT = 128 * 1024;
const LEGACY = { account: "satansdeer", project: "fabric", automation: "issues" };
const LEGACY_POLICY = { repo: "satansdeer/fabric", author: "satansdeer", sender: "satansdeer", label: "ready" };
const SEGMENT = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const segment = value => typeof value === "string" && SEGMENT.test(value);
const routeKey = identity => `${identity.account}/${identity.project}/${identity.automation}`;
const routePath = identity => `/v1/projects/${identity.account}/${identity.project}/automations/${identity.automation}`;
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
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

function initial(identity, principal, body) {
  return { schema_version: 2, identity, principal_id: principal.id, worker_id: body.worker_id,
    policy: validGenericPolicy(body.policy) ? {
      kind: "generic", source_id: body.policy.source_id, tenant_id: body.policy.tenant_id,
      scope: body.policy.scope, route: body.policy.route, max_payload_bytes: body.policy.max_payload_bytes,
    } : { repo: body.policy.repo, author: body.policy.author, sender: body.policy.sender, label: body.policy.label },
    webhook_secret: body.webhook_secret,
    events: [], rate: [], worker_last_seen_at: null };
}
function validState(state) {
  return object(state) && [1, 2].includes(state.schema_version) && WORKER_ID.test(state.worker_id || "") && Array.isArray(state.events) &&
    (state.schema_version === 1 || (object(state.identity) &&
      ["account", "project", "automation"].every(key => segment(state.identity[key])) &&
      safeID(state.principal_id) && validRoutePolicy(state.policy) && HEX.test(state.webhook_secret || "") &&
      (state.legacy_token_sha256 === undefined || HEX.test(state.legacy_token_sha256)))) &&
      (!validGenericPolicy(state.policy) || (state.policy.tenant_id === state.identity.account &&
        state.policy.scope === state.identity.project && state.policy.route === state.identity.automation &&
        (state.local_id === undefined || HEX.test(state.local_id)) &&
        (state.worker_offline === undefined || typeof state.worker_offline === "boolean"))) &&
    state.events.length <= 256 && Array.isArray(state.rate) && state.rate.length <= 60 &&
    state.rate.every(Number.isFinite) &&
    (state.worker_last_seen_at === null || Number.isFinite(Date.parse(state.worker_last_seen_at))) &&
    state.events.every(event => object(event) && HEX.test(event.event_id) &&
      STATES.includes(event.status) && Number.isFinite(Date.parse(event.received_at)) &&
      (ACTIVE.has(event.status) || Number.isFinite(Date.parse(event.terminal_at)))) &&
    // Reserve bounded identity metadata above the v1 cap; a full old inbox must still migrate.
    Buffer.byteLength(JSON.stringify(state)) <= 512 * 1024 + (state.schema_version === 2 ? 2048 : 0);
}

function migrate(state, identity, env) {
  if (state.schema_version === 2) {
    if (!same(state.identity, identity)) fail(503);
    return state;
  }
  if (!same(identity, LEGACY) || state.worker_id !== env.FABRIC_CONNECT_WORKER_ID ||
      !HEX.test(env.FABRIC_CONNECT_WORKER_TOKEN_SHA256 || "") ||
      !HEX.test(env.FABRIC_CONNECT_WEBHOOK_SECRET || "")) fail(503);
  // Same key, same URL, one CAS: events, leases, receipts and replay history stay in place.
  return { ...state, schema_version: 2, identity: LEGACY, principal_id: "satansdeer",
    policy: LEGACY_POLICY, webhook_secret: env.FABRIC_CONNECT_WEBHOOK_SECRET,
    legacy_token_sha256: env.FABRIC_CONNECT_WORKER_TOKEN_SHA256 };
}

function principals(env) {
  let values;
  try { values = JSON.parse(env.FABRIC_CONNECT_PRINCIPALS || "[]"); } catch { fail(503); }
  if (!Array.isArray(values) || values.length > 64) fail(503);
  const ids = new Map(), tokens = new Set();
  for (const principal of values) {
    if (!object(principal) || !safeID(principal.id) || !segment(principal.account) ||
        !HEX.test(principal.token_sha256 || "") ||
        (ids.has(principal.id) && ids.get(principal.id) !== principal.account) || tokens.has(principal.token_sha256) ||
        !Array.isArray(principal.grants) || principal.grants.length > 64) fail(503);
    ids.set(principal.id, principal.account); tokens.add(principal.token_sha256);
    for (const grant of principal.grants) {
      if (!object(grant) || !(grant.project === null || segment(grant.project)) ||
          !(grant.automation === null || segment(grant.automation)) ||
          (grant.project === null && grant.automation !== null) ||
          !Array.isArray(grant.permissions) || !grant.permissions.length ||
          !grant.permissions.every(p => ["claim", "read", "poll", "ack"].includes(p)) ||
          (grant.automation === null && grant.permissions.some(p => p !== "claim"))) fail(503);
    }
  }
  return values;
}

function authenticate(request, configured, state) {
  const bearer = /^Bearer ([^\s]{1,1024})$/.exec(request.headers.get("authorization") || "");
  if (!bearer) fail(401);
  const digest = createHash("sha256").update(bearer[1]).digest();
  const matches = hash => timingSafeEqual(digest, Buffer.from(hash, "hex"));
  const principal = configured.find(value => matches(value.token_sha256));
  if (principal) return principal;
  if (state?.legacy_token_sha256 && matches(state.legacy_token_sha256)) {
    return { id: state.principal_id, account: LEGACY.account,
      grants: [{ project: LEGACY.project, automation: LEGACY.automation, permissions: ["read", "poll", "ack"] }] };
  }
  fail(401);
}

function authorize(principal, identity, permission, state) {
  if (principal.account !== identity.account || (state && state.principal_id !== principal.id) ||
      !principal.grants.some(grant => grant.project === (identity.project ?? null) &&
        grant.automation === (identity.automation ?? null) && grant.permissions.includes(permission))) fail(403);
}

function maintain(state, time) {
  let changed = false;
  for (const event of state.events) {
    if ((event.status === "queued" || event.status === "delivering") &&
        !(event.source === "generic" && event.attempts > 0) &&
        time >= Date.parse(event.received_at) + DAY) {
      event.status = "expired";
      event.terminal_at = iso(Date.parse(event.received_at) + DAY);
      delete event.lease_id;
      delete event.lease_expires_at;
      changed = true;
    } else if (event.status === "delivering" && time >= Date.parse(event.lease_expires_at)) {
      event.status = event.source !== "generic" && event.attempts >= 5 ? "failed" : "queued";
      if (event.status === "failed") {
        event.terminal_at = iso(time);
        event.reason = "delivery_exhausted";
      } else event.next_attempt_at = iso(Date.parse(event.lease_expires_at) + Math.min(60000, 1000 * 2 ** Math.min(16, event.attempts - 1)));
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
  // Explicit allowlist: never expose internal acknowledgements or future private fields.
  const fields = Object.fromEntries(["schema_version", "source", "event_id", "repo", "issue_number", "action",
    "delivery_id", "status", "received_at", "attempts", "lease_id", "lease_expires_at",
    "next_attempt_at", "terminal_at", "reason", "batch_id", "run_id", "artifact_manifest_id", "trigger_json"]
    .filter(key => event[key] !== undefined).map(key => [key, event[key]]));
  if (event.trigger_json !== undefined) fields.trigger = JSON.parse(event.trigger_json);
  return fields;
}
function worker(state, id, time) {
  return { id, last_seen_at: state.worker_last_seen_at,
    online: state.worker_offline !== true && state.worker_last_seen_at !== null && time - Date.parse(state.worker_last_seen_at) <= 60000 };
}
function status(state, id, time, origin) {
  const counts = Object.fromEntries(STATES.map(name => [name, 0]));
  for (const event of state.events) counts[event.status]++;
  return { schema_version: 1, owner: state.identity.account, project: state.identity.project,
    automation: state.identity.automation, url: origin + routePath(state.identity), policy: state.policy,
    local_status: worker(state, id, time).online ? "local online" : "local offline",
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

// One existing Durable Object serializes bounded per-route CAS records. No hosted execution.
export function createHandler({ store, env, now = Date.now }) {
  const configured = principals(env);
  let window = 0, requests = 0;

  async function load(identity) {
    const stored = await store.getWithMetadata(routeKey(identity), { type: "json", consistency: "strong" });
    if (stored && (!stored.etag || !validState(stored.data))) fail(503);
    return stored;
  }

  function legacyInitial(identity) {
    if (!same(identity, LEGACY)) fail(404);
    if (!WORKER_ID.test(env.FABRIC_CONNECT_WORKER_ID || "")) fail(503);
    return migrate({ schema_version: 1, worker_id: env.FABRIC_CONNECT_WORKER_ID,
      events: [], rate: [], worker_last_seen_at: null }, identity, env);
  }

  async function transaction(identity, operation) {
    const key = routeKey(identity);
    for (let attempt = 0; attempt < 8; attempt++) {
      const stored = await load(identity);
      const state = stored ? migrate(stored.data, identity, env) : legacyInitial(identity);
      const time = Number(now());
      const migrating = !stored || stored.data.schema_version === 1;
      const changed = maintain(state, time) || migrating;
      const result = operation(state, time);
      if (!changed && !result.changed) return reply(result.body, result.status);
      if (!validState(state)) fail(429);
      if (migrating) {
        const principal = { id: state.principal_id };
        await owner({ account: identity.account }, principal, true);
        await owner({ account: identity.account, project: identity.project }, principal, true);
      }
      const written = await store.setJSON(key, state, stored ? { onlyIfMatch: stored.etag } : { onlyIfNew: true });
      if (written.modified) return reply(result.body, result.status);
    }
    fail(503);
  }

  async function owner(identity, principal, create) {
    const key = identity.project ? `project:${identity.account}/${identity.project}` : `account:${identity.account}`;
    // Reserve the deployed namespace even before its first post-upgrade request.
    const reserved = identity.account === LEGACY.account &&
      (!identity.project || identity.project === LEGACY.project);
    if (reserved && principal.id !== "satansdeer") fail(403);
    for (let attempt = 0; attempt < 8; attempt++) {
      const stored = await store.getWithMetadata(key, { type: "json", consistency: "strong" });
      if (stored) {
        if (!stored.etag || stored.data?.schema_version !== 1 || !safeID(stored.data.principal_id)) fail(503);
        if (stored.data.principal_id !== principal.id) fail(403);
        return;
      }
      if (!create) {
        if (reserved) return;
        fail(409);
      }
      const written = await store.setJSON(key, { schema_version: 1, principal_id: principal.id }, { onlyIfNew: true });
      if (written.modified) return;
    }
    fail(503);
  }

  return async request => {
    try {
      const url = new URL(request.url), path = url.pathname;
      if (url.protocol !== "https:" && !(url.protocol === "http:" &&
          ["127.0.0.1", "[::1]", "localhost"].includes(url.hostname))) fail(400);
      if (request.method === "GET" && path === "/health") return reply({ service: "fabric-connect", schema_version: 1 });
      // Bounded per-object request work, including invalid signatures and claims. Resets on cold start;
      // durable authenticated admission limits below remain authoritative across restart.
      const time = Number(now());
      if (time - window >= 60000) { window = time; requests = 0; }
      if (++requests > 600) fail(429);
      if (url.search) fail(400);
      const accountClaim = /^\/v1\/accounts\/([^/]+)\/claim$/.exec(path);
      const projectClaim = /^\/v1\/projects\/([^/]+)\/([^/]+)\/claim$/.exec(path);
      const route = /^\/v1\/projects\/([^/]+)\/([^/]+)\/automations\/([^/]+)\/(claim|github|webhook|heartbeat|status|poll|events\/([0-9a-f]{64})\/ack)$/.exec(path);
      const identity = accountClaim ? { account: accountClaim[1] } :
        projectClaim ? { account: projectClaim[1], project: projectClaim[2] } :
          route ? { account: route[1], project: route[2], automation: route[3] } : null;
      if (!identity || !Object.values(identity).every(segment)) fail(404);
      const claim = Boolean(accountClaim || projectClaim || route?.[4] === "claim");
      const ingress = ["github", "webhook"].includes(route?.[4]);
      const view = route?.[4] === "status";
      const poll = route?.[4] === "poll";
      const heartbeat = route?.[4] === "heartbeat";
      if (request.method !== (view ? "GET" : "POST")) fail(404);
      const stored = route ? await load(identity) : null;
      const state = stored ? migrate(stored.data, identity, env) :
        route && same(identity, LEGACY) ? legacyInitial(identity) : null;
      let principal;
      if (!ingress) {
        principal = authenticate(request, configured, state);
        authorize(principal, identity, claim ? "claim" : view ? "read" : poll || heartbeat ? "poll" : "ack", state);
      }
      if (claim) {
        const body = await readJSON(request);
        if (accountClaim) {
          if (Object.keys(body).length) fail(400);
          await owner(identity, principal, true);
        } else {
          await owner({ account: identity.account }, principal, false);
          if (projectClaim) {
            if (Object.keys(body).length) fail(400);
            await owner(identity, principal, true);
          } else {
            await owner({ account: identity.account, project: identity.project }, principal, false);
            if (Object.keys(body).sort().join(",") !== "policy,webhook_secret,worker_id" ||
                !WORKER_ID.test(body.worker_id || "") || !HEX.test(body.webhook_secret || "") ||
                !validRoutePolicy(body.policy)) fail(400);
            if (validGenericPolicy(body.policy) && (body.policy.tenant_id !== identity.account ||
                body.policy.scope !== identity.project || body.policy.route !== identity.automation)) fail(400);
            const desired = initial(identity, principal, body);
            let current = state;
            if (!current) {
              const written = await store.setJSON(routeKey(identity), desired, { onlyIfNew: true });
              if (written.modified) return reply({ identity, url: url.origin + routePath(identity) });
              const raced = await load(identity);
              if (!raced) fail(503);
              current = migrate(raced.data, identity, env);
              authorize(principal, identity, "claim", current);
            }
            if (current.worker_id !== desired.worker_id || !same(current.policy, desired.policy) ||
                current.webhook_secret !== desired.webhook_secret) fail(409);
            // Persist a pending v1 migration through the same CAS path.
            return await transaction(identity, () => ({ body: { identity, url: url.origin + routePath(identity) }, changed: false }));
          }
        }
        return reply({ identity, ...(route ? { url: url.origin + routePath(identity) } : {}) });
      }
      if (!state) fail(404);
      const id = state.worker_id;
      if (ingress) {
        const generic = validGenericPolicy(state.policy);
        if (route[4] !== (generic ? "webhook" : "github")) fail(404);
        const raw = await readBody(request);
        const parsed = generic ? genericEvent(request.headers, raw, state.webhook_secret, time, state.policy, routeKey(identity)) :
          githubEvent(request.headers, raw, state.webhook_secret, time, state.policy);
        if (parsed.error) fail(parsed.error);
        if (parsed.reply) return reply(parsed.reply);
        return await transaction(identity, (current, stamp) => {
          const existing = current.events.find(event => event.event_id === parsed.event.event_id);
          if (existing) {
            if (parsed.event.source === "generic" && existing.payload_sha256 !== parsed.event.payload_sha256) fail(409);
            return { body: { event_id: existing.event_id, status: existing.status, duplicate: true }, changed: false };
          }
          if (parsed.stale) fail(400);
          // A delivery ID may be retried with identical bytes, never repurposed for other content.
          if (parsed.event.delivery_id && current.events.some(event => event.delivery_id === parsed.event.delivery_id && event.event_id !== parsed.event.event_id)) fail(409);
          if (current.events.length >= 256 || current.events.filter(event => ACTIVE.has(event.status)).length >= 64 || current.rate.length >= 60) fail(429);
          current.events.push({ ...parsed.event, status: "queued", received_at: iso(stamp), attempts: 0 });
          current.rate.push(stamp);
          // Reserve future leases, handles, duplicate acknowledgement fields and installation metadata.
          if (Buffer.byteLength(JSON.stringify(current)) + 2048 * current.events.length + 256 > 512 * 1024 + 2048) fail(429);
          return { body: { event_id: parsed.event.event_id, status: "queued" }, status: 202, changed: true };
        });
      }
      if (view) return await transaction(identity, (current, stamp) => ({ body: status(current, id, stamp, url.origin), changed: false }));
      const body = await readJSON(request);
      if (body.worker_id !== id) fail(403);
      const generic = validGenericPolicy(state.policy);
      if (generic && (!HEX.test(body.local_id || "") ||
          (state.local_id && state.local_id !== body.local_id))) fail(403);
      if (heartbeat) {
        if (!generic || typeof body.online !== "boolean") fail(400);
        return await transaction(identity, (current, stamp) => {
          if (current.local_id && current.local_id !== body.local_id) fail(403);
          current.local_id = body.local_id;
          current.worker_last_seen_at = iso(stamp);
          current.worker_offline = !body.online;
          return { body: { worker: worker(current, id, stamp) }, changed: true };
        });
      }
      if (generic && !state.local_id) fail(409); // Bind the private installation before leasing.
      if (poll && (generic ? (body.event_id !== undefined && !HEX.test(body.event_id)) :
          (!Number.isSafeInteger(body.issue_number) || body.issue_number < 1 || body.issue_number > 2147483647))) fail(400);
      if (!poll && (typeof body.lease_id !== "string" || !SAFE_ID.test(body.lease_id) || !["running", "completed", "failed"].includes(body.status))) fail(400);
      return await transaction(identity, (current, stamp) => {
        if (generic && current.local_id !== body.local_id) fail(403);
        current.worker_offline = false;
        if (poll) {
          const matches = current.events.filter(event => generic ?
            (body.event_id === undefined || event.event_id === body.event_id) : event.issue_number === body.issue_number);
          let event = matches.find(event => event.status === "running") || matches.find(event => event.status === "delivering");
          if (generic && body.event_id && !event) {
            const retained = matches.find(event => !ACTIVE.has(event.status));
            if (retained) return { body: { event: publicEvent(retained), worker: worker(current, id, stamp) }, changed: false };
          }
          if (!event) {
            event = matches.find(event => event.status === "queued" && (!event.next_attempt_at || stamp >= Date.parse(event.next_attempt_at)));
            if (event) {
              event.status = "delivering";
              event.attempts++;
              event.lease_id = randomUUID();
              delete event.next_attempt_at;
            }
          }
          if (event?.status === "delivering") event.lease_expires_at = iso(stamp + LEASE);
          current.worker_last_seen_at = iso(stamp);
          return { body: { event: event ? publicEvent(event) : null, worker: worker(current, id, stamp) }, changed: true };
        }
        const event = current.events.find(event => event.event_id === route[5]);
        ackEvent(event, body, stamp);
        current.worker_last_seen_at = iso(stamp);
        return { body: { event: publicEvent(event), worker: worker(current, id, stamp) }, changed: true };
      });
    } catch (error) {
      const code = [400, 401, 403, 404, 409, 413, 429, 503].includes(error?.status) ? error.status : 503;
      return reply({ error: code === 503 ? "Service unavailable" : code === 404 ? "Not found" : "Request rejected" }, code);
    }
  };
}
