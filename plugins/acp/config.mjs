import { constants, openSync, closeSync, fstatSync, readSync, realpathSync, statSync, lstatSync } from 'node:fs';
import { isAbsolute, relative } from 'node:path';
import { createHash } from 'node:crypto';

export const VERSION = 'fabric-acp-bridge/0.1.0';
export const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
export const fail = code => { throw Object.assign(new Error(code), { code }); };
export const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
export const fields = (value, names) => object(value) && Object.keys(value).every(key => names.includes(key));
const literal = value => typeof value === 'string' && value.length <= 4096 && !/[\0-\x1f\x7f]/.test(value);
const MAX_PIN_BYTES = 512 * 1024 * 1024;
export function absolute(path) {
  if (!literal(path) || !isAbsolute(path) || realpathSync(path) !== path) fail('acp_unsafe_path');
  return path;
}
export function fileHash(path) {
  absolute(path);
  const fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const before = fstatSync(fd);
    if (!before.isFile() || before.size > MAX_PIN_BYTES) fail('acp_unsafe_file');
    const hash = createHash('sha256'), buffer = Buffer.allocUnsafe(65_536);
    let count = 0, n;
    while ((n = readSync(fd, buffer))) {
      count += n;
      if (count > MAX_PIN_BYTES) fail('acp_unsafe_file');
      hash.update(buffer.subarray(0, n));
    }
    const after = fstatSync(fd);
    if (count !== before.size || before.size !== after.size || before.mtimeMs !== after.mtimeMs || before.ctimeMs !== after.ctimeMs) fail('acp_changed_file');
    return hash.digest('hex');
  } finally { closeSync(fd); }
}
function pin(value, executable = false) {
  if (!fields(value, ['path', 'sha256']) || typeof value.sha256 !== 'string' || !/^[a-f0-9]{64}$/.test(value.sha256)) fail('acp_invalid_config');
  absolute(value.path);
  if (executable && !(statSync(value.path).mode & 0o111)) fail('acp_unsafe_executable');
  if (fileHash(value.path) !== value.sha256) fail('acp_launch_changed');
}
export function permissionDirectory(path, workspace) {
  absolute(path);
  const stat = lstatSync(path);
  if (!stat.isDirectory() || stat.uid !== process.getuid() || (stat.mode & 0o077) ||
      Buffer.byteLength(path) + 33 > 103) fail('acp_unsafe_permission_directory');
  if (workspace) {
    const child = relative(workspace, path);
    if (!child || (!child.startsWith('../') && child !== '..' && !isAbsolute(child))) fail('acp_unsafe_permission_directory');
  }
  return { directory: path, dev: stat.dev, ino: stat.ino };
}
export function validate(config) {
  if (!fields(config, ['executable', 'args', 'nativeHome', 'cwd', 'pins', 'requestTimeoutMs', 'runtime', 'permission']) ||
      !Array.isArray(config.args) || config.args.length > 120 || !config.args.every(literal) ||
      config.cwd !== 'fabric-workspace' || !Array.isArray(config.pins) || config.pins.length > 16 ||
      !Number.isSafeInteger(config.requestTimeoutMs) || config.requestTimeoutMs < 1 || config.requestTimeoutMs > 300_000) fail('acp_invalid_config');
  absolute(config.nativeHome);
  if (!statSync(config.nativeHome).isDirectory()) fail('acp_unsafe_path');
  if (config.permission !== undefined) {
    const p = config.permission;
    if (!fields(p, ['directory', 'dev', 'ino'])) fail('acp_invalid_config');
    const actual = permissionDirectory(p.directory);
    if (p.dev !== actual.dev || p.ino !== actual.ino) fail('acp_unsafe_permission_directory');
  }
  pin(config.executable, true);
  pin(config.runtime, true);
  for (const item of config.pins) pin(item);
  // Script entrypoints must be literal pinned argv to an approved absolute
  // runtime, not env/shell shebangs that resolve another executable through PATH.
  const fd = openSync(config.executable.path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const prefix = Buffer.allocUnsafe(2);
    if (readSync(fd, prefix) === 2 && prefix[0] === 35 && prefix[1] === 33) fail('acp_unsafe_executable');
  } finally { closeSync(fd); }
  if (config.executable.path === config.runtime.path && !config.pins.some(item => item.path === config.args[0])) fail('acp_invalid_config');
  return config;
}
