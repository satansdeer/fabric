import { lstatSync, readdirSync, realpathSync, existsSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { createHash } from 'node:crypto';
import { gzipSync, gunzipSync } from 'node:zlib';
import { readBounded, decode, metadata, safePath, fail, MAX_PACKAGE, MAX_FRAME } from './protocol.mjs';

export const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const MAX_ARCHIVE = MAX_PACKAGE + 1024 * 1024;
function validate(files) {
  if (!files.has('package.json') || files.size > 256) fail('invalid_package');
  let total = 0;
  for (const [name, bytes] of files) {
    if (!safePath(name) || name.split('/').some(p => p === 'node_modules' || p === '.git') || name.split('/').length > 17 || (total += bytes.length) > MAX_PACKAGE) fail('invalid_package');
  }
  if (files.get('package.json').length > MAX_FRAME) fail('invalid_package');
  const pkg = decode(files.get('package.json'));
  const info = metadata(pkg);
  if (!files.has(info.entry)) fail('missing_entry');
  if (['dependencies', 'optionalDependencies', 'bundledDependencies', 'bundleDependencies'].some(k => pkg[k] && Object.keys(pkg[k]).length)) fail('dependencies_unsupported: vendor reviewed source instead');
  return { files, info, hash: contentHash(files) };
}
export function contentHash(files) {
  const hash = createHash('sha256');
  for (const name of [...files.keys()].sort()) hash.update(`${name}\0${sha256(files.get(name))}\n`);
  return hash.digest('hex');
}
export function directory(path) {
  const root = resolve(path);
  if (realpathSync(root) !== root || !lstatSync(root).isDirectory()) fail('unsafe_package_path');
  const pkgBytes = readBounded(join(root, 'package.json'), MAX_FRAME);
  const pkg = decode(pkgBytes);
  metadata(pkg);
  if (!Array.isArray(pkg.files) || !pkg.files.length || pkg.files.length > 32) fail('package_files_required');
  const files = new Map([['package.json', pkgBytes]]);
  let total = pkgBytes.length;
  let entries = 0;
  function visit(name, depth = 0) {
    if (!safePath(name) || name.split('/').some(p => p === 'node_modules' || p === '.git') || depth > 16 || ++entries > 512) fail('unsafe_package_path');
    const path = join(root, name);
    if (realpathSync(path) !== path) fail('unsafe_package_path');
    const stat = lstatSync(path);
    if (stat.isDirectory()) {
      for (const child of readdirSync(path).sort()) visit(`${name}/${child}`, depth + 1);
    } else if (stat.isFile()) {
      if (files.has(name)) return;
      const bytes = readBounded(path, MAX_PACKAGE - total);
      total += bytes.length;
      files.set(name, bytes);
      if (files.size > 256) fail('package_too_large');
    } else fail('unsafe_package_path');
  }
  for (const name of pkg.files) visit(name);
  if (existsSync(join(root, 'README.md')) && !files.has('README.md')) visit('README.md');
  return validate(files);
}
function octal(bytes) {
  const text = bytes.toString('ascii').replace(/\0.*$/, '').trim();
  if (!/^[0-7]+$/.test(text)) fail('invalid_archive');
  return parseInt(text, 8);
}
export function archive(bytes) {
  if (bytes.length > MAX_ARCHIVE) fail('archive_too_large');
  let tar;
  try { tar = gunzipSync(bytes, { maxOutputLength: MAX_ARCHIVE }); }
  catch { fail('invalid_archive'); }
  const files = new Map();
  const names = new Set();
  let offset = 0;
  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every(b => b === 0)) {
      if (tar.length - offset < 1024 || !tar.subarray(offset).every(b => b === 0)) fail('invalid_archive');
      return validate(files);
    }
    let checksum = 0;
    for (let i = 0; i < 512; i++) checksum += i >= 148 && i < 156 ? 32 : header[i];
    if (checksum !== octal(header.subarray(148, 156))) fail('invalid_archive');
    const text = (start, end) => header.subarray(start, end).toString('utf8').replace(/\0.*$/, '');
    const prefix = text(345, 500);
    const full = (prefix ? `${prefix}/` : '') + text(0, 100);
    const name = full.slice(8).replace(/\/$/, '');
    const type = header[156];
    const size = octal(header.subarray(124, 136));
    if (!full.startsWith('package/') || !safePath(name) || names.has(name) || names.size >= 512 || ![0, 48, 53].includes(type)) fail('unsafe_archive_entry');
    names.add(name);
    offset += 512;
    if (size > MAX_PACKAGE || offset + Math.ceil(size / 512) * 512 > tar.length) fail('invalid_archive');
    if (type === 53) { if (size) fail('invalid_archive'); }
    else files.set(name, tar.subarray(offset, offset + size));
    offset += Math.ceil(size / 512) * 512;
  }
  fail('invalid_archive');
}
export function loadPackage(path) {
  return lstatSync(path).isDirectory() ? directory(path) : archive(readBounded(path, MAX_ARCHIVE));
}
export function pack(snapshot) {
  const blocks = [];
  for (const name of [...snapshot.files.keys()].sort()) {
    const full = `package/${name}`;
    if (Buffer.byteLength(full) > 100) fail('archive_path_too_long: keep packaged paths below 93 bytes');
    const bytes = snapshot.files.get(name);
    const h = Buffer.alloc(512);
    h.write(full, 0, 100);
    const field = (n, start, length) => h.write(n.toString(8).padStart(length - 1, '0') + '\0', start, length);
    field(0o644, 100, 8); field(0, 108, 8); field(0, 116, 8);
    field(bytes.length, 124, 12); field(0, 136, 12);
    h.fill(32, 148, 156); h[156] = 48;
    h.write('ustar\0', 257); h.write('00', 263);
    const sum = h.reduce((a, b) => a + b, 0);
    h.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 8);
    blocks.push(h, bytes, Buffer.alloc((512 - bytes.length % 512) % 512));
  }
  blocks.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(blocks), { level: 9 });
}
