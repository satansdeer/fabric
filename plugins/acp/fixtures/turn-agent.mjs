// Deterministic fake ACP peer for integration and extracted-release smoke only; never a native agent.
import { createInterface } from 'node:readline';
import { appendFileSync, writeFileSync, existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';

const log = process.argv[2];
const record = value => appendFileSync(log, JSON.stringify(value) + '\n');
const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
let initialized = false, session = false;
process.on('SIGTERM', () => { record({ stopped: process.pid }); process.exit(0); });
for await (const line of createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  record({ method: request.method });
  if (!Object.hasOwn(request, 'method')) { record({ response: request }); continue; }
  if (request.method === 'initialize') {
    if (initialized || request.params.protocolVersion !== 1 || Object.keys(request.params.clientCapabilities).length) process.exit(21);
    initialized = true;
    send({ id: request.id, result: { protocolVersion: 1, agentCapabilities: {}, authMethods: [],
      agentInfo: { name: 'UNTRUSTED_IDENTITY_CANARY', version: 'UNTRUSTED_VERSION_CANARY' } } });
  } else if (request.method === 'session/new') {
    if (!initialized || session || request.params.cwd !== process.cwd() || request.params.mcpServers.length) process.exit(22);
    session = true;
    send({ id: request.id, result: { sessionId: 'connection-local' } });
  } else if (request.method === 'session/prompt') {
    if (!session || request.params.sessionId !== 'connection-local' || request.params.prompt.length !== 1 || request.params.prompt[0].type !== 'text') process.exit(23);
    const prompt = request.params.prompt[0].text;
    const mode = prompt.startsWith('fixture:') ? prompt.slice(8).split('\n', 1)[0] : 'text';
    const pgid = pid => Number(spawnSync('/bin/ps', ['-o', 'pgid=', '-p', String(pid)], { encoding: 'utf8' }).stdout.trim());
    record({ mode, pid: process.pid, parent: process.ppid, group: pgid(process.pid), parentGroup: pgid(process.ppid),
      cwd: process.cwd(), home: process.env.HOME, environment: Object.keys(process.env).sort(), prompt });
    const update = (update, sessionId = 'connection-local') => send({ method: 'session/update', params: { sessionId, update } });
    const text = { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: prompt } };
    const extension = (method = 'cursor/task', data = 'PRIVATE_EXTENSION_CANARY') => send({
      method, params: { sessionId: 'untrusted-other-session', data, marker: mode },
    });
    if (['extensions', 'cancel', 'concurrent'].includes(mode)) extension();
    if (['count-limit', 'count-flood', 'byte-flood'].includes(mode)) {
      for (let i = 0; i < (mode === 'count-limit' ? 32 : 33); i++) {
        extension('vendor/progress', mode === 'byte-flood' ? '😀'.repeat(2000) : 'bounded');
      }
    }
    if (mode === 'large-extension') extension(`vendor/${'x'.repeat(3000)}`, '😀'.repeat(2000));
    if (mode === 'cancel') { record({ extensionsSent: mode }); continue; }
    if (mode === 'slow') await delay(2100); // Longer than the fixture's setup RPC deadline.
    if (mode === 'concurrent') {
      record({ extensionsSent: mode });
      while (!existsSync(`${log}.release`)) await delay(20);
    }
    if (mode === 'failure') { send({ id: request.id, error: { code: -32000, message: 'SECRET_PEER_ERROR_CANARY', data: { credential: 'SECRET_PEER_DATA_CANARY' } } }); continue; }
    if (mode === 'disconnect') { process.exit(24); }
    if (['request', 'vendor-request', 'extension-request'].includes(mode)) {
      send({ id: mode === 'extension-request' ? 0 : 'peer-request',
        method: mode === 'request' ? 'session/request_permission' : mode === 'vendor-request' ? 'cursor/ask_question' : 'cursor/task',
        params: { sessionId: 'connection-local', secret: 'SECRET_REQUEST_CANARY' } });
      continue;
    }
    if (mode === 'late') {
      process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } }) + '\n' +
        JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'connection-local', update: text } }) + '\n');
      continue;
    }
    if (mode === 'malformed') update({ ...text, unexpected: 'SECRET_UPDATE_CANARY' });
    else if (mode === 'mismatch') update(text, 'other-connection');
    else if (mode === 'tool') update({ sessionUpdate: 'tool_call', toolCallId: 'unsafe', title: 'SECRET_TOOL_CANARY' });
    else if (mode === 'image') update({ sessionUpdate: 'agent_message_chunk', content: { type: 'image', data: 'SECRET_IMAGE_CANARY', mimeType: 'image/png' } });
    else if (['core-session', 'core-fs', 'core-terminal', 'core-method', 'core-rpc'].includes(mode)) {
      send({ method: { 'core-session': 'session/future', 'core-fs': 'fs/changed',
        'core-terminal': 'terminal/created', 'core-method': 'authenticate',
        'core-rpc': 'rpc.private/event' }[mode], params: {} });
    }
    else if (mode !== 'stop') {
      writeFileSync('README.md', mode === 'concurrent' ? `ACP fixture worker\n${prompt}\n` : 'ACP fixture worker');
      update(text);
      if (mode === 'extensions') {
        extension('cursor/update_todos');
        extension('cursor/generate_image');
      }
    }
    send({ id: request.id, result: { stopReason: mode === 'stop' ? 'invented-success' : 'end_turn' } });
  } else if (request.method === 'session/cancel') {
    record({ cancelled: process.pid });
  } else if (request.id !== undefined) process.exit(25);
}
