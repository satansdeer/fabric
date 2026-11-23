#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { constants as osConstants } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const usage = `Usage: fabric-native install|up|exec --version 0.1.2 --platform macos-arm64 --prefix /canonical/new/directory [--archive /absolute/local.tar.gz] [--trust-plugins] [-- NATIVE_ARGS...]
install verifies and extracts only; --trust-plugins explicitly registers executable plugins.
up explicitly executes the released 'fabric init' onboarding command, not a daemon.
exec explicitly executes the exact native arguments after --.
Only macOS arm64 is released. Node >=22.18 is needed only for this optional launcher.
Offline: supply the pinned release with --archive. Existing prefixes are never reused.
This package/namespace is private, unpublished and unowned; no npm installation is claimed.`;

function fail(message) {
  console.error(`fabric-native: ${message}\n${usage}`);
  process.exit(1);
}

const [major, minor] = process.versions.node.split('.').map(Number);
if (major < 22 || (major === 22 && minor < 18)) fail('Node >=22.18 is required; use /usr/bin/ruby distribution/install.rb without Node.');
const args = process.argv.slice(2);
if (args.length === 1 && ['--help', '-h'].includes(args[0])) {
  console.log(usage);
  process.exit(0);
}
const command = args.shift();
if (!['install', 'up', 'exec'].includes(command)) fail('choose install, up or exec.');
const options = new Map();
let nativeArgs = [];
let separator = false;
while (args.length) {
  const option = args.shift();
  if (option === '--') {
    separator = true;
    nativeArgs = args;
    break;
  }
  if (!['--version', '--platform', '--prefix', '--archive', '--trust-plugins'].includes(option)) fail(`unknown installer argument: ${option}; put native arguments after --.`);
  if (options.has(option)) fail(`duplicate argument: ${option}`);
  if (option === '--trust-plugins') options.set(option, true);
  else {
    const value = args.shift();
    if (!value || value.startsWith('--')) fail(`missing value for ${option}`);
    options.set(option, value);
  }
}
for (const option of ['--version', '--platform', '--prefix']) if (!options.has(option)) fail(`${option} is required explicitly.`);
if (options.get('--version') !== '0.1.2') fail('only reviewed version 0.1.2 is pinned; future releases require reviewed distribution pin updates.');
if (options.get('--platform') !== 'macos-arm64' || process.platform !== 'darwin' || process.arch !== 'arm64') fail(`unsupported platform: requested ${options.get('--platform')}, host ${process.platform}-${process.arch}; only native macOS arm64 is released.`);
if (command === 'install' && separator) fail('install does not execute native arguments.');
if (command === 'exec' && (!separator || !nativeArgs.length)) fail('exec requires native arguments after --.');
const installerArgs = [join(dirname(fileURLToPath(import.meta.url)), 'install.rb')];
for (const [option, value] of options) {
  installerArgs.push(option);
  if (value !== true) installerArgs.push(value);
}
const installed = spawnSync('/usr/bin/ruby', installerArgs, { stdio: ['inherit', command === 'install' ? 'inherit' : 2, 'inherit'] });
if (installed.error) fail(`cannot run the system Ruby installer: ${installed.error.message}`);
if (installed.signal) {
  process.kill(process.pid, installed.signal);
  process.exit(128 + (osConstants.signals[installed.signal] ?? 1));
}
if (installed.status !== 0) process.exit(installed.status ?? 1);
if (command !== 'install') {
  const binary = join(options.get('--prefix'), 'bin/fabric');
  const env = { ...process.env };
  if (options.has('--trust-plugins')) env.FABRIC_PLUGIN_REGISTRY = join(options.get('--prefix'), 'registry');
  // Replace the launcher: native exit status and signals reach the caller unchanged.
  try {
    process.execve(binary, [binary, ...(command === 'up' ? ['init'] : []), ...nativeArgs], env);
  } catch (error) {
    fail(`cannot execute installed native Fabric: ${error.message}`);
  }
}
