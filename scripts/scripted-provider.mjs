// Test-only, bounded loopback fixture. Never forwards requests or persists transcripts.
import assert from 'node:assert/strict';
import { createServer, validateHeaderName, validateHeaderValue } from 'node:http';

const MAX_REQUESTS = 128;
const MAX_BODY = 1024 * 1024;
const MAX_RESPONSE = 4 * 1024 * 1024;
const CANCELLED = Symbol('cancelled');

export async function startScriptedProvider(steps) {
  assert.ok(Array.isArray(steps) && steps.length <= MAX_REQUESTS, 'Expected at most 128 steps');
  let responseBytes = 0;
  const script = steps.map(step => {
    assert.ok(step && typeof step === 'object' && !Array.isArray(step), 'Invalid step');
    assert.ok(Object.keys(step).every(key => ['method', 'path', 'check', 'status', 'headers', 'chunks', 'disconnect', 'wait'].includes(key)), 'Unknown step option');
    assert.equal(typeof step.method, 'string', 'Invalid method');
    validateHeaderName(step.method);
    assert.ok(typeof step.path === 'string' && /^\/[^\s#]*$/.test(step.path), 'Expected an exact origin-form path');
    assert.ok(step.check === undefined || typeof step.check === 'function', 'Invalid check');
    const status = step.status === undefined ? 200 : step.status;
    assert.ok(Number.isInteger(status) && status >= 200 && status <= 599, 'Invalid response status');
    assert.ok(step.disconnect === undefined || typeof step.disconnect === 'boolean', 'Invalid disconnect');
    assert.ok(step.wait === undefined || step.wait instanceof Promise, 'Invalid wait');
    const headers = Object.create(null);
    assert.ok(step.headers === undefined || (step.headers && typeof step.headers === 'object' && !Array.isArray(step.headers)), 'Invalid headers');
    for (const [name, value] of Object.entries(step.headers ?? {})) {
      validateHeaderName(name);
      const values = Array.isArray(value) ? value : [value];
      assert.ok(values.length > 0, 'Empty header values');
      for (const item of values) {
        assert.ok(typeof item === 'string' || (typeof item === 'number' && Number.isFinite(item)), 'Invalid header value');
        validateHeaderValue(name, item);
      }
      headers[name] = Array.isArray(value) ? value.slice() : value;
    }
    assert.ok(step.chunks === undefined || Array.isArray(step.chunks), 'Invalid chunks');
    const chunks = (step.chunks ?? []).slice();
    for (const chunk of chunks) {
      assert.ok(typeof chunk === 'string' || Buffer.isBuffer(chunk), 'Invalid response chunk');
      responseBytes += Buffer.byteLength(chunk);
      assert.ok(responseBytes <= MAX_RESPONSE, 'Declared responses exceed 4 MiB');
    }
    return { ...step, status, headers, chunks };
  });

  let failure;
  const fail = message => { failure ??= new Error(message); };
  // Observe rejected waits immediately, including waits on steps never reached.
  for (const step of script) {
    step.wait = step.wait?.then(() => undefined, () => { fail('Scripted wait rejected'); });
  }
  let closing = false;
  let closePromise;
  let stop;
  const stopped = new Promise(resolve => { stop = () => resolve(CANCELLED); });
  const sockets = new Set();
  const requests = [];
  let received = 0;
  let next = 0;
  let validation = Promise.resolve();

  function reject(res, status, message) {
    fail(message);
    if (!res.destroyed && !res.headersSent) {
      res.writeHead(status, { 'content-type': 'text/plain', connection: 'close' });
      res.end('Scripted provider rejected request');
    } else {
      res.destroy();
    }
  }

  function readBody(req, res) {
    return new Promise(resolve => {
      let chunks = [];
      let size = 0;
      let settled = false;
      const finish = value => {
        if (settled) return;
        settled = true;
        chunks = [];
        resolve(value);
      };
      req.on('error', () => {
        if (!closing) fail('Request body error');
        finish(CANCELLED);
      });
      req.on('aborted', () => {
        if (!closing) fail('Request body aborted');
        finish(CANCELLED);
      });
      req.on('data', chunk => {
        if (settled) return;
        size += chunk.length;
        if (size > MAX_BODY) {
          reject(res, 413, 'Request body exceeds 1 MiB');
          finish(CANCELLED);
        } else {
          chunks.push(chunk);
        }
      });
      req.on('end', () => {
        if (settled) return;
        try {
          const text = new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks, size));
          finish({ body: JSON.parse(text) });
        } catch {
          reject(res, 400, 'Invalid JSON request body');
          finish(CANCELLED);
        }
      });
      if (!/^application\/json(?:\s*;\s*charset=utf-8)?$/i.test(req.headers['content-type'] ?? '')) {
        reject(res, 415, 'Expected application/json request body');
        finish(CANCELLED);
      } else if (req.headers['content-length'] !== undefined && Number(req.headers['content-length']) > MAX_BODY) {
        reject(res, 413, 'Request body exceeds 1 MiB');
        finish(CANCELLED);
      }
    });
  }

  async function respond(res, step) {
    let cancel;
    const disconnected = new Promise(resolve => { cancel = () => resolve(CANCELLED); });
    res.on('close', cancel);
    res.on('error', cancel);
    const race = promise => Promise.race([promise, disconnected, stopped]);
    try {
      if (closing || res.destroyed) return;
      res.writeHead(step.status, step.headers);
      res.flushHeaders();
      for (const chunk of step.chunks) {
        // Write callbacks bound buffering and ensure chunks flush before wait.
        const result = await race(new Promise(resolve => res.write(chunk, error => resolve(error))));
        if (result === CANCELLED || res.destroyed) return;
        if (result) throw result;
      }
      if (step.wait && await race(step.wait) === CANCELLED) return;
      if (closing || res.destroyed) return;
      if (failure) res.destroy();
      else if (step.disconnect) res.destroy();
      else res.end();
    } catch {
      if (!closing && !res.destroyed) fail('Scripted response failed');
      res.destroy();
    } finally {
      res.off('close', cancel);
      res.off('error', cancel);
    }
  }

  const server = createServer((req, res) => {
    // Client cancellation is allowed after a fully validated request.
    res.on('error', () => {});
    received++;
    if (closing || received > MAX_REQUESTS) {
      req.on('error', () => {});
      reject(res, 503, 'Request count exceeds 128 or fixture is closing');
      req.resume();
      return;
    }
    const body = readBody(req, res);
    // Arrival order governs validation, never body completion or response lifetime.
    validation = validation.then(async () => {
      const parsed = await Promise.race([body, stopped]);
      if (parsed === CANCELLED || closing) return;
      if (failure) { reject(res, 500, 'Fixture already failed'); return; }
      requests.push({ method: req.method, path: req.url, body: parsed.body });
      const step = script[next];
      if (!step || req.method !== step.method || req.url !== step.path) {
        reject(res, 400, `Unexpected request at step ${next + 1}`);
        return;
      }
      try {
        const checked = step.check?.({ method: req.method, path: req.url, headers: req.headers, body: parsed.body });
        if (await Promise.race([Promise.resolve(checked), stopped]) === CANCELLED || closing) return;
      } catch {
        reject(res, 400, `Request check failed at step ${next + 1}`);
        return;
      }
      if (failure) { reject(res, 500, 'Fixture already failed'); return; }
      next++;
      // Deliberately detached: a held stream must not block the next request.
      void respond(res, step);
    }).catch(() => { reject(res, 500, 'Request validation failed'); });
  });
  server.maxConnections = MAX_REQUESTS;
  server.on('connection', socket => {
    sockets.add(socket);
    socket.on('error', () => {});
    socket.on('close', () => sockets.delete(socket));
    if (closing) socket.destroy();
  });
  server.on('drop', () => fail('Connection count exceeds 128'));
  server.on('clientError', (_error, socket) => {
    if (!closing) fail('Malformed HTTP request');
    socket.destroy();
  });
  for (const event of ['upgrade', 'connect']) {
    server.on(event, (req, socket) => {
      fail('Unsupported HTTP request');
      socket.destroy();
    });
  }
  server.on('error', () => fail('Scripted server error'));
  await new Promise((resolve, rejectListen) => {
    server.once('error', rejectListen);
    server.listen(0, '127.0.0.1', () => {
      server.off('error', rejectListen);
      resolve();
    });
  });

  return {
    baseUrl: `http://127.0.0.1:${server.address().port}`,
    requests,
    assertDone() {
      if (failure) throw failure;
      assert.equal(next, script.length, `Unconsumed scripted steps: ${script.length - next}`);
      assert.equal(received, next, 'Pending or extra scripted requests');
    },
    close() {
      if (!closePromise) {
        closing = true;
        stop();
        closePromise = new Promise((resolve, rejectClose) => {
          server.close(error => error ? rejectClose(error) : resolve());
          for (const socket of sockets) socket.destroy();
        });
      }
      return closePromise;
    },
  };
}
