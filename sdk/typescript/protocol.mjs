import { constants, openSync, fstatSync, readSync, closeSync } from 'node:fs';

export const SDK_VERSION = '0.1.0';
export const PROTOCOL_VERSION = 1;
export const MAX_FRAME = 65_536;
export const MAX_PACKAGE = 32 * 1024 * 1024;
export const kinds = Object.freeze({ harness: 'harness.run', target: 'target.plan', connector: 'connector.evidence' });
export const fail = (code) => { throw new Error(code); };
export const object = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);
export const exact = (v, keys) => object(v) && Object.keys(v).length === keys.length && keys.every(k => Object.hasOwn(v, k));
export const validId = (v) => typeof v === 'string' && v.length > 0 && v.length <= 128 && !/[^a-z0-9-]/.test(v) && !['fake', 'fake-a', 'fake-b', 'omp', 'codex'].includes(v);
export function validVersion(v) {
  if (typeof v !== 'string' || v.length > 64) return false;
  const m = /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?$/.exec(v);
  return !!m && m[0] === v && [m[4], m[5]].every((part, i) => part === undefined || part.split('.').every(s => s && (i === 1 || !/^0[0-9]+$/.test(s))));
}
export function identity(v) {
  if (!object(v) || !validId(v.id) || !validVersion(v.version) || !Object.hasOwn(kinds, v.kind)) fail('invalid_identity');
  return { id: v.id, version: v.version, kind: v.kind };
}
export function metadata(pkg) {
  if (!object(pkg) || !validVersion(pkg.version) || !exact(pkg.fabricPlugin, ['id', 'kind', 'entry', 'sdk', 'protocol'])) fail('invalid_package');
  const p = pkg.fabricPlugin;
  if (p.sdk !== SDK_VERSION) fail('sdk_mismatch: use SDK 0.1.0; regenerate or update the plugin');
  if (p.protocol !== PROTOCOL_VERSION) fail('protocol_mismatch: Fabric supports plugin protocol 1');
  if (!safePath(p.entry) || !/\.(?:ts|mjs|js)$/.test(p.entry)) fail('invalid_entry');
  return { ...identity({ ...p, version: pkg.version }), entry: p.entry };
}
export function safePath(p) {
  return typeof p === 'string' && p.length <= 240 && p.split('/').every(s => s.length > 0 && !/[^A-Za-z0-9_@.-]/.test(s) && s !== '.' && s !== '..');
}
export function readBounded(path, max) {
  let fd;
  try {
    fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    const stat = fstatSync(fd);
    if (!stat.isFile() || stat.size > max) fail('invalid_file');
    // A bounded buffer also bounds a file growing after fstat.
    const bytes = Buffer.alloc(Math.min(stat.size + 1, max + 1));
    let n = 0;
    while (n < bytes.length) {
      const got = readSync(fd, bytes, n, bytes.length - n, null);
      if (!got) break;
      n += got;
    }
    if (n > max || n > stat.size) fail('invalid_file');
    return bytes.subarray(0, n);
  } finally { if (fd !== undefined) closeSync(fd); }
}
export function decode(bytes) {
  try { return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes)); }
  catch { fail('malformed_json'); }
}
export function frame(value) {
  const bytes = Buffer.from(JSON.stringify(value) + '\n');
  if (bytes.length > MAX_FRAME) fail('frame_too_large');
  return bytes;
}
export async function* frames(stream) {
  let pending = Buffer.alloc(0);
  for await (const chunk of stream) {
    let start = 0;
    for (let i = 0; i < chunk.length; i++) {
      if (chunk[i] !== 10) continue;
      if (pending.length + i - start + 1 > MAX_FRAME) fail('frame_too_large');
      const bytes = pending.length ? Buffer.concat([pending, chunk.subarray(start, i)]) : chunk.subarray(start, i);
      yield decode(bytes);
      pending = Buffer.alloc(0);
      start = i + 1;
    }
    if (pending.length + chunk.length - start >= MAX_FRAME) fail('frame_too_large');
    if (start < chunk.length) pending = Buffer.concat([pending, chunk.subarray(start)]);
  }
  if (pending.length) fail('truncated_frame');
}
export function resultFor(kind, value) {
  const key = kind === 'target' ? 'placement' : kind === 'connector' ? 'available' : 'ready';
  if (!exact(value, [key]) || (kind === 'target' ? value.placement !== 'local' : typeof value[key] !== 'boolean')) fail('invalid_result');
  return value;
}
