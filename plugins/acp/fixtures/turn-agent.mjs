// Deterministic fake ACP peer for integration and extracted-release smoke only; never a native agent.
import { createInterface } from 'node:readline';
import { appendFileSync, writeFileSync, existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';

const log = process.argv[2];
const record = value => appendFileSync(log, JSON.stringify(value) + '\n');
const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
let initialized = false, session = false;
const tool = { sessionUpdate: 'tool_call', toolCallId: 'SECRET_TOOL_ID_CANARY', title: 'SECRET_TOOL_TITLE_CANARY' };
const delta = { sessionUpdate: 'tool_call_update', toolCallId: tool.toolCallId };
process.on('SIGTERM', () => { record({ stopped: process.pid }); process.exit(0); });
for await (const line of createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  record({ method: request.method });
  if (!Object.hasOwn(request, 'method')) { record({ response: request }); continue; }
  if (request.method === 'initialize') {
    if (initialized || request.params.protocolVersion !== 1 || Object.keys(request.params.clientCapabilities).length) process.exit(21);
    initialized = true;
    if (process.argv[3]?.startsWith('tool-early-')) record({ early: process.argv[3], earlyPid: process.pid });
    if (process.argv[3] === 'tool-early-initialize') send({ method: 'session/update',
      params: { sessionId: 'connection-local', update: tool } });
    send({ id: request.id, result: { protocolVersion: 1, agentCapabilities: {}, authMethods: [],
      agentInfo: { name: 'UNTRUSTED_IDENTITY_CANARY', version: 'UNTRUSTED_VERSION_CANARY' } } });
  } else if (request.method === 'session/new') {
    if (!initialized || session || request.params.cwd !== process.cwd() || request.params.mcpServers.length) process.exit(22);
    session = true;
    if (process.argv[3] === 'tool-early-session') send({ method: 'session/update',
      params: { sessionId: 'connection-local', update: delta } });
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
    if (['cancel', 'concurrent'].includes(mode)) {
      update(tool); update({ ...delta, status: 'in_progress' });
    }
    if (mode === 'tool') {
      update({ ...tool, name: 'SECRET_TOOL_NAME_CANARY', rawInput: false, rawOutput: null });
      update(delta);
      update({ ...delta, title: null, name: null, kind: null, status: null, content: null, locations: null,
        rawInput: [1, 'SECRET_ARGUMENT_CANARY'], rawOutput: 42 });
      update({ ...delta, status: 'in_progress', kind: 'execute', rawInput: '$(touch MUST_NOT_EXIST)',
        rawOutput: 'SECRET_OUTPUT_CANARY', _meta: { command: 'touch MUST_NOT_EXIST' },
        locations: [{ path: `${log}.MUST_NOT_EXIST`, line: 0 }, { path: 'SECRET_PATH_CANARY', line: null }],
        content: [
          { type: 'diff', path: `${log}.MUST_NOT_EXIST`, oldText: null, newText: 'SECRET_DIFF_CANARY' },
          { type: 'diff', path: 'README.md', newText: 'SECRET_DIFF_CANARY' },
          { type: 'terminal', terminalId: 'SECRET_TERMINAL_CANARY' },
          { type: 'content', content: { type: 'text', text: 'SECRET_CONTENT_CANARY',
            annotations: { audience: ['assistant'], priority: 0.5, lastModified: null }, _meta: null } },
          { type: 'content', content: { type: 'image', data: 'SECRET_IMAGE_CANARY', mimeType: 'image/png', uri: null } },
          { type: 'content', content: { type: 'audio', data: 'SECRET_AUDIO_CANARY', mimeType: 'audio/wav' } },
          { type: 'content', content: { type: 'resource_link', uri: 'SECRET_URI_CANARY', name: 'SECRET_LINK_CANARY', size: 1 } },
          { type: 'content', content: { type: 'resource', resource: { uri: 'SECRET_URI_CANARY', text: 'SECRET_RESOURCE_CANARY' } } },
          { type: 'content', content: { type: 'resource', resource: { uri: 'SECRET_URI_CANARY', blob: 'SECRET_BLOB_CANARY' } } },
        ] });
      update(text);
      update({ ...delta, status: 'completed', content: [], locations: [], rawInput: null, rawOutput: true });
      update({ ...delta, name: 'SECRET_CORRECTED_NAME_CANARY', rawOutput: {} });
      update({ ...delta, status: 'in_progress', rawInput: 0 }); // v1 does not require monotonic statuses.
      update({ ...delta, status: 'failed' });
      update({ ...delta, toolCallId: 'SECRET_ORPHAN_CANARY', rawInput: '' });
      update({ ...delta, toolCallId: 'SECRET_ORPHAN_CANARY', status: 'completed' });
      update({ ...tool, toolCallId: 'SECRET_TERMINAL_INITIAL_CANARY', status: 'failed', name: null });
      update({ ...tool, toolCallId: '', status: 'completed' });
      update({ ...tool, toolCallId: 'SECRET_INCOMPLETE_CANARY', status: 'in_progress' });
    }
    if (['tool-limit', 'tool-overflow'].includes(mode)) {
      for (let i = 0; i < (mode === 'tool-limit' ? 256 : 257); i++) {
        update({ ...tool, toolCallId: `SECRET_ID_${i}` });
        if (i % 16 === 15) await delay(10);
      }
    }
    if (mode.startsWith('tool-invalid-')) {
      const invalid = JSON.parse(prompt.slice(prompt.indexOf('\n') + 1));
      update(invalid);
    }
    if (mode === 'tool-mismatch') update(tool, 'other-connection');
    if (mode === 'tool-late') {
      process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } }) + '\n' +
        JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'connection-local', update: delta } }) + '\n');
      continue;
    }
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
    if (['request', 'vendor-request', 'extension-request', 'tool-request', 'tool-fs-request', 'tool-terminal-request'].includes(mode)) {
      if (mode.startsWith('tool-')) update(tool);
      send({ id: mode === 'extension-request' ? 0 : 'peer-request',
        method: { request: 'session/request_permission', 'vendor-request': 'cursor/ask_question', 'extension-request': 'cursor/task',
          'tool-request': 'session/request_permission', 'tool-fs-request': 'fs/write_text_file',
          'tool-terminal-request': 'terminal/create' }[mode],
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
    else if (mode === 'image') update({ sessionUpdate: 'agent_message_chunk', content: { type: 'image', data: 'SECRET_IMAGE_CANARY', mimeType: 'image/png' } });
    else if (['core-session', 'core-fs', 'core-terminal', 'core-method', 'core-rpc'].includes(mode)) {
      send({ method: { 'core-session': 'session/future', 'core-fs': 'fs/changed',
        'core-terminal': 'terminal/created', 'core-method': 'authenticate',
        'core-rpc': 'rpc.private/event' }[mode], params: {} });
    }
    else if (mode !== 'stop' && !mode.startsWith('tool')) {
      writeFileSync('README.md', mode === 'concurrent' ? `ACP fixture worker\n${prompt}\n` : 'ACP fixture worker');
      update(text);
      if (mode === 'extensions') {
        extension('cursor/update_todos');
        extension('cursor/generate_image');
      }
    }
    if (mode === 'concurrent') update({ ...delta, status: 'completed' });
    send({ id: request.id, result: { stopReason: mode === 'stop' ? 'invented-success' : 'end_turn' } });
  } else if (request.method === 'session/cancel') {
    record({ cancelled: process.pid });
  } else if (request.id !== undefined) process.exit(25);
}
