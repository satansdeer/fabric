// Bun runner. No downloads, builds, inherited credentials, or provider fallback.
// Usage: bun integrations/cline/scripted-provider-smoke.mjs /absolute/private-inputs.json
import assert from "node:assert/strict";
import { constants } from "node:fs";
import { access, mkdir, mkdtemp, readFile, readdir, realpath, rm, rmdir, stat, symlink, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, join, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { Database } from "bun:sqlite";
import { startScriptedProvider } from "../../scripts/scripted-provider.mjs";
import { reference, install } from "../../sdk/typescript/cli.mjs";
import { directory } from "../../sdk/typescript/package.mjs";
import { fileHash, sha256, VERSION } from "../../plugins/acp/config.mjs";
import { launchAcp } from "../../plugins/acp/transport.mjs";
import { nativeEnvironment } from "./fixture-launcher.mjs";

assert.equal(process.argv.length, 3, "Expected private inputs JSON");
assert.equal(process.platform, "darwin");
assert.ok(["arm64", "x64"].includes(process.arch));
const source = await realpath(join(dirname(fileURLToPath(import.meta.url)), "../.."));
const input = process.argv[2];
assert.ok(isAbsolute(input) && await realpath(input) === input);
const inputInfo = await stat(input);
assert.ok(inputInfo.isFile() && inputInfo.size <= 32768 && inputInfo.uid === process.getuid() && !(inputInfo.mode & 0o077));
const inputs = JSON.parse(await readFile(input, "utf8"));
const names = ["bun", "fabric", "node", "cline", "localPlugin", "bootstrap"];
assert.deepEqual(Object.keys(inputs).sort(), [...names, "clineVersion"].sort());
assert.equal(inputs.clineVersion, "3.0.75");
for (const name of names) {
  const pin = inputs[name];
  assert.deepEqual(Object.keys(pin).sort(), ["path", "sha256"]);
  assert.match(pin.sha256, /^[a-f0-9]{64}$/);
  assert.equal(fileHash(pin.path), pin.sha256, `${name}: pin mismatch`);
  if (name !== "bootstrap") {
    await access(pin.path, constants.X_OK);
    assert.notEqual(await Bun.file(pin.path).slice(0, 2).text(), "#!", `${name}: native executable required`);
  }
}
assert.equal(inputs.bun.path, await realpath(process.execPath));
assert.equal(inputs.bootstrap.path, await realpath(join(source, "integrations/cline/fixture-launcher.mjs")));
process.umask(0o077);
const fixture = await realpath(await mkdtemp("/private/tmp/fabric-cline-"));
// Short, canonical, private, outside all execution workspaces (macOS socket bound).
const permissions = await realpath(await mkdtemp("/private/tmp/fcp-"));
const root = join(fixture, "storage"), repo = join(fixture, "source"), state = join(fixture, "native");
const home = join(fixture, "home"), registry = join(fixture, "registry"), bin = join(fixture, "bin");
for (const path of [repo, home, registry, bin, join(home, "tmp"), state,
  ...["home", "tmp", "cline", "data/settings"].map(path => join(state, path))]) await mkdir(path, { recursive: true, mode: 0o700 });
await symlink(inputs.node.path, join(bin, "node"));
const env = { HOME: home, USERPROFILE: home, TMPDIR: join(home, "tmp"), LC_ALL: "C", PATH: `${bin}:/usr/bin:/bin`,
  XDG_CONFIG_HOME: home, XDG_DATA_HOME: home, XDG_CACHE_HOME: home, XDG_STATE_HOME: home,
  FABRIC_PLUGIN_REGISTRY: registry, FABRIC_ACP_HOME: join(state, "home"),
  GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0", OTEL_SDK_DISABLED: "true",
  GIT_AUTHOR_NAME: "Fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid", GIT_COMMITTER_NAME: "Fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid" };
const model = "fabric-test", key = "fabric-dummy", target = "fixture-marker.txt", original = "fixture baseline\n";
const complete = "FABRIC_CLINE_COMPLETE", held = "FABRIC_CLINE_HELD";
const route = "/v1/chat/completions";
const jsonl = value => value.toString().split("\n").filter(line => line.trim()).map(line => JSON.parse(line));
const pause = () => new Promise(resolve => setTimeout(resolve, 25));
const batches = [], peers = new Set(), proof = [], errors = [], correlations = [];
let server, hub, prepared, nodeVersion, nativeVersion, previousDecision, phase = "setup", sequence = 0;
let managerStopped = false, hubStopped = false, stateCleaned = false, permissionsCleaned = false;
async function run(program, args, cwd = fixture, seconds = 60, environment = env) {
  const prefix = join(fixture, `command-${++sequence}`);
  const child = Bun.spawn([program, ...args], { cwd, env: environment, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  let expired = false;
  const timer = setTimeout(() => { expired = true; child.kill("SIGKILL"); }, seconds * 1000);
  async function bounded(stream) {
    const chunks = []; let size = 0;
    for await (const bytes of stream) {
      size += bytes.length;
      if (size > 2097152) { child.kill("SIGKILL"); throw new Error("Command output ceiling"); }
      chunks.push(bytes);
    }
    return Buffer.concat(chunks).toString();
  }
  try {
    const [stdout, stderr, code] = await Promise.all([bounded(child.stdout), bounded(child.stderr), child.exited]);
    await writeFile(`${prefix}.json`, JSON.stringify({ program, args, code, expired }));
    await writeFile(`${prefix}.stdout`, stdout); await writeFile(`${prefix}.stderr`, stderr);
    assert.ok(!expired && code === 0, `Command failed; private evidence ${prefix}`);
    return stdout;
  } finally { clearTimeout(timer); child.kill(); await child.exited; }
}
// The permission client is Node software; Bun 1.3.5 cannot realpath macOS sockets.
async function permissionControl(endpoint, message) {
  const module = pathToFileURL(join(source, "plugins/acp/permission.mjs")).href;
  return JSON.parse(await run(inputs.node.path, ["--input-type=module", "-e",
    `import { permissionControl } from ${JSON.stringify(module)}; console.log(JSON.stringify(await permissionControl(process.argv[1], JSON.parse(process.argv[2]))));`,
    endpoint, JSON.stringify(message)]));
}
const cli = async args => JSON.parse(await run(inputs.fabric.path, ["--root", root, ...args], repo, args[0] === "wait" ? 105 : 60));
const native = args => run(inputs.cline.path, args, repo, 45, nativeEnvironment(state, hub?.port ?? 0));
async function until(check, label, milliseconds = 45000) {
  const deadline = Date.now() + milliseconds;
  while (Date.now() < deadline) { const value = await check(); if (value) return value; await pause(); }
  throw new Error(`Observation deadline: ${label}`); // Failure only, never a permission decision.
}
function chunk(delta, finish_reason = null) {
  return `data: ${JSON.stringify({ id: "chatcmpl-fabric", object: "chat.completion.chunk", created: 1, model,
    choices: [{ index: 0, delta, finish_reason }] })}\n\n`;
}
function textWire(text, terminal = true) {
  return chunk({ role: "assistant", content: "" }) + chunk({ content: text.slice(0, 9) }) + chunk({ content: text.slice(9) }) +
    (terminal ? chunk({}, "stop") + "data: [DONE]\n\n" : "");
}
function commandFor(name) {
  return `printf '%s\\n' '${name}' >> ${target}; printf '%s\\n' '${name}'`;
}
function toolWire(name) {
  const args = JSON.stringify({ commands: [commandFor(name)] }), middle = Math.floor(args.length / 2);
  return chunk({ role: "assistant", tool_calls: [{ index: 0, id: `call_${name}`, type: "function",
    function: { name: "run_commands", arguments: args.slice(0, middle) } }] }) +
    chunk({ tool_calls: [{ index: 0, function: { arguments: args.slice(middle) } }] }) +
    chunk({}, "tool_calls") + "data: [DONE]\n\n";
}
const textContent = content => typeof content === "string" ? content : Array.isArray(content) ?
  content.filter(block => block?.type === "text").map(block => block.text).join("\n") : "";
function matches(prompt, tool) {
  return ({ body }) => Array.isArray(body?.messages) && textContent(body.messages.findLast(message => message?.role === "user")?.content) === prompt &&
    body.messages.filter(message => message?.role === "tool").length === (tool ? 1 : 0);
}
function step(prompt, wire, { tool, denied = false, wait, status = 200, onRequest } = {}) {
  return { method: "POST", path: route, match: matches(prompt, tool), status,
    headers: { "content-type": status === 200 ? "text/event-stream" : "application/json", connection: "close" },
    chunks: [wire.slice(0, 7), wire.slice(7, 51), wire.slice(51)], ...(wait ? { wait } : {}),
    async check(request) {
      assert.equal(request.method, "POST"); assert.equal(request.path, route);
      assert.equal(request.headers.authorization, `Bearer ${key}`);
      for (const header of ["cookie", "proxy-authorization", "x-api-key", "api-key"]) assert.ok(!request.headers[header]);
      const { body } = request;
      assert.equal(body.model, model); assert.equal(body.stream, true);
      assert.ok(matches(prompt, tool)(request));
      const advertised = body.tools?.filter(item => item.type === "function" && item.function?.name === "run_commands");
      assert.equal(advertised?.length, 1, "Actual native run_commands schema required");
      const schema = advertised[0].function.parameters;
      assert.equal(schema.type, "object"); assert.equal(schema.additionalProperties, false);
      assert.deepEqual(schema.required, ["commands"]);
      assert.deepEqual(Object.keys(schema.properties), ["commands"]);
      assert.equal(schema.properties.commands.type, "array"); assert.equal(schema.properties.commands.minItems, 1);
      assert.equal(schema.properties.commands.items.type, "string");
      const calls = body.messages.flatMap(message => message.tool_calls ?? []);
      if (tool) {
        assert.equal(calls.length, 1); assert.equal(calls[0].id, `call_${tool}`);
        assert.equal(calls[0].type, "function"); assert.equal(calls[0].function.name, "run_commands");
        assert.deepEqual(JSON.parse(calls[0].function.arguments), { commands: [commandFor(tool)] });
        const result = body.messages.find(message => message.role === "tool");
        assert.equal(result.tool_call_id, `call_${tool}`);
        const content = textContent(result.content);
        assert.ok(denied ? /User rejected the tool call/.test(content) : content.includes(tool), "Missing actual tool result");
        correlations.push({ call_id: calls[0].id, result_id: result.tool_call_id, denied, result_sha256: sha256(content) });
      } else assert.equal(calls.length, 0, "Unexpected tool history");
      await writeFile(join(fixture, `request-${server.requests.length}.json`), JSON.stringify(request, null, 2));
      onRequest?.();
    } };
}
function deferred() { let resolve; const promise = new Promise(done => { resolve = done; }); return { promise, resolve }; }
const cancels = ["cancel-stream", "cancel-permission"].map(name => ({ name, gate: deferred() }));
const scenarios = [
  { name: "text", state: "SUCCEEDED", requests: 1 },
  { name: "approved", state: "SUCCEEDED", requests: 2, decision: "allow_once", resultGate: deferred() },
  { name: "denied", state: "SUCCEEDED", requests: 2, decision: "reject_once", resultGate: deferred() },
  { name: "api-error", state: "FAILED", requests: 1 },
  ...cancels.map(item => ({ ...item, state: "CANCELLED", requests: 3 })),
  { name: "fabric-b", state: "SUCCEEDED", requests: 1 },
];
const promptFor = name => `FABRIC_CLINE_FIXTURE_${name}`;
function plan() {
  const steps = [];
  for (const scenario of scenarios) {
    const { name } = scenario, prompt = promptFor(name);
    if (scenario.decision) steps.push(step(prompt, toolWire(name)), step(prompt, textWire(complete),
      { tool: name, denied: name === "denied", wait: scenario.resultGate.promise }));
    else if (name === "api-error") steps.push(step(prompt, JSON.stringify({ error: { message: "Fixture terminal denial", type: "invalid_request_error", code: "fixture_denied" } }), { status: 400 }));
    else if (scenario.gate) {
      steps.push(step(prompt, name === "cancel-stream" ? textWire(held, false) : toolWire(name),
        name === "cancel-stream" ? { wait: new Promise(() => {}) } : {}));
      steps.push(step(promptFor(`${name}-b`), textWire(complete), { wait: scenario.gate.promise }));
      steps.push(step(promptFor(`${name}-followup`), textWire(complete)));
    } else steps.push(step(prompt, textWire(complete)));
  }
  return steps;
}
async function ownedHub() {
  const discovery = join(state, "data/hub.json"), info = await stat(discovery);
  assert.ok(info.isFile() && info.uid === process.getuid() && !(info.mode & 0o077));
  assert.equal(await realpath(discovery), discovery);
  const current = JSON.parse(await readFile(discovery, "utf8"));
  assert.equal(current.pid, hub.pid); assert.equal(current.authToken, hub.authToken); assert.equal(current.url, hub.url);
  assert.equal(current.port, hub.port); assert.equal(current.host, "127.0.0.1");
  process.kill(hub.pid, 0);
  return true;
}
async function pendingPermission(name) {
  const found = await until(async () => {
    const sockets = (await readdir(permissions)).filter(name => /^p-[a-f0-9]{24}\.sock$/.test(name));
    assert.ok(sockets.length <= 1, "One Fabric peer per owned permission endpoint");
    if (!sockets.length) return;
    const endpoint = join(permissions, sockets[0]);
    const listing = await permissionControl(endpoint, { op: "list" });
    assert.ok(!listing.error && listing.pending.length <= 1);
    if (!listing.pending.length) return;
    const review = listing.pending[0];
    assert.equal(review.request.sessionId, review.sessionId);
    assert.equal(review.request.toolCall.toolCallId, `call_${name}`);
    assert.deepEqual(review.request.toolCall.rawInput, { commands: [commandFor(name)] });
    assert.equal(review.request.toolCall.status, "pending");
    return { endpoint, listing, review };
  }, "owned pending permission");
  await writeFile(join(fixture, `${name}-permission.json`), JSON.stringify(found, null, 2));
  return found;
}
function decisionFor({ endpoint, listing, review }, kind) {
  const option = review.request.options.find(option => option.kind === kind);
  assert.ok(option);
  return { op: "decide", endpoint, nonce: listing.nonce, sessionId: review.sessionId,
    requestId: review.requestId, digest: review.digest, optionId: option.optionId, kind };
}
async function checkPending(found, runId, name) {
  const show = await cli(["runs", "show", runId, "--json"]);
  assert.equal(show.run.state, "RUNNING");
  const workspace = show.workspace.managed_path;
  assert.ok(isAbsolute(workspace) && workspace.startsWith(`${root}${sep}`));
  assert.equal(await readFile(join(workspace, target), "utf8"), original, "Tool executed before explicit decision");
  const current = await permissionControl(found.endpoint, { op: "list" });
  assert.deepEqual(current.pending, [found.review]);
  assert.equal(server.requests.filter(request => matches(promptFor(name), name)(request)).length, 0);
  return workspace;
}
async function invalidDecisions(found, runId, name) {
  const valid = decisionFor(found, "allow_once");
  const invalid = [ { ...valid, endpoint: `${valid.endpoint}.wrong` }, { ...valid, nonce: "unknown" },
    { ...valid, sessionId: `${valid.sessionId}-other` }, { ...valid, requestId: "unknown" },
    { ...valid, digest: "0".repeat(64) }, { ...valid, optionId: "unknown" }, { ...valid, kind: "allow_always" } ];
  if (previousDecision) invalid.push(previousDecision);
  for (const decision of invalid) {
    assert.deepEqual(await permissionControl(found.endpoint, decision), { error: "invalid_decision" });
    await checkPending(found, runId, name);
  }
  return invalid.length;
}
async function observeHeld(runId) {
  const child = Bun.spawn([inputs.fabric.path, "--root", root, "runs", "logs", runId, "--raw", "stdout", "--follow", "--json"],
    { cwd: repo, env, stdin: "ignore", stdout: "pipe", stderr: "ignore" });
  const timer = setTimeout(() => child.kill("SIGKILL"), 45000);
  let buffer = "", raw = "", text = "", size = 0;
  const decoder = new TextDecoder();
  try {
    for await (const bytes of child.stdout) {
      size += bytes.length; assert.ok(size <= 2097152);
      buffer += decoder.decode(bytes, { stream: true });
      let newline;
      while ((newline = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, newline); buffer = buffer.slice(newline + 1);
        if (!line.trim()) continue;
        const observation = JSON.parse(line); raw += observation.raw ?? "";
        let end;
        while ((end = raw.indexOf("\n")) >= 0) {
          const line = raw.slice(0, end); raw = raw.slice(end + 1);
          if (line.trim()) { const event = JSON.parse(line); if (event.type === "acp.text") text += event.text; }
        }
        assert.ok(!observation.terminal, "Held stream terminated");
        if (text.includes(held)) return;
      }
    }
    throw new Error("No actual held ACP text");
  } finally { clearTimeout(timer); child.kill(); await child.exited; }
}
async function collect(batchId, runId, scenario) {
  const result = await cli(["results", batchId, "--json"]), show = await cli(["runs", "show", runId, "--json"]);
  assert.ok(result.terminal && result.tasks.length === 1);
  const task = result.tasks[0], report = task.result, receipt = show.launch_receipt;
  assert.equal(task.state, scenario.state); assert.equal(show.run.state, task.state); assert.equal(report.state, task.state);
  assert.equal(result.succeeded, task.state === "SUCCEEDED");
  assert.ok(task.run_id === runId && report.run_id === runId && show.run.run_id === runId && show.run.batch_id === batchId && show.run.harness === "acp");
  assert.ok(task.source.prepared_state_id === prepared.state_id && task.source.base_commit_sha === prepared.source_commit_sha);
  assert.ok(receipt?.run_id === runId && receipt.workspace_id === report.workspace_id && receipt.workspace_path === report.workspace_path);
  assert.ok(receipt.prepared_state_id === prepared.state_id && receipt.source_commit_sha === prepared.source_commit_sha && receipt.actual_sha256 === receipt.source.prepared_sha256);
  assert.ok(receipt.workspace_device != null && receipt.workspace_inode != null);
  assert.ok(show.workspace.run_id === runId && show.workspace.workspace_id === report.workspace_id && show.workspace.managed_path === report.workspace_path);
  assert.equal(show.workspace.state, "RECLAIMED");
  assert.ok(report.cleanup?.action === "reclaimed" && report.cleanup.run_id === runId && report.cleanup.workspace_id === report.workspace_id);
  await assert.rejects(stat(report.workspace_path), { code: "ENOENT" });
  assert.equal(show.worker_attempts.length, 1);
  const attempt = show.worker_attempts[0];
  assert.ok(attempt.attempt_id === receipt.attempt_id && attempt.run_id === runId && attempt.started_at && attempt.finished_at && (attempt.exit_code != null || attempt.exit_signal != null));
  assert.equal(attempt.harness_version, VERSION);
  const group = /^local-process-group:([1-9][0-9]*)$/.exec(show.run.pid_metadata ?? "");
  assert.ok(group); assert.throws(() => process.kill(-Number(group[1]), 0), { code: "ESRCH" });
  assert.ok(report.capture && !report.capture.truncated && report.capture.event_counts.invalid === 0 && report.capture.event_counts.oversized === 0);
  assert.ok(report.artifact_manifest_id && task.artifacts.length);
  const contents = new Map(); let total = 0;
  for (const artifact of task.artifacts) {
    const path = resolve(root, artifact.relative_path);
    assert.ok(path.startsWith(`${root}${sep}`)); total += artifact.size_bytes;
    assert.ok(artifact.size_bytes >= 0 && total <= 16777216);
    const bytes = await readFile(path);
    assert.equal(bytes.length, artifact.size_bytes); assert.equal(sha256(bytes), artifact.sha256);
    assert.ok(!contents.has(artifact.kind)); contents.set(artifact.kind, bytes);
  }
  const manifest = JSON.parse(contents.get("manifest").toString());
  assert.ok(manifest.run_id === runId && manifest.workspace_id === report.workspace_id && manifest.prepared_state_id === prepared.state_id && manifest.base_commit_sha === prepared.source_commit_sha);
  const raw = jsonl(contents.get("stdout")), events = jsonl(contents.get("events")).map(record => record.event).filter(Boolean);
  const text = raw.filter(event => event.type === "acp.text").map(event => event.text).join("");
  const normalized = events.filter(event => event.type === "output").map(event => event.text).join("");
  if (scenario.state === "SUCCEEDED") {
    assert.ok(report.exit_code === 0 && !report.error_kind && !report.capture.native_failure && !task.error);
    assert.equal(raw.filter(event => event.type === "acp.stop" && event.reason === "end_turn").length, 1);
    assert.ok(text.includes(complete) && normalized.includes(complete));
    assert.ok(!raw.some(event => event.type === "acp.failure") && !events.some(event => event.type === "failure"));
  } else {
    assert.ok(!text.includes(complete) && !normalized.includes(complete) && !raw.some(event => event.type === "acp.stop"));
    assert.ok(!events.some(event => event.type === "completed" && event.success));
    if (scenario.state === "FAILED") assert.ok(report.capture.native_failure || report.error_kind || report.exit_code !== 0 || events.some(event => event.type === "failure"));
  }
  const tools = raw.filter(event => event.type === "acp.tool");
  if (scenario.decision || scenario.name === "cancel-permission") {
    assert.ok(tools.some(event => event.status === "pending"));
    assert.ok(tools.every(event => event.call === 1));
    if (scenario.decision) {
      const status = scenario.decision === "allow_once" ? "completed" : "failed";
      assert.ok(tools.some(event => event.status === status && event.phase === "end"));
      assert.ok(events.some(event => event.type === "tool" && event.name === `acp.tool.1.${status}` && event.phase === "end"));
    }
  } else assert.equal(tools.length, 0);
  const patch = contents.get("tracked_patch")?.toString() ?? "";
  if (scenario.name === "approved") {
    assert.equal(patch.split("\n").filter(line => line === "+approved").length, 1, "Exact-once marker patch required");
    const restored = join(fixture, "restored"), patchFile = join(fixture, "marker.patch");
    await run("/usr/bin/git", ["clone", "--quiet", "--no-local", "--no-hardlinks", repo, restored]);
    await writeFile(patchFile, patch); await run("/usr/bin/git", ["apply", "--binary", patchFile], restored);
    assert.equal(await readFile(join(restored, target), "utf8"), `${original}approved\n`);
    assert.equal((await run("/usr/bin/git", ["diff", "--name-only"], restored)).trim(), target);
  } else assert.equal(patch, "", "Unapproved workspace mutation");
  assert.equal(contents.get("untracked_paths")?.length, 0);
  assert.equal(await readFile(join(repo, target), "utf8"), original);
  const db = new Database(join(root, "fabric.db"), { readonly: true });
  try { assert.equal(db.query("SELECT state FROM workspaces WHERE workspace_id=?").get(report.workspace_id)?.state, "RECLAIMED"); }
  finally { db.close(); }
  await writeFile(join(fixture, `${scenario.name}-capture.json`), JSON.stringify({ result, show }, null, 2));
  return { run_id: runId, batch_id: batchId, state: task.state, capture_verified: true, workspace_reclaimed: true,
    child_reaped: true, attempts: 1, patch_restored: scenario.name === "approved",
    artifacts: task.artifacts.map(({ kind, size_bytes, sha256 }) => ({ kind, size_bytes, sha256 })) };
}
async function nativeSibling(scenario) {
  const cwd = join(fixture, `${scenario.name}-native-b`);
  await mkdir(cwd, { mode: 0o700 });
  const notifications = [];
  const connection = launchAcp({ executable: inputs.cline.path, args: ["--acp"], cwd,
    env: nativeEnvironment(state, hub.port), requestTimeoutMs: 45000, failOnPeerRequest: true,
    onNotification(message) { assert.ok(notifications.length < 2048); notifications.push(message); } });
  peers.add(connection);
  const initialized = await connection.initialize();
  assert.equal(initialized.protocolVersion, 1);
  assert.equal(initialized.agentInfo?.version, "3.0.75");
  const session = await connection.request("session/new", { cwd, mcpServers: [] });
  assert.ok(typeof session.sessionId === "string" && session.sessionId.length);
  const prompt = async name => {
    const start = notifications.length;
    const result = await connection.request("session/prompt", { sessionId: session.sessionId, prompt: [{ type: "text", text: promptFor(name) }] });
    assert.equal(result.stopReason, "end_turn");
    await new Promise(resolve => setImmediate(resolve));
    const updates = notifications.slice(start).filter(message => message.method === "session/update");
    assert.ok(updates.every(message => message.params.sessionId === session.sessionId));
    assert.ok(updates.filter(message => message.params.update.sessionUpdate === "agent_message_chunk")
      .map(message => message.params.update.content.text ?? "").join("").includes(complete));
    return result;
  };
  const first = prompt(`${scenario.name}-b`);
  // Attach rejection observation immediately while A's owner remains in control.
  void first.catch(() => {});
  return { connection, first, prompt, async finish() {
    await writeFile(join(fixture, `${scenario.name}-native-b.json`), JSON.stringify({ initialized, session, notifications }, null, 2));
    const pid = connection.pid; await connection.close(); peers.delete(connection);
    assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
  } };
}
try {
  nodeVersion = (await run(inputs.node.path, ["--version"])).trim();
  const [major, minor] = nodeVersion.slice(1).split(".").map(Number);
  assert.ok(major > 22 || (major === 22 && minor >= 18));
  server = await startScriptedProvider(plan(), { unordered: true });
  const settings = join(state, "data/settings");
  await writeFile(join(settings, "providers.json"), JSON.stringify({ version: 1, lastUsedProvider: "fabric-local", modes: {},
    providers: { "fabric-local": { settings: { provider: "fabric-local", model, apiKey: key, baseUrl: `${server.baseUrl}/v1`,
      protocol: "openai-chat", client: "openai-compatible", capabilities: ["streaming", "tools"],
      modelCatalog: { loadLatestOnInit: false, loadPrivateOnAuth: false, includeClineCloudModels: false } },
      updatedAt: "2026-01-01T00:00:00.000Z", tokenSource: "manual" } } }));
  await writeFile(join(settings, "global-settings.json"), JSON.stringify({ telemetryOptOut: true, autoUpdateEnabled: false, compactionEnabled: false, toolAutoApprove: false }));
  nativeVersion = (await native(["--version"])).trim(); assert.equal(nativeVersion, "3.0.75");
  const ensured = JSON.parse(await native(["hub", "--cwd", repo, "--host", "127.0.0.1", "--port", "0", "ensure", "--json", "--no-connectors"]));
  const candidate = JSON.parse(await readFile(join(state, "data/hub.json"), "utf8"));
  assert.ok(Number.isSafeInteger(candidate.pid) && candidate.pid > 0 && Number.isInteger(candidate.port) && candidate.port > 0 && candidate.port <= 65535);
  assert.equal(candidate.host, "127.0.0.1");
  assert.ok(typeof candidate.authToken === "string" && candidate.authToken.length > 0);
  assert.equal(candidate.url, ensured.url); assert.equal(candidate.authToken, ensured.authToken);
  hub = candidate; await ownedHub();
  const endpoint = new URL(candidate.url);
  assert.equal(endpoint.origin, `ws://127.0.0.1:${candidate.port}`);
  assert.equal(endpoint.pathname, "/hub"); assert.ok(!endpoint.username && !endpoint.password && !endpoint.search && !endpoint.hash);
  const launch = join(fixture, "native-launch.json");
  await writeFile(launch, JSON.stringify({ state, port: hub.port }));
  const config = join(fixture, "launch-input.json"), pkg = join(fixture, "acp-package");
  await writeFile(config, JSON.stringify({ executable: inputs.node, args: [inputs.bootstrap.path, inputs.cline.path, launch],
    nativeHome: join(state, "home"), cwd: "fabric-workspace", requestTimeoutMs: 45000, permission: { directory: permissions },
    pins: [inputs.bootstrap, inputs.cline, { path: launch, sha256: fileHash(launch) },
      ...["providers.json", "global-settings.json"].map(name => ({ path: join(settings, name), sha256: fileHash(join(settings, name)) }))] }));
  await run(inputs.node.path, [join(source, "plugins/acp/package.mjs"), config, pkg, join(fixture, "acp.tgz")]);
  const acp = install(pkg, { registry, fabric: inputs.fabric.path, version: "0.1.0", sha256: directory(pkg).hash, trust: true });
  await symlink(join(registry, "packages", acp.sha256, "fabric-acp-bridge"), join(bin, "fabric-acp-bridge"));
  const local = reference("local", join(fixture, "local-package"), inputs.localPlugin.path);
  install(local, { registry, fabric: inputs.fabric.path, version: "0.1.0", sha256: directory(local).hash, trust: true });
  // A synthetic repository works from an extracted kit; never copies source hooks/config.
  await run("/usr/bin/git", ["init", "--quiet"], repo);
  await run("/usr/bin/git", ["config", "core.hooksPath", "/dev/null"], repo);
  await writeFile(join(repo, target), original);
  await run("/usr/bin/git", ["add", target], repo);
  await run("/usr/bin/git", ["-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "fixture baseline"], repo);
  const recipe = join(fixture, "prepare.toml");
  await writeFile(recipe, 'version=1\n[[steps]]\nargv=["/usr/bin/true"]\n');
  await cli(["project", "add", "--name", "scripted", "--path", repo, "--json"]);
  prepared = await cli(["project", "prepare", "scripted", "--recipe", recipe, "--json"]); assert.equal(prepared.status, "READY");
  for (const scenario of scenarios) {
    phase = scenario.name;
    const before = server.requests.length, request = join(fixture, `${scenario.name}.json`);
    await writeFile(request, JSON.stringify({ project: "scripted", placement: "local", max_concurrency: 1, tasks: [{
      task_id: scenario.name, harness_id: "acp", prompt: promptFor(scenario.name),
      execution_context: { kind: "explicit", harness_version: VERSION, native_config_refs: [{ kind: "native-home", path: join(state, "home") }], connection_refs: [] },
      source_ref: { kind: "prepared", state_id: prepared.state_id },
      limits: { timeout_seconds: 180, copy_limit_bytes: 67108864, max_log_bytes: 1048576, max_artifact_bytes: 16777216 },
    }] }));
    const accepted = await cli(["workers", "spawn", "--request", request, "--json"]);
    if (accepted.batch_id) batches.push(accepted.batch_id);
    assert.ok(accepted.accepted && accepted.batch_id && accepted.tasks.length === 1);
    const batchId = accepted.batch_id, runId = accepted.tasks[0].run_id; assert.ok(runId);
    let permission, invalid = 0, siblingProof;
    if (scenario.decision || scenario.name === "cancel-permission") {
      permission = await pendingPermission(scenario.name);
      await checkPending(permission, runId, scenario.name);
      invalid = await invalidDecisions(permission, runId, scenario.name);
      if (scenario.decision) {
        const decision = decisionFor(permission, scenario.decision);
        assert.deepEqual(await permissionControl(permission.endpoint, decision), { ok: true });
        assert.deepEqual(await permissionControl(permission.endpoint, decision), { error: "invalid_decision" }, "Stale decision granted access");
        previousDecision = decision;
        scenario.resultGate.resolve();
      }
    }
    if (scenario.gate) {
      if (scenario.name === "cancel-stream") await observeHeld(runId);
      const sibling = await nativeSibling(scenario);
      await until(() => server.requests.some(matches(promptFor(`${scenario.name}-b`))), "independent native B request");
      if (permission) await checkPending(permission, runId, scenario.name);
      await ownedHub();
      const started = Date.now(); await cli(["cancel", batchId, "--json"]);
      assert.equal((await cli(["wait", batchId, "--timeout-seconds", "45", "--json"])).terminal, true);
      assert.ok(Date.now() - started < 60000, "Cancellation exceeded bound");
      await ownedHub();
      if (permission) {
        await until(async () => !(await readdir(permissions)).includes(permission.endpoint.split("/").at(-1)), "cancelled permission endpoint cleanup");
        await assert.rejects(permissionControl(permission.endpoint, decisionFor(permission, "allow_once")));
      }
      scenario.gate.resolve(); await sibling.first;
      await sibling.prompt(`${scenario.name}-followup`); await ownedHub(); await sibling.finish();
      siblingProof = { kind: "REAL_NATIVE_ACP_PEER_NOT_FABRIC_FOLLOWUP", prompt_completed: true, followup_completed: true, child_reaped: true, same_owned_hub_alive: true };
    }
    assert.equal((await cli(["wait", batchId, "--timeout-seconds", "90", "--json"])).terminal, true);
    let capture, captureError;
    try { capture = await collect(batchId, runId, scenario); }
    catch (error) { captureError = error; errors.push(error); }
    assert.equal(server.requests.length - before, scenario.requests, "Retry/fallback/auxiliary request count");
    await until(async () => (await readdir(permissions)).length === 0, "permission endpoint cleanup");
    await ownedHub();
    if (captureError) {
      const observed = await cli(["results", batchId, "--json"]);
      proof.push({ scenario: scenario.name, outcome: "FAIL", expected_state: scenario.state,
        state: observed.tasks[0]?.state, run_id: runId, batch_id: batchId, model_requests: scenario.requests });
    } else proof.push({ scenario: scenario.name, outcome: "PASS", model_requests: scenario.requests, ...capture,
      ...(permission ? { pending_observed: true, invalid_bindings_rejected: invalid, stale_decision_rejected: true,
        permission_correlation_sha256: sha256(JSON.stringify(permission.review)) } : {}), ...(siblingProof ? { native_sibling: siblingProof } : {}) });
  }
} catch (error) { errors.push(error); }
finally {
  for (const scenario of scenarios) { scenario.gate?.resolve(); scenario.resultGate?.resolve(); }
  let dependentsEnded = true;
  for (const peer of peers) {
    try { const pid = peer.pid; await peer.close(); assert.throws(() => process.kill(pid, 0), { code: "ESRCH" }); }
    catch (error) { errors.push(error); dependentsEnded = false; }
  }
  if (await Bun.file(join(root, "fabric.db")).exists()) {
    for (const batch of batches) {
      try {
        await cli(["cancel", batch, "--json"]);
        assert.equal((await cli(["wait", batch, "--timeout-seconds", "45", "--json"])).terminal, true);
        const result = await cli(["results", batch, "--json"]);
        for (const task of result.tasks) {
          assert.ok(task.run_id);
          const show = await cli(["runs", "show", task.run_id, "--json"]);
          const group = /^local-process-group:([1-9][0-9]*)$/.exec(show.run.pid_metadata ?? "");
          assert.ok(group); assert.throws(() => process.kill(-Number(group[1]), 0), { code: "ESRCH" });
          assert.equal(show.workspace.state, "RECLAIMED");
        }
      }
      catch (error) { errors.push(error); dependentsEnded = false; }
    }
    try { await cli(["manager", "stop", "--json"]); managerStopped = true; }
    catch (error) { errors.push(error); dependentsEnded = false; }
  }
  // Stop only after every dependent peer/worker ended, and only matching discovery.
  if (hub && dependentsEnded) {
    try {
      await ownedHub(); const stopped = JSON.parse(await native(["hub", "stop"])); assert.equal(stopped.stopped, true);
      await until(() => { try { process.kill(hub.pid, 0); return false; } catch (error) { assert.equal(error.code, "ESRCH"); return true; } }, "owned hub exit");
      hubStopped = true;
      assert.equal(await realpath(state), state);
      await rm(state, { recursive: true }); await assert.rejects(stat(state), { code: "ENOENT" });
      stateCleaned = true;
    } catch (error) { errors.push(error); }
  }
  if (server) {
    try { await writeFile(join(fixture, "provider-requests.json"), JSON.stringify(server.requests, null, 2)); server.assertDone(); }
    catch (error) { errors.push(error); }
    try { await server.close(); } catch (error) { errors.push(error); }
  }
  try { assert.deepEqual(await readdir(permissions), []); await rmdir(permissions); permissionsCleaned = true; }
  catch (error) { errors.push(error); }
}
const result = { result: errors.length || proof.length !== scenarios.length || !hubStopped || !stateCleaned || !permissionsCleaned ? "FAIL" : "PASS", evidence_root: fixture,
  generic_acp_only: true, native_version: nativeVersion ?? null, node_version: nodeVersion ?? null, inputs,
  source_recipe_upstream_commit: "06f0ec711ea417430d552b98e7911fdac6f495a1", source_recipe_openai_compatible_sdk: "3.0.67",
  source_commit_sha: prepared?.source_commit_sha, prepared_state_id: prepared?.state_id,
  os_egress_confined_by_script: false, fabric_session_followup: false,
  native_sibling_followup: proof.filter(item => item.native_sibling?.followup_completed).length === cancels.length,
  model_requests: server?.requests.length ?? 0, expected_model_requests: scenarios.reduce((sum, scenario) => sum + scenario.requests, 0),
  tool_correlations: correlations, scenarios: proof, cleanup: { manager_stopped: managerStopped, owned_hub_stopped: hubStopped,
    native_state_removed: stateCleaned, permission_directory_removed: permissionsCleaned, retained_private_evidence: true },
  ...(errors.length ? { last_phase: phase, failure_evidence: "failure.txt" } : {}) };
if (errors.length) await writeFile(join(fixture, "failure.txt"), errors.map(error => String(error?.stack ?? error)).join("\n"));
await writeFile(join(fixture, "proof.json"), JSON.stringify(result, null, 2));
// No socket names, nonces, permission payloads, HTTP bodies, reasoning, or hub tokens.
console.log(JSON.stringify(result));
if (result.result !== "PASS") process.exitCode = 1;
