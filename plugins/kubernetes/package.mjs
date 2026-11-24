#!/usr/bin/env node
// Explicit owner action only. Reuse the public SDK installer and content hashing.
import { mkdirSync, writeFileSync, readFileSync, realpathSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { directory } from '../../sdk/typescript/package.mjs';

const [destination, python, configuration] = process.argv.slice(2);
if (!destination || !python || !configuration || process.argv.length !== 5) throw Error('Usage: node package.mjs NEW_PRIVATE_DIRECTORY ABSOLUTE_PYTHON ABSOLUTE_PRIVATE_CONFIG');
const root = resolve(destination), config = realpathSync(configuration), runtime = realpathSync(python);
if (python !== runtime || configuration !== config || realpathSync(dirname(root)) !== dirname(root)) throw Error('Canonical absolute paths required');
const meta = statSync(config);
if (!meta.isFile() || meta.uid !== process.getuid() || meta.mode & 0o077 || meta.nlink !== 1 || meta.size > 65536 || !statSync(runtime).isFile()) throw Error('Private configuration required');
const digest = createHash('sha256').update(readFileSync(config)).digest('hex');
const quote = text => "'" + text.replaceAll("'", "'\\''") + "'";
mkdirSync(root, { mode: 0o700 });
writeFileSync(join(root, 'target.py'), readFileSync(join(dirname(fileURLToPath(import.meta.url)), 'target.py')), { flag: 'wx', mode: 0o600 });
writeFileSync(join(root, 'plugin'), '#!/bin/sh\nexec ' + quote(runtime) + ' "$(dirname "$0")/target.py" --config ' + quote(config) + ' --config-sha256 ' + quote(digest) + ' "$@"\n', { flag: 'wx', mode: 0o700 });
writeFileSync(join(root, 'package.json'), JSON.stringify({ name: 'kubernetes', version: '0.1.0', files: ['plugin', 'target.py'], fabricPlugin: { id: 'kubernetes', kind: 'target', entry: 'plugin', sdk: '0.1.0', protocol: 1, runtime: 'native', args: [] } }, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
const snapshot = directory(root);
console.log(JSON.stringify({ id: snapshot.info.id, version: snapshot.info.version, sha256: snapshot.hash }));
