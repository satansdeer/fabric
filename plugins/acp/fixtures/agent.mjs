import { createInterface } from 'node:readline';

const [mode, encoded] = process.argv.slice(2);
const send = value => process.stdout.write(JSON.stringify(value) + '\n');
const reply = (id, result) => send({ jsonrpc: '2.0', id, result });
const notification = (method, params = {}) => ({ jsonrpc: '2.0', method, params });
const normal = {
  protocolVersion: 1,
  agentCapabilities: { loadSession: true, promptCapabilities: { image: true }, sessionCapabilities: { list: {} } },
  authMethods: [{ id: 'local', name: 'Local metadata', description: null }],
  agentInfo: { name: 'fixture', version: '1', title: '界 🐎' },
};
if (mode === 'ignore-term') {
  process.on('SIGTERM', () => {});
  setInterval(() => {}, 1000);
}
let reversed = [];
let blocking;
let answers = [];
let lastNotification;
let initializeSeen;
for await (const message of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
  const value = JSON.parse(message);
  if (Object.hasOwn(value, 'error')) {
    answers.push(value);
    if (answers.length === 4) { reply(blocking, answers); answers = []; }
    continue;
  }
  const { id, method, params } = value;
  if (id === undefined) { lastNotification = value; continue; }
  if (method === 'initialize') {
    if (mode === 'init-hang') { send(notification('fixture/initializing')); continue; }
    if (mode === 'init-error') {
      send({ jsonrpc: '2.0', id, error: { code: -32000, message: 'PRIVATE_CHILD_TEXT', data: 'PRIVATE_CHILD_TEXT' } });
      continue;
    }
    reply(id, mode === 'init-value' ? JSON.parse(encoded) : normal);
  } else if (method === 'inspect-initialize') {
    reply(id, { explicit: process.env.ACP_FIXTURE, inherited: Object.hasOwn(process.env, params.parentKey),
      cwd: process.cwd(), args: process.argv.slice(2) });
  } else if (method === 'echo') reply(id, params);
  else if (method === 'last-notification') reply(id, lastNotification);
  else if (method === 'fragments') {
    const bytes = Buffer.from(JSON.stringify(notification('session/update', { text: '界🐎' })) + '\n' +
      JSON.stringify({ jsonrpc: '2.0', id, result: 'naïve 🐎' }) + '\n');
    const split = bytes.indexOf(Buffer.from('🐎')) + 1;
    process.stdout.write(bytes.subarray(0, split));
    await new Promise(resolve => setImmediate(resolve));
    process.stdout.write(bytes.subarray(split, split + 1));
    await new Promise(resolve => setImmediate(resolve));
    process.stdout.write(bytes.subarray(split + 1));
  } else if (method === 'reverse') {
    reversed.push({ jsonrpc: '2.0', id, result: params });
    if (reversed.length === 2) {
      process.stdout.write(reversed.reverse().map(value => JSON.stringify(value) + '\n').join(''));
      reversed = [];
    }
  } else if (method === 'blocking') {
    blocking = id;
    for (const [id, method] of [['ask', 'cursor/ask_question'], [1, 'cursor/create_plan'],
      ['permission', 'session/request_permission'], ['terminal', 'terminal/create']]) {
      send({ jsonrpc: '2.0', id, method, params: { content: 'PRIVATE_CHILD_TEXT' } });
    }
  } else if (method === 'unknown-notification') {
    send(notification('unknown/capabilities', { terminal: true, fs: { writeTextFile: true } }));
    reply(id, {});
  } else if (method === 'hang') {
    send(notification('fixture/pending', params));
  } else if (method === 'remote-error') {
    process.stderr.write('PRIVATE_CHILD_TEXT\n');
    send({ jsonrpc: '2.0', id, error: { code: -32000, message: 'PRIVATE_CHILD_TEXT', data: { secret: 'PRIVATE_CHILD_TEXT' } } });
  } else if (method === 'notify-twice') {
    process.stdout.write([notification('one'), notification('two')].map(value => JSON.stringify(value) + '\n').join(''));
  } else if (method === 'sized') {
    const result = { recordBytes: params.bytes, padding: '' };
    const envelope = { jsonrpc: '2.0', id, result };
    const paddingBytes = params.bytes - Number(params.crlf) - Buffer.byteLength(JSON.stringify(envelope));
    result.padding = '界'.repeat(Math.floor(paddingBytes / 3)) + 'x'.repeat(paddingBytes % 3);
    process.stdout.write(JSON.stringify(envelope) + (params.crlf ? '\r\n' : '\n'));
  } else if (method === 'fault') {
    switch (params.kind) {
      case 'json': process.stdout.write('PRIVATE_CHILD_TEXT\n'); break;
      case 'utf8': process.stdout.write(Buffer.from([0xff, 10])); break;
      case 'oversized': process.stdout.write('x'.repeat(params.bytes)); break;
      case 'truncated': process.stdout.end('{"jsonrpc":'); break;
      case 'eof': process.stdout.end(); break;
      case 'exit': process.exit(0); break;
      case 'duplicate':
        process.stdout.write([id, id].map(id => JSON.stringify({ jsonrpc: '2.0', id, result: 'first' }) + '\n').join(''));
        break;
      case 'remote-id': send({ jsonrpc: '2.0', id: params.id, result: {} }); break;
      case 'bad-error': send({ jsonrpc: '2.0', id, error: { code: 'no', message: 'PRIVATE_CHILD_TEXT' } }); break;
      case 'mixed': send({ jsonrpc: '2.0', id, method: 'echo', result: {} }); break;
      case 'both': send({ jsonrpc: '2.0', id, result: {}, error: { code: 1, message: 'no' } }); break;
      case 'array': send([{ jsonrpc: '2.0', id, result: {} }]); break;
      case 'params': send({ jsonrpc: '2.0', method: 'oops', params: 'PRIVATE_CHILD_TEXT' }); break;
      case 'request-id': send({ jsonrpc: '2.0', id: null, method: 'oops' }); break;
      case 'extra': send({ jsonrpc: '2.0', id, result: {}, extra: true }); break;
    }
  } else if (method === 'initialize-seen') {
    reply(id, initializeSeen);
  }
  if (method === 'initialize') initializeSeen = value;
}
