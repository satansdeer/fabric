#!/usr/bin/env node
// Local-only protocol checks; no native runtime, config, credentials or transcript files.
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { request } from 'node:http';
import { connect } from 'node:net';
import { startScriptedProvider } from './scripted-provider.mjs';

const path = '/v1/chat/completions?fixture=1';
const step = overrides => ({ method: 'POST', path, ...overrides });
const deferred = () => {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const closeEvent = stream => new Promise(resolve => stream.once('close', resolve));

async function fixture(steps, check) {
  const provider = await startScriptedProvider(steps);
  let timer;
  try {
    await Promise.race([
      check(provider),
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Fixture check timed out')), 10_000); }),
    ]);
  } finally {
    clearTimeout(timer);
    await provider.close();
  }
}

function open(provider, body = '{}', options = {}) {
  const response = deferred();
  const req = request(provider.baseUrl + (options.path ?? path), {
    method: options.method ?? 'POST', agent: false,
    headers: { 'content-type': 'application/json', ...options.headers },
  }, res => response.resolve(res));
  req.on('error', response.reject);
  if (options.end !== false) req.end(body);
  return { req, response: response.promise };
}

async function read(response) {
  const res = await response;
  const chunks = [];
  for await (const chunk of res) chunks.push(chunk);
  return { status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString('utf8') };
}
const send = (provider, body, options) => read(open(provider, body, options).response);

const conversation = {
  model: 'scripted-only', stream: true,
  messages: [
    { role: 'user', content: 'Read the synthetic fixture.' },
    { role: 'assistant', content: null, tool_calls: [{ id: 'call_fixture', type: 'function', function: { name: 'read_fixture', arguments: '{"name":"synthetic"}' } }] },
    { role: 'tool', tool_call_id: 'call_fixture', content: 'synthetic result' },
  ],
  tools: [{ type: 'function', function: { name: 'read_fixture', parameters: { type: 'object' } } }],
};
const frames = [
  'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_next","type":"function","function":{"name":"read_fixture","arguments":""}}]}}]}\n\n',
  'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\\"name\\":"}}]}}]}\n\n',
  'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\\"synthetic\\"}"}}]},"finish_reason":"tool_calls"}]}\n\n',
  'data: [DONE]\n\n',
];
const wire = frames.join('');
await fixture([step({
  headers: { 'content-type': 'text/event-stream' },
  chunks: [wire.slice(0, 7), Buffer.from(wire.slice(7, 101)), wire.slice(101, -3), wire.slice(-3)],
  check({ method, path: actualPath, headers, body }) {
    assert.equal(method, 'POST');
    assert.equal(actualPath, path);
    assert.equal(headers.authorization, 'Bearer scripted-fixture-only');
    assert.equal(body.model, 'scripted-only');
    assert.equal(body.stream, true);
    assert.deepEqual(body.messages, conversation.messages);
    assert.deepEqual(body.tools, conversation.tools);
  },
})], async provider => {
  assert.match(provider.baseUrl, /^http:\/\/127\.0\.0\.1:\d+$/);
  const response = await send(provider, JSON.stringify(conversation), { headers: { authorization: 'Bearer scripted-fixture-only' } });
  assert.equal(response.status, 200);
  assert.equal(response.headers['content-type'], 'text/event-stream');
  assert.equal(response.body, wire);
  const payloads = response.body.split('\n\n').filter(Boolean).map(frame => frame.slice(6));
  assert.equal(payloads.pop(), '[DONE]');
  const deltas = payloads.map(frame => JSON.parse(frame).choices[0]);
  assert.equal(deltas[0].delta.tool_calls[0].id, 'call_next');
  assert.deepEqual(JSON.parse(deltas.map(delta => delta.delta.tool_calls[0].function.arguments).join('')), { name: 'synthetic' });
  assert.equal(deltas.at(-1).finish_reason, 'tool_calls');
  assert.deepEqual(provider.requests, [{ method: 'POST', path, body: conversation }]);
  provider.assertDone();
});

await fixture([step({ status: 429, headers: { 'content-type': 'application/json', 'retry-after': '0' }, chunks: ['{"error":{"message":"synthetic rate limit"}}'] })], async provider => {
  const response = await send(provider);
  assert.equal(response.status, 429);
  assert.equal(response.headers['retry-after'], '0');
  assert.equal(JSON.parse(response.body).error.message, 'synthetic rate limit');
  provider.assertDone();
});

await fixture([step({ chunks: ['data: partial\n\n'], disconnect: true })], async provider => {
  await assert.rejects(send(provider));
  provider.assertDone();
});

// A held response neither blocks validation of the next request nor close().
await fixture([step({ chunks: ['data: held\n\n'], wait: new Promise(() => {}) }), step({ chunks: ['second'] })], async provider => {
  const held = await open(provider).response;
  const reader = held[Symbol.asyncIterator]();
  assert.equal((await reader.next()).value.toString(), 'data: held\n\n');
  assert.equal((await send(provider)).body, 'second');
  provider.assertDone();
  const closed = closeEvent(held);
  const firstClose = provider.close();
  assert.equal(provider.close(), firstClose);
  await firstClose;
  await closed;
  provider.assertDone();
});

await fixture([step({ chunks: ['held'], wait: new Promise(() => {}) }), step({ chunks: ['after cancellation'] })], async provider => {
  const held = await open(provider).response;
  const reader = held[Symbol.asyncIterator]();
  assert.equal((await reader.next()).value.toString(), 'held');
  const closed = closeEvent(held);
  held.destroy();
  await closed;
  assert.equal((await send(provider)).body, 'after cancellation');
  provider.assertDone();
});

const release = deferred();
await fixture([step({ chunks: ['before wait'], wait: release.promise })], async provider => {
  const res = await open(provider).response;
  const reader = res[Symbol.asyncIterator]();
  assert.equal((await reader.next()).value.toString(), 'before wait');
  assert.equal(res.complete, false);
  release.resolve();
  assert.equal((await reader.next()).done, true);
  provider.assertDone();
});

// Bodies can arrive out of order; checks must still use request arrival order.
const checked = deferred();
await fixture([
  step({ check: () => checked.resolve(), chunks: ['first'] }),
  step({ path: '/second', chunks: ['second'] }),
], async provider => {
  const first = open(provider, undefined, { end: false, headers: { expect: '100-continue' } });
  const admitted = once(first.req, 'continue');
  first.req.flushHeaders();
  await admitted;
  first.req.write('{');
  const second = open(provider, undefined, { path: '/second', end: false, headers: { expect: '100-continue' } });
  const secondAdmitted = once(second.req, 'continue');
  second.req.flushHeaders();
  await secondAdmitted;
  second.req.end('{}');
  first.req.end('}');
  await checked.promise;
  assert.equal((await read(first.response)).body, 'first');
  assert.equal((await read(second.response)).body, 'second');
  provider.assertDone();
});

for (const options of [
  { body: '{', expected: /JSON/ },
  { body: '{}', headers: { 'content-type': 'text/plain' }, expected: /application\/json/ },
  { body: Buffer.from([0x22, 0xff, 0x22]), expected: /JSON/ },
  { body: '{}', method: 'PUT', expected: /Unexpected request/ },
  { body: '{}', path: '/v1/chat/completions?fixture=wrong', expected: /Unexpected request/ },
]) {
  await fixture([step()], async provider => {
    const response = await send(provider, options.body, options);
    assert.ok(response.status >= 400);
    assert.throws(provider.assertDone, options.expected);
    await provider.close();
    assert.throws(provider.assertDone, options.expected);
  });
}

await fixture([step({ check: () => assert.fail('synthetic failed check') })], async provider => {
  assert.equal((await send(provider)).status, 400);
  assert.throws(provider.assertDone, /check failed/);
  assert.equal((await send(provider)).status, 500, 'failed validation cannot fall through to a successful response');
  assert.throws(provider.assertDone, /check failed/);
});
await fixture([], async provider => {
  assert.equal((await send(provider)).status, 400);
  assert.throws(provider.assertDone, /Unexpected request/);
});
await fixture([step()], async provider => {
  assert.throws(provider.assertDone, /Unconsumed/);
});

// Exact byte/count boundaries are usable; one byte/request over fails closed.
const maxBody = '"' + 'x'.repeat(1024 * 1024 - 2) + '"';
await fixture([step()], async provider => {
  assert.equal((await send(provider, maxBody)).status, 200);
  assert.equal(provider.requests[0].body.length, 1024 * 1024 - 2);
  provider.assertDone();
});
await fixture([step()], async provider => {
  const declared = open(provider, undefined, {
    end: false, headers: { 'content-length': String(1024 * 1024 + 1), expect: '100-continue' },
  });
  declared.req.flushHeaders();
  assert.equal((await read(declared.response)).status, 413);
  assert.throws(provider.assertDone, /exceeds 1 MiB/);
  assert.equal(provider.requests.length, 0);
});
await fixture([step()], async provider => {
  // Closing an oversized upload may reset its still-writing socket before 413 arrives.
  await send(provider, '"' + 'x'.repeat(1024 * 1024) + '"', {
    headers: { 'transfer-encoding': 'chunked' },
  }).then(
    response => assert.equal(response.status, 413),
    error => assert.ok(['ECONNRESET', 'EPIPE'].includes(error.code), error.message),
  );
  assert.throws(provider.assertDone, /exceeds 1 MiB/);
  assert.equal(provider.requests.length, 0);
});
await fixture(Array.from({ length: 128 }, () => step()), async provider => {
  for (let i = 0; i < 128; i++) assert.equal((await send(provider)).status, 200);
  provider.assertDone();
  assert.equal((await send(provider)).status, 503);
  assert.equal(provider.requests.length, 128);
  assert.throws(provider.assertDone, /count exceeds 128/);
});
await fixture([step({ chunks: [Buffer.alloc(4 * 1024 * 1024, 120)] })], async provider => {
  assert.equal((await send(provider)).body, 'x'.repeat(4 * 1024 * 1024));
  provider.assertDone();
});

for (const invalid of [
  null, Array.from({ length: 129 }, () => step()), [null],
  [step({ method: 'BAD\r\nMETHOD' })], [step({ path: '/bad\r\npath' })],
  [step({ status: 199 })], [step({ status: 600 })], [step({ check: true })],
  [step({ headers: { 'bad\nname': 'value' } })], [step({ headers: { good: 'bad\r\nvalue' } })],
  [step({ headers: { good: ['safe', 'bad\nvalue'] } })], [step({ chunks: [123] })],
  [step({ disconnect: 'yes' })], [step({ wait: {} })], [step({ host: '0.0.0.0' })],
  [step({ chunks: ['x'.repeat(4 * 1024 * 1024)] }), step({ chunks: ['x'] })],
]) {
  await assert.rejects(startScriptedProvider(invalid));
}

// Invalid HTTP and interrupted inbound bodies cannot leave a passing fixture.
await fixture([step()], async provider => {
  const socket = connect(new URL(provider.baseUrl).port, '127.0.0.1');
  socket.on('error', () => {});
  const closed = closeEvent(socket);
  socket.end('NOT HTTP\r\n\r\n');
  await closed;
  assert.throws(provider.assertDone, /Malformed HTTP/);
});
await fixture([step()], async provider => {
  const socket = connect(new URL(provider.baseUrl).port, '127.0.0.1');
  socket.on('error', () => {});
  const closed = closeEvent(socket);
  // FIN after a partial Content-Length body deterministically exercises abortion.
  socket.end(`POST ${path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{`);
  await closed;
  assert.throws(provider.assertDone, /Malformed HTTP|aborted|body error/);
});

await fixture([], async provider => {
  const socket = connect(new URL(provider.baseUrl).port, '127.0.0.1');
  socket.on('error', () => {});
  const closed = closeEvent(socket);
  await once(socket, 'connect');
  await provider.close();
  await closed;
  provider.assertDone();
});

const rejectedWait = deferred();
await fixture([step({ chunks: ['waiting'], wait: rejectedWait.promise })], async provider => {
  const res = await open(provider).response;
  const reader = res[Symbol.asyncIterator]();
  await reader.next();
  rejectedWait.reject(new Error('synthetic wait failure'));
  await assert.rejects(reader.next());
  assert.throws(provider.assertDone, /wait rejected/);
  await provider.close();
  assert.throws(provider.assertDone, /wait rejected/);
});

console.log('PASS: scripted loopback SSE/tools, errors, cancellation, ordering, fail-closed validation and byte/count bounds');
