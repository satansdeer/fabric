#!/usr/bin/env node
// Trusted-local HTTP reference. Only Core's existing durable manager admits work.
import { createServer } from 'node:http';
import { spawnSync } from 'node:child_process';
import { isAbsolute } from 'node:path';
import { pathToFileURL } from 'node:url';

export function webhookServer({ fabric, root, registry, automation }) {
  if (![fabric, root, registry, automation].every(p => typeof p === 'string' && isAbsolute(p))) throw new Error('absolute_paths_required');
  const server = createServer(async (req, res) => {
    const reply = (status, body) => { res.writeHead(status, { 'content-type': 'application/json', 'connection': 'close' }); res.end(JSON.stringify(body)); };
    if (req.method !== 'POST' || req.url !== '/webhook') { req.resume(); reply(404, { error: 'unknown_route' }); return; }
    if (Number(req.headers['content-length']) > 8192) { req.resume(); reply(413, { error: 'payload_limit' }); return; }
    const names = ['authorization', 'x-fabric-timestamp', 'x-fabric-expires', 'x-fabric-nonce'];
    if (names.some(name => req.rawHeaders.filter((_, i) => i % 2 === 0 && req.rawHeaders[i].toLowerCase() === name).length !== 1) ||
        !/^[0-9]{1,16}$/.test(req.headers['x-fabric-timestamp']) || !/^[0-9]{1,16}$/.test(req.headers['x-fabric-expires']) ||
        !/^[A-Za-z0-9_.-]{1,128}$/.test(req.headers['x-fabric-nonce']) || req.headers.authorization.length > 512) {
      req.resume(); reply(400, { error: 'invalid_envelope' }); return;
    }
    const chunks = [];
    let size = 0, oversized = false;
    try {
      for await (const chunk of req) {
        size += chunk.length;
        if (oversized) continue;
        if (size > 8192) { oversized = true; chunks.length = 0; reply(413, { error: 'payload_limit' }); continue; }
        chunks.push(chunk);
      }
      if (oversized) return;
      const body = new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks, size));
      const envelope = { body, timestamp: Number(req.headers['x-fabric-timestamp']), expires_at: Number(req.headers['x-fabric-expires']),
        nonce: req.headers['x-fabric-nonce'], authorization: req.headers.authorization };
      const result = spawnSync(fabric, ['--root', root, 'connector', '--registry', registry, '--automation', automation, '--request', '-'], {
        input: JSON.stringify(envelope), encoding: 'utf8', cwd: '/', env: { PATH: '/usr/bin:/bin', LC_ALL: 'C', FABRIC_PLUGIN_REGISTRY: registry },
        timeout: 30000, maxBuffer: 1024 * 1024, stdio: ['pipe', 'pipe', 'ignore'],
      });
      if (result.error || result.status !== 0) { reply(403, { error: 'not_acknowledged' }); return; }
      const ack = JSON.parse(result.stdout);
      if (ack.version !== 1 || ack.acknowledged !== true) { reply(503, { error: 'not_acknowledged' }); return; }
      reply(202, ack);
    } catch { if (!res.headersSent) reply(400, { error: 'invalid_request' }); }
  });
  server.requestTimeout = 5000;
  server.headersTimeout = 5000;
  server.keepAliveTimeout = 1000;
  server.maxHeadersCount = 32;
  return server;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const [fabric, root, registry, automation, portText, ...extra] = process.argv.slice(2);
    const port = Number(portText);
    if (extra.length || !Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid_arguments');
    const server = webhookServer({ fabric, root, registry, automation });
    server.on('error', () => { process.stderr.write('Fabric webhook: listener failed.\n'); process.exitCode = 1; });
    server.listen(port, '127.0.0.1', () => process.stdout.write('Fabric webhook: listening on loopback.\n'));
  } catch { process.stderr.write('Usage: node fabric-webhook.mjs FABRIC ROOT REGISTRY AUTOMATION PORT\n'); process.exitCode = 1; }
}
