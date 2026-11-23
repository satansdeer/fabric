import { createHmac, timingSafeEqual } from 'node:crypto';
import { constants, openSync, fstatSync, readSync, closeSync, realpathSync } from 'node:fs';
import { isAbsolute } from 'node:path';
import { exact, object, fail } from './protocol.mjs';

const id = value => typeof value === 'string' && /^[A-Za-z0-9_.-]{1,128}$/.test(value);
export function validConnectorInput(input) {
  if (!exact(input, ['version', 'request', 'policy', 'now']) || input.version !== 1 || !Number.isSafeInteger(input.now)) return false;
  const { request: r, policy: p, now } = input;
  return exact(r, ['body', 'timestamp', 'expires_at', 'nonce', 'authorization']) &&
    exact(p, ['source_id', 'tenant_id', 'scope', 'route', 'auth', 'secret_ref', 'max_payload_bytes', 'max_age_seconds']) &&
    [p.source_id, p.tenant_id, p.scope, p.route, r.nonce].every(id) &&
    ['hmac-sha256', 'bearer'].includes(p.auth) && typeof p.secret_ref === 'string' && isAbsolute(p.secret_ref) && p.secret_ref.length <= 4096 &&
    Number.isInteger(p.max_payload_bytes) && p.max_payload_bytes > 0 && p.max_payload_bytes <= 8192 &&
    Number.isInteger(p.max_age_seconds) && p.max_age_seconds > 0 && p.max_age_seconds <= 3600 &&
    typeof r.body === 'string' && Buffer.byteLength(r.body) <= p.max_payload_bytes &&
    typeof r.authorization === 'string' && r.authorization.length <= 512 &&
    Number.isSafeInteger(r.timestamp) && Number.isSafeInteger(r.expires_at) && r.timestamp >= 0 && r.timestamp <= now &&
    r.expires_at > now && r.expires_at > r.timestamp && r.expires_at - r.timestamp <= p.max_age_seconds && now - r.timestamp <= p.max_age_seconds;
}

/** The exact UTF-8 bytes to sign; includes trusted route/scope and the unmodified body string. */
export function webhookSigningBytes({ policy: p, request: r }) {
  return Buffer.from(JSON.stringify([1, p.source_id, p.tenant_id, p.scope, p.route, r.timestamp, r.expires_at, r.nonce, r.body]));
}
function secret(path) {
  if (realpathSync(path) !== path) fail('unsafe_secret_reference');
  const fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile() || stat.uid !== process.getuid() || stat.nlink !== 1 || (stat.mode & 0o077) || stat.size < 32 || stat.size > 256) fail('unsafe_secret_reference');
    const buffer = Buffer.alloc(257);
    const length = readSync(fd, buffer, 0, buffer.length, 0);
    if (length < 32 || length > 256) fail('unsafe_secret_reference');
    const bytes = buffer.subarray(0, length);
    return bytes;
  } finally { closeSync(fd); }
}
/** Scoped bearer references require TLS (or a trusted local transport). Never returns secret bytes. */
export function verifyWebhook(input) {
  if (!validConnectorInput(input)) return { verified: false };
  try {
    const key = secret(input.policy.secret_ref), auth = input.request.authorization;
    if (input.policy.auth === 'hmac-sha256') {
      if (!/^sha256=[a-f0-9]{64}$/.test(auth)) return { verified: false };
      const expected = createHmac('sha256', key).update(webhookSigningBytes(input)).digest();
      return { verified: timingSafeEqual(expected, Buffer.from(auth.slice(7), 'hex')) };
    }
    if (!auth.startsWith('Bearer ')) return { verified: false };
    const actual = Buffer.from(auth.slice(7));
    return { verified: actual.length === key.length && timingSafeEqual(actual, key) };
  } catch { return { verified: false }; }
}
export function validNormalizedTrigger(t) {
  return exact(t, ['version', 'source_id', 'event_id', 'tenant_id', 'idempotency_key', 'scope', 'route', 'data']) &&
    t.version === 1 && [t.source_id, t.event_id, t.tenant_id, t.idempotency_key, t.scope, t.route].every(id) &&
    object(t.data) && Buffer.byteLength(JSON.stringify(t.data)) <= 8192;
}
/** Reference JSON format: event_id, idempotency_key, data. No routing or instruction authority. */
export function normalizeWebhook({ request, policy }) {
  const body = JSON.parse(request.body);
  if (!exact(body, ['event_id', 'idempotency_key', 'data'])) fail('invalid_webhook');
  const trigger = { version: 1, source_id: policy.source_id, tenant_id: policy.tenant_id,
    scope: policy.scope, route: policy.route, ...body };
  if (!validNormalizedTrigger(trigger)) fail('invalid_webhook');
  return trigger;
}
