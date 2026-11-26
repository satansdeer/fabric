// Deterministic fake ACP peer for integration and extracted-release smoke only; never a native agent.
import { createInterface } from 'node:readline';
import { appendFileSync, writeFileSync, existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';

const log = process.argv[2];
const record = value => appendFileSync(log, JSON.stringify(value) + '\n');
const send = value => process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...value }) + '\n');
let initialized = false, session = false;
let permissionTurn;
const tool = { sessionUpdate: 'tool_call', toolCallId: 'SECRET_TOOL_ID_CANARY', title: 'SECRET_TOOL_TITLE_CANARY' };
const delta = { sessionUpdate: 'tool_call_update', toolCallId: tool.toolCallId };
const thought = { sessionUpdate: 'agent_thought_chunk', messageId: 'SECRET_MESSAGE_ID_CANARY',
  content: { type: 'text', text: 'SECRET_REASONING_CANARY', annotations: {
    audience: ['assistant'], priority: 0.5, lastModified: 'SECRET_ANNOTATION_CANARY',
    _meta: { private: 'SECRET_ANNOTATION_META_CANARY' } }, _meta: { private: 'SECRET_CONTENT_META_CANARY' } },
  _meta: { private: 'SECRET_THOUGHT_META_CANARY' } };
const plan = { sessionUpdate: 'plan', entries: ['high', 'medium', 'low'].map((priority, index) => ({
  content: 'SECRET_PLAN_CANARY $(touch MUST_NOT_EXIST)', priority, status: ['pending', 'in_progress', 'completed'][index],
  _meta: { private: 'SECRET_ENTRY_META_CANARY' },
})), _meta: { private: 'SECRET_PLAN_META_CANARY' } };
const privateUpdate = mode => mode.startsWith('thought-') ? thought : plan;
process.on('SIGTERM', () => { record({ stopped: process.pid }); process.exit(0); });
for await (const line of createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  record({ method: request.method });
  if (!Object.hasOwn(request, 'method')) {
    record({ response: request });
    if (permissionTurn && /^permission-[01]$/.test(request.id) && request.result?.outcome?.outcome === 'selected') {
      if (request.result.outcome.optionId === 'once') appendFileSync(`${log}.executed`, 'executed\n');
      if (--permissionTurn.remaining === 0) {
        send({ id: permissionTurn.id, result: { stopReason: 'end_turn' } });
        permissionTurn = undefined;
      }
    }
    continue;
  }
  if (request.method === 'initialize') {
    if (initialized || request.params.protocolVersion !== 1 || Object.keys(request.params.clientCapabilities).length) process.exit(21);
    initialized = true;
    if (/^(thought|plan)-early-/.test(process.argv[3] ?? '')) {
      record({ early: process.argv[3], earlyPid: process.pid });
      if (process.argv[3].endsWith('initialize')) send({ method: 'session/update',
        params: { sessionId: 'connection-local', update: privateUpdate(process.argv[3]) } });
    }
    if (process.argv[3]?.startsWith('tool-early-')) record({ early: process.argv[3], earlyPid: process.pid });
    if (process.argv[3] === 'tool-early-initialize') send({ method: 'session/update',
      params: { sessionId: 'connection-local', update: tool } });
    send({ id: request.id, result: { protocolVersion: 1, agentCapabilities: {}, authMethods: [],
      agentInfo: { name: 'UNTRUSTED_IDENTITY_CANARY', version: 'UNTRUSTED_VERSION_CANARY' } } });
  } else if (request.method === 'session/new') {
    if (!initialized || session || request.params.cwd !== process.cwd() || request.params.mcpServers.length) process.exit(22);
    session = true;
    if (/^(thought|plan)-early-session$/.test(process.argv[3] ?? '')) send({ method: 'session/update',
      params: { sessionId: 'connection-local', update: privateUpdate(process.argv[3]) } });
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
    if (mode.startsWith('permission-')) {
      permissionTurn = { id: request.id, remaining: mode === 'permission-two' ? 2 : 1 };
      const params = {
        sessionId: mode === 'permission-session' ? 'wrong-session' : 'connection-local',
        toolCall: { toolCallId: 'PRIVATE_PERMISSION_TOOL', title: 'PRIVATE_PERMISSION_TITLE\u009b31m\u202e',
          kind: 'execute', rawInput: { command: 'PRIVATE_PERMISSION_COMMAND' } },
        options: [
          { optionId: 'once', name: 'Allow once', kind: 'allow_once' },
          { optionId: 'no', name: 'Reject once', kind: 'reject_once' },
          { optionId: 'always', name: 'Allow always', kind: 'allow_always' },
        ],
      };
      if (mode === 'permission-malformed') params.options[1].optionId = 'once';
      if (mode === 'permission-shape') params.toolCall.kind = 1;
      if (mode === 'permission-oversize') params.toolCall.rawInput = 'x'.repeat(32_768);
      if (mode === 'permission-options') params.options = [params.options[2]];
      const permission = (index = 0) => ({ id: `permission-${index}`, method: 'session/request_permission',
        params: { ...params, toolCall: { ...params.toolCall, toolCallId: `PRIVATE_PERMISSION_TOOL_${index}` } } });
      if (mode === 'permission-late') {
        process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } }) + '\n' +
          JSON.stringify({ jsonrpc: '2.0', ...permission() }) + '\n');
      } else if (mode === 'permission-early-stop') {
        process.stdout.write(JSON.stringify({ jsonrpc: '2.0', ...permission() }) + '\n' +
          JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } }) + '\n');
      } else {
        send(permission());
        if (mode === 'permission-two') send(permission(1));
        if (mode === 'permission-duplicate') send(permission());
        if (mode === 'permission-tool-duplicate') send({ ...permission(), id: 'other-request' });
        if (mode === 'permission-overflow') for (let i = 1; i < 9; i++) send(permission(i));
        // Demonstrate the reader still handles nonpermission traffic while waiting.
        update({ sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: 'PRIVATE_PERMISSION_THOUGHT' } });
      }
      continue;
    }
    if (['cancel', 'concurrent'].includes(mode)) {
      update(plan); update(thought);
    }
    if (mode === 'private') {
      // One write: private updates before the response must survive deferred delivery.
      const updates = [{ sessionUpdate: 'available_commands_update', availableCommands: [
        { name: 'SECRET_COMMAND_CANARY', description: 'SECRET_DESCRIPTION_CANARY', input: null },
        { name: 'SECRET_COMMAND_CANARY', description: '', input: { hint: 'SECRET_HINT_CANARY' } },
      ] }, { sessionUpdate: 'usage_update', used: 10, size: 200000, cost: null },
      { sessionUpdate: 'usage_update', used: 20, size: 200000,
        cost: { amount: 0.00036, currency: 'SECRET_CURRENCY_CANARY' }, _meta: { model: 'SECRET_MODEL_CANARY' } },
      { sessionUpdate: 'session_info_update', title: 'SECRET_TITLE_CANARY', updatedAt: null,
        _meta: { private: 'SECRET_SESSION_META_CANARY' } },
      plan, thought, tool, { ...plan, entries: [] }, { ...text, messageId: 'SECRET_MESSAGE_ID_CANARY',
        content: { ...text.content, annotations: { audience: ['assistant'], priority: 0, lastModified: '' } } },
        { ...delta, status: 'completed' }, { ...thought, messageId: null, _meta: null },
        { ...plan, entries: [{ content: '', priority: 'low', status: 'completed', _meta: null }], _meta: null }];
      process.stdout.write(updates.map(update => JSON.stringify({ jsonrpc: '2.0', method: 'session/update',
        params: { sessionId: 'connection-local', update, _meta: { private: 'SECRET_PARAMS_META_CANARY' } } }) + '\n').join('') +
        JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn',
          usage: { totalTokens: 20, inputTokens: 10, outputTokens: 10, thoughtTokens: null,
            cachedReadTokens: 0, cachedWriteTokens: 0, _meta: { private: 'SECRET_USAGE_META_CANARY' } } } }) + '\n');
      continue;
    }
    if (mode.startsWith('usage-invalid-')) {
      send({ id: request.id, result: { stopReason: 'end_turn', usage: JSON.parse(prompt.slice(prompt.indexOf('\n') + 1)) } });
      continue;
    }
    if (mode.startsWith('private-')) {
      if (mode.startsWith('private-invalid-')) update(JSON.parse(prompt.slice(prompt.indexOf('\n') + 1)));
      else {
        update(plan); update(thought);
        if (mode === 'private-many') {
          for (let i = 0; i < 80; i++) {
            update({ ...plan, entries: Array.from({ length: 100 }, () => plan.entries[i % 3]) });
            update({ ...thought, content: { type: 'text', text: 'SECRET_LARGE_REASONING_CANARY'.repeat(3000) } });
            await delay(5);
          }
          for (const content of [
            { type: 'image', data: 'SECRET_IMAGE_CANARY', mimeType: 'image/png', uri: null },
            { type: 'audio', data: 'SECRET_AUDIO_CANARY', mimeType: 'audio/wav' },
            { type: 'resource_link', uri: 'SECRET_URI_CANARY', name: 'SECRET_NAME_CANARY', size: 0 },
            { type: 'resource', resource: { uri: 'SECRET_URI_CANARY', text: 'SECRET_RESOURCE_CANARY' } },
            { type: 'resource', resource: { uri: 'SECRET_URI_CANARY', blob: 'SECRET_BLOB_CANARY' } },
          ]) update({ ...thought, content });
        }
      }
      if (mode === 'private-failure') send({ id: request.id, error: { code: -32000, message: 'SECRET_ERROR_CANARY' } });
      else send({ id: request.id, result: { stopReason: {
        'private-cancelled': 'cancelled', 'private-incomplete': 'max_tokens', 'private-stop': 'invented-success',
      }[mode] ?? 'end_turn' } });
      continue;
    }
    if (/^(thought|plan)-(mismatch|late|oversize)$/.test(mode)) {
      const value = privateUpdate(mode);
      if (mode.endsWith('mismatch')) update(value, 'SECRET_OTHER_SESSION_CANARY');
      else if (mode.endsWith('oversize')) update(mode.startsWith('thought-') ?
        { ...thought, content: { type: 'text', text: 'x'.repeat(1_048_576) } } :
        { ...plan, entries: [{ ...plan.entries[0], content: 'x'.repeat(1_048_576) }] });
      else {
        process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } }) + '\n' +
          JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 'connection-local', update: value } }) + '\n');
        continue;
      }
      send({ id: request.id, result: { stopReason: 'end_turn' } });
      continue;
    }
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
