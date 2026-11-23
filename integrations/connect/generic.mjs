// Reference generic connector: signed data only; routing authority comes from the claim.
import { createHash, createHmac, timingSafeEqual } from "node:crypto";

const ID = /^[A-Za-z0-9_.-]{1,128}$/;
export function validGenericPolicy(policy) {
  return policy && Object.keys(policy).sort().join(",") === "kind,max_payload_bytes,route,scope,source_id,tenant_id" &&
    policy.kind === "generic" && ["source_id", "tenant_id", "scope", "route"].every(key =>
      typeof policy[key] === "string" && ID.test(policy[key])) &&
    Number.isSafeInteger(policy.max_payload_bytes) && policy.max_payload_bytes > 0 && policy.max_payload_bytes <= 8192;
}

function validData(value, depth = 0) {
  if (depth > 64) return false;
  if (typeof value === "string") {
    for (const character of value) {
      const code = character.codePointAt(0);
      if (code >= 0xd800 && code <= 0xdfff) return false;
    }
  } else if (typeof value === "number") return Number.isFinite(value);
  else if (value !== null && typeof value === "object") {
    for (const [key, child] of Object.entries(value)) {
      if (!validData(key, depth + 1) || !validData(child, depth + 1)) return false;
    }
  }
  return true;
}

export function genericEvent(headers, raw, secret, now, policy, namespace) {
  const signature = headers.get("x-fabric-signature-256") || "";
  const expected = createHmac("sha256", secret).update(namespace + "\n").update(raw).digest();
  if (!/^sha256=[0-9a-f]{64}$/.test(signature) ||
      !timingSafeEqual(expected, Buffer.from(signature.slice(7), "hex"))) return { error: 401 };
  if (raw.length > policy.max_payload_bytes) return { error: 413 };
  let body;
  try { body = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(raw)); }
  catch { return { error: 400 }; }
  if (!body || Object.keys(body).sort().join(",") !== "data,event_id,timestamp,version" || body.version !== 1 ||
      typeof body.event_id !== "string" || !ID.test(body.event_id) || !Number.isSafeInteger(body.timestamp) ||
      !body.data || typeof body.data !== "object" || Array.isArray(body.data) || !validData(body.data)) return { error: 400 };
  // A producer ID names one immutable payload, including its timestamp. Re-sign identical bytes on retry.
  const event_id = createHash("sha256").update(body.event_id).digest("hex");
  const trigger = { version: 1, source_id: policy.source_id, tenant_id: policy.tenant_id,
    scope: policy.scope, route: policy.route, event_id, idempotency_key: event_id, data: body.data };
  const trigger_json = JSON.stringify(trigger);
  if (Buffer.byteLength(trigger_json, "utf8") > 8192) return { error: 413 };
  return { stale: body.timestamp * 1000 < now - 86400000 || body.timestamp * 1000 > now + 300000,
    event: { schema_version: 1, source: "generic", event_id,
      payload_sha256: createHash("sha256").update(raw).digest("hex"), trigger_json } };
}
