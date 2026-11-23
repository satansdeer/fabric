// Reviewed provider boundary. Core and Connect see only the existing generic contract.
import { createHash, createHmac, timingSafeEqual } from 'node:crypto';
import { constants, openSync, fstatSync, readSync, closeSync, realpathSync, createReadStream, mkdirSync, lstatSync, rmdirSync } from 'node:fs';
import { isAbsolute, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { exact, object } from '../../sdk/typescript/protocol.mjs';
import { validConnectorInput } from '../../sdk/typescript/webhook.mjs';

export const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const ID = /^[A-Za-z0-9_.-]{1,128}$/;
const HEX = /^[a-f0-9]{64}$/;
const LOGIN = /^[A-Za-z0-9][A-Za-z0-9-]{0,38}$/;
const positive = value => Number.isSafeInteger(value) && value > 0;
const text = (value, max) => typeof value === 'string' && Buffer.byteLength(value) <= max &&
  ![...value].some(c => c.codePointAt(0) >= 0xd800 && c.codePointAt(0) <= 0xdfff);
export function requireThat(condition) { if (!condition) throw new Error('github_connector_rejected'); }
export function privateBytes(path, max = 65536, min = 1) {
  requireThat(isAbsolute(path) && realpathSync(path) === path);
  const fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const stat = fstatSync(fd);
    requireThat(stat.isFile() && stat.uid === process.getuid() && stat.nlink === 1 && !(stat.mode & 0o077) && stat.size >= min && stat.size <= max);
    const bytes = Buffer.alloc(stat.size + 1);
    let size = 0, count;
    while (size < bytes.length && (count = readSync(fd, bytes, size, bytes.length - size, null))) size += count;
    requireThat(size === stat.size);
    return bytes.subarray(0, size);
  } finally { closeSync(fd); }
}
export const privateJSON = path => JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(privateBytes(path)));
export function origin(value, loopback) {
  const url = new URL(value);
  requireThat(url.origin === value && !url.username && !url.password && (url.protocol === 'https:' ||
    loopback && url.protocol === 'http:' && ['127.0.0.1', '[::1]'].includes(url.hostname)));
  return value;
}
function actor(value, bot = false) {
  return exact(value, ['id', 'login']) && positive(value.id) && typeof value.login === 'string' &&
    (LOGIN.test(value.login) || bot && /^[A-Za-z0-9][A-Za-z0-9-]{0,38}\[bot\]$/.test(value.login));
}
export function loadConfig(path) {
  const c = privateJSON(path);
  requireThat(exact(c, ['version', 'binding', 'repo', 'repository_id', 'installation_id', 'author', 'sender', 'label',
    'actions', 'approvals', 'webhook_secret_ref', 'connect', 'local', 'response']) && c.version === 1);
  requireThat(exact(c.binding, ['source_id', 'tenant_id', 'scope', 'route']) && Object.values(c.binding).every(v => typeof v === 'string' && ID.test(v)));
  requireThat(['tenant_id', 'scope', 'route'].every(k => /^[a-z0-9][a-z0-9_-]{0,62}$/.test(c.binding[k])));
  requireThat(typeof c.repo === 'string' && c.repo.split('/').length === 2 && c.repo.split('/').every(v => /^[A-Za-z0-9_.-]{1,100}$/.test(v) && !['.', '..'].includes(v)));
  requireThat(positive(c.repository_id) && positive(c.installation_id) && actor(c.author) && actor(c.sender) && text(c.label, 100) && c.label.length > 0);
  requireThat(Array.isArray(c.actions) && c.actions.length > 0 && new Set(c.actions).size === c.actions.length &&
    c.actions.every(v => ['opened', 'edited', 'reopened', 'labeled'].includes(v)));
  requireThat(Array.isArray(c.approvals) && c.approvals.length <= 100 && c.approvals.every(a =>
    exact(a, ['issue_number', 'sha256', 'timestamp']) && positive(a.issue_number) && a.issue_number <= 2147483647 &&
    HEX.test(a.sha256) && positive(a.timestamp)) && new Set(c.approvals.map(a => `${a.issue_number}:${a.sha256}`)).size === c.approvals.length);
  requireThat(exact(c.connect, ['endpoint', 'secret_ref', 'connection_ref', 'allow_loopback_http']) && typeof c.connect.allow_loopback_http === 'boolean');
  origin(c.connect.endpoint, c.connect.allow_loopback_http);
  requireThat(exact(c.local, ['fabric', 'root']) && exact(c.response, ['api_origin', 'token_ref', 'actor', 'allow_loopback_http']) &&
    typeof c.response.allow_loopback_http === 'boolean' && actor(c.response.actor, true));
  origin(c.response.api_origin, c.response.allow_loopback_http);
  // Production credentials cannot be sent to an arbitrary HTTPS origin.
  requireThat(c.response.api_origin === 'https://api.github.com' || c.response.allow_loopback_http && c.response.api_origin.startsWith('http://'));
  for (const p of [c.webhook_secret_ref, c.connect.secret_ref, c.connect.connection_ref, c.local.fabric, c.local.root, c.response.token_ref])
    requireThat(typeof p === 'string' && isAbsolute(p) && p.length <= 4096);
  return c;
}

// Match #16's Python ensure_ascii/sort_keys/separators digest, including non-ASCII issue text.
export function canonical(value) {
  const sort = v => Array.isArray(v) ? v.map(sort) : object(v) ?
    Object.fromEntries(Object.keys(v).sort().map(k => [k, sort(v[k])])) : v;
  return JSON.stringify(sort(value)).replace(/[\u007f-\uffff]/g, c => '\\u' + c.charCodeAt(0).toString(16).padStart(4, '0'));
}
function snapshot(c, request, approved) {
  requireThat(exact(request, ['body', 'timestamp', 'expires_at', 'nonce', 'authorization']) && text(request.body, 8192));
  const wire = JSON.parse(request.body);
  requireThat(exact(wire, ['event', 'delivery', 'raw']) && wire.event === 'issues' &&
    typeof wire.delivery === 'string' && /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/i.test(wire.delivery) && text(wire.raw, 8192));
  const body = JSON.parse(wire.raw), issue = body?.issue;
  requireThat(object(body) && c.actions.includes(body.action) && body.repository?.full_name === c.repo && body.repository?.id === c.repository_id &&
    body.installation?.id === c.installation_id && body.sender?.id === c.sender.id && body.sender?.login === c.sender.login &&
    issue?.user?.id === c.author.id && issue.user.login === c.author.login);
  // Provider fields never carry worker, routing, model, source or permission authority.
  requireThat(!['worker', 'model', 'harness_id', 'prompt', 'source_ref', 'execution_context', 'limits', 'automation', 'route', 'scope', 'tenant_id', 'source_id']
    .some(k => Object.hasOwn(body, k)));
  requireThat(positive(issue.number) && issue.number <= 2147483647 && issue.state === 'open' && !Object.hasOwn(issue, 'pull_request') &&
    issue.html_url === `https://github.com/${c.repo}/issues/${issue.number}` && Array.isArray(issue.labels) && issue.labels.length <= 100 &&
    issue.labels.every(l => object(l) && text(l.name, 100)) && issue.labels.some(l => l.name === c.label) &&
    (body.action !== 'labeled' || body.label?.name === c.label) && text(issue.title, 1024) && issue.title.length > 0 &&
    (issue.body === null || text(issue.body, 65536)));
  const data = { repo: c.repo, issue_number: issue.number, issue_url: issue.html_url, author: issue.user.login, title: issue.title, body: issue.body ?? '' };
  data.approval_sha256 = sha256(canonical(Object.fromEntries(['repo', 'issue_number', 'author', 'title', 'body'].map(k => [k, data[k]]))));
  const approval = c.approvals.find(a => a.issue_number === issue.number && a.sha256 === data.approval_sha256);
  requireThat(!approved || approval);
  return { data, approval, wire };
}
export function inspect(c, request, secretRef, approved = true) {
  const result = snapshot(c, request, approved), signature = request.authorization;
  requireThat(typeof signature === 'string' && /^sha256=[a-f0-9]{64}$/.test(signature));
  const expected = createHmac('sha256', privateBytes(secretRef, 256, 32)).update(Buffer.from(result.wire.raw, 'utf8')).digest();
  requireThat(timingSafeEqual(expected, Buffer.from(signature.slice(7), 'hex')));
  return result;
}
export function normalize(c, request, secretRef = c.webhook_secret_ref) {
  const { data } = inspect(c, request, secretRef);
  const id = sha256(JSON.stringify([c.repo, c.repository_id, c.installation_id, data.issue_number, data.approval_sha256]));
  return { version: 1, ...c.binding, event_id: id, idempotency_key: id, data: { issue: data } };
}
export function verify(c, input) {
  try {
    requireThat(validConnectorInput(input) && input.policy.auth === 'hmac-sha256' &&
      Object.entries(c.binding).every(([k, v]) => input.policy[k] === v));
    inspect(c, input.request, input.policy.secret_ref);
    return { verified: true };
  } catch { return { verified: false }; }
}
export function genericBody(c, trigger) {
  const issue = trigger.data.issue;
  const approval = c.approvals.find(a => a.issue_number === issue.issue_number && a.sha256 === issue.approval_sha256);
  requireThat(approval);
  // Approval time, not receipt time: redelivery/reordering serializes identical generic bytes.
  const body = JSON.stringify({ version: 1, event_id: trigger.event_id, timestamp: approval.timestamp, data: trigger.data });
  requireThat(Buffer.byteLength(body) <= 8192);
  return body;
}
export const routePath = c => `/v1/projects/${c.binding.tenant_id}/${c.binding.scope}/automations/${c.binding.route}`;
export async function httpJSON(url, { method = 'GET', token, body, headers = {} } = {}) {
  const response = await fetch(url, { method, headers: { accept: 'application/vnd.github+json', 'x-github-api-version': '2022-11-28',
    'user-agent': 'fabric-github-connector', ...(token ? { authorization: `Bearer ${token}` } : {}),
    ...(body !== undefined ? { 'content-type': 'application/json' } : {}), ...headers },
    body, redirect: 'error', signal: AbortSignal.timeout(15000) });
  const chunks = []; let length = 0;
  for await (const chunk of response.body) { length += chunk.length; requireThat(length <= 1024 * 1024); chunks.push(chunk); }
  requireThat(response.ok);
  return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks, length)));
}
function token(path) {
  const result = privateBytes(path, 512, 32).toString('utf8');
  requireThat(/^[!-~]{32,512}$/.test(result));
  return result;
}
export async function forward(c, trigger) {
  const body = genericBody(c, trigger), namespace = `${c.binding.tenant_id}/${c.binding.scope}/${c.binding.route}`;
  const key = token(c.connect.secret_ref);
  requireThat(HEX.test(key));
  const signature = createHmac('sha256', key).update(namespace + '\n').update(body).digest('hex');
  const result = await httpJSON(c.connect.endpoint + routePath(c) + '/webhook', {
    method: 'POST', body, headers: { 'x-fabric-signature-256': 'sha256=' + signature } });
  requireThat(result.event_id === sha256(trigger.event_id) && ['queued', 'delivering', 'running', 'completed', 'failed', 'expired'].includes(result.status));
  return result;
}
function cli(c, ...args) {
  requireThat(realpathSync(c.local.fabric) === c.local.fabric && realpathSync(c.local.root) === c.local.root);
  const p = spawnSync(c.local.fabric, ['--root', c.local.root, ...args, '--json'], {
    env: { PATH: '/usr/bin:/bin', LC_ALL: 'C' }, encoding: 'utf8', timeout: 30000, maxBuffer: 4 * 1024 * 1024, stdio: ['ignore', 'pipe', 'ignore'] });
  requireThat(p.status === 0 && !p.error);
  return JSON.parse(p.stdout);
}
async function artifact(c, a) {
  requireThat(object(a) && typeof a.relative_path === 'string' && !isAbsolute(a.relative_path) &&
    a.relative_path.split('/').every(p => p && p !== '.' && p !== '..') && HEX.test(a.sha256) &&
    Number.isSafeInteger(a.size_bytes) && a.size_bytes >= 0 && a.size_bytes <= 134217728);
  const path = join(c.local.root, a.relative_path);
  requireThat(realpathSync(path) === path && resolve(path).startsWith(c.local.root + '/'));
  const fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const stat = fstatSync(fd); requireThat(stat.isFile() && stat.size === a.size_bytes);
    const hash = createHash('sha256'); let size = 0;
    for await (const chunk of createReadStream(null, { fd, autoClose: false })) { size += chunk.length; requireThat(size <= a.size_bytes); hash.update(chunk); }
    requireThat(size === a.size_bytes && hash.digest('hex') === a.sha256);
  } finally { closeSync(fd); }
}
async function localMapping(c, trigger, receipt) {
  // Read existing durable admission, never call admission from a response hook.
  const root = lstatSync(c.local.root), path = join(c.local.root, 'fabric.db'), stat = lstatSync(path);
  requireThat(root.isDirectory() && root.uid === process.getuid() && !(root.mode & 0o077) &&
    realpathSync(path) === path && stat.isFile() && stat.uid === process.getuid() && stat.nlink === 1);
  const { DatabaseSync } = await import('node:sqlite');
  const database = new DatabaseSync(path, { readOnly: true });
  try {
    const p = c.binding, namespace = sha256(JSON.stringify([p.source_id, p.tenant_id, p.scope, p.route]));
    const rows = database.prepare('SELECT a.batch_id,t.run_id FROM async_batches a JOIN batch_tasks t USING(batch_id) WHERE a.idempotency_key=?')
      .all(`connect:${namespace}:${trigger.event_id}`);
    requireThat(rows.length === 1 && rows[0].batch_id === receipt.batch_id && rows[0].run_id === receipt.run_id);
  } finally { database.close(); }
}
export async function subscribe(c, data) {
  requireThat(exact(data, []));
  // This endpoint accepts installation access tokens, not an App JWT or an operator PAT.
  const result = await httpJSON(c.response.api_origin + '/installation/repositories?per_page=100', { token: token(c.response.token_ref) });
  requireThat(result.total_count === 1 && Array.isArray(result.repositories) && result.repositories.length === 1 &&
    result.repositories[0].id === c.repository_id && result.repositories[0].full_name === c.repo);
  return { repository: c.repo, repository_id: c.repository_id, token_repository_scope_verified: true,
    configured_installation_id: c.installation_id, required_events: ['issues'],
    required_permissions: { issues: 'write', metadata: 'read' }, webhook_path: '/github' };
}
export async function reply(c, data) {
  // The caller supplies a signed original delivery, never proposed batch/run/result/comment content.
  requireThat(exact(data, ['request']));
  const trigger = normalize(c, data.request), id = sha256(trigger.event_id);
  requireThat(realpathSync(c.local.root) === c.local.root);
  const locks = join(c.local.root, 'github-responses');
  try { mkdirSync(locks, { mode: 0o700 }); } catch (e) { if (e.code !== 'EEXIST') throw e; }
  const stat = lstatSync(locks);
  requireThat(stat.isDirectory() && !stat.isSymbolicLink() && stat.uid === process.getuid() && !(stat.mode & 0o077));
  const lock = join(locks, id + '.lock');
  mkdirSync(lock, { mode: 0o700 }); // Fail closed on concurrent calls or an interrupted response.
  try {
  const connection = privateJSON(c.connect.connection_ref);
  requireThat(exact(connection, ['schema_version', 'endpoint', 'identity', 'worker_id', 'token']) && connection.schema_version === 2 &&
    connection.endpoint === c.connect.endpoint && exact(connection.identity, ['account', 'project', 'automation']) &&
    connection.identity.account === c.binding.tenant_id && connection.identity.project === c.binding.scope && connection.identity.automation === c.binding.route &&
    typeof connection.token === 'string' && /^[!-~]{32,512}$/.test(connection.token));
  const status = await httpJSON(c.connect.endpoint + routePath(c) + '/status', { token: connection.token });
  requireThat(status.owner === c.binding.tenant_id && status.project === c.binding.scope && status.automation === c.binding.route &&
    status.policy?.kind === 'generic' && Object.entries(c.binding).every(([k, v]) => status.policy[k] === v));
  const receipt = status.events?.find(e => e.event_id === id);
  requireThat(receipt && ['completed', 'failed'].includes(receipt.status) && receipt.source === 'generic' && typeof receipt.trigger_json === 'string');
  const expected = { ...trigger, event_id: id, idempotency_key: id };
  requireThat(canonical(JSON.parse(receipt.trigger_json)) === canonical(expected));
  const handle = v => typeof v === 'string' && /^[A-Za-z0-9][A-Za-z0-9_.-]{0,199}$/.test(v);
  requireThat(handle(receipt.batch_id) && handle(receipt.run_id) && handle(receipt.artifact_manifest_id));
  await localMapping(c, expected, receipt);
  const results = cli(c, 'results', receipt.batch_id);
  requireThat(results.batch_id === receipt.batch_id && results.terminal === true && typeof results.succeeded === 'boolean' &&
    Array.isArray(results.tasks) && results.tasks.length === 1);
  const task = results.tasks[0];
  requireThat(task.run_id === receipt.run_id && task.result?.run_id === receipt.run_id &&
    task.result.artifact_manifest_id === receipt.artifact_manifest_id && (results.succeeded ? receipt.status === 'completed' && task.state === 'SUCCEEDED' : receipt.status === 'failed') &&
    Array.isArray(task.artifacts) && task.artifacts.length > 0);
  requireThat(task.artifacts.reduce((sum, a) => sum + a.size_bytes, 0) <= 134217728);
  for (const a of task.artifacts) await artifact(c, a);
  await subscribe(c, {}); // Outbound token must itself be restricted to this one repository.
  const apiToken = token(c.response.token_ref), issue = trigger.data.issue;
  const current = await httpJSON(c.response.api_origin + `/repos/${c.repo}/issues/${issue.issue_number}`, { token: apiToken });
  requireThat(current.number === issue.issue_number && current.html_url === issue.issue_url && !current.pull_request &&
    current.user?.id === c.author.id && current.user?.login === c.author.login);
  const marker = `<!-- fabric-github:${id} -->`;
  const body = `${marker}\nFabric evidence for ${c.repo}#${issue.issue_number}.\nBatch: \`${receipt.batch_id}\`; run: \`${receipt.run_id}\`.\nTerminal: true; succeeded: ${results.succeeded}.\nRetained artifact manifest: \`${receipt.artifact_manifest_id}\`.\nWorker outcome is not proof of checks passing. Human review/PR required; no automatic merge.`;
  const endpoint = c.response.api_origin + `/repos/${c.repo}/issues/${issue.issue_number}/comments`;
  let existing;
  // Bounded discovery also reconciles a POST whose successful response was lost. Never blind-retry a write.
  for (let page = 1; page <= 10; page++) {
    const comments = await httpJSON(endpoint + `?per_page=100&page=${page}`, { token: apiToken });
    requireThat(Array.isArray(comments) && comments.length <= 100);
    for (const comment of comments) if (comment.body?.startsWith(marker)) {
      requireThat(comment.user?.id === c.response.actor.id && comment.user?.login === c.response.actor.login && !existing && positive(comment.id));
      existing = comment;
    }
    if (comments.length < 100) break;
    requireThat(page < 10);
  }
  if (existing?.body === body) return { posted: true, duplicate: true, comment_id: existing.id, batch_id: receipt.batch_id, run_id: receipt.run_id, artifact_manifest_id: receipt.artifact_manifest_id };
  const posted = await httpJSON(existing ? c.response.api_origin + `/repos/${c.repo}/issues/comments/${existing.id}` : endpoint,
    { method: existing ? 'PATCH' : 'POST', token: apiToken, body: JSON.stringify({ body }) });
  requireThat(positive(posted.id) && posted.body === body && posted.user?.id === c.response.actor.id && posted.user?.login === c.response.actor.login);
  return { posted: true, duplicate: false, comment_id: posted.id, batch_id: receipt.batch_id, run_id: receipt.run_id, artifact_manifest_id: receipt.artifact_manifest_id };
  } finally { rmdirSync(lock); }
}
