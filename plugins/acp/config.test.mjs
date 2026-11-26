import test from 'node:test';
import assert from 'node:assert/strict';
import { closeSync, ftruncateSync, mkdtempSync, openSync, realpathSync, rmSync, writeSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileHash } from './config.mjs';

test('large native CLI pins retain content verification and a bounded file size', t => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), 'acp-large-pin-')));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const path = join(root, 'native'), fd = openSync(path, 'w');
  try {
    const previousLimit = 256 * 1024 * 1024;
    writeSync(fd, Buffer.from([1]), 0, 1, previousLimit);
    assert.equal(fileHash(path), 'a623c8547710fef0ba1a9eb6fffdc664e87aed251802123ae17389ab768eaa15');
    ftruncateSync(fd, 512 * 1024 * 1024 + 1);
    assert.throws(() => fileHash(path), { code: 'acp_unsafe_file' });
  } finally { closeSync(fd); }
});
