// Bun runner; no downloads, builds, credentials, source commits, or production adapters.
// Usage: /absolute/bun integrations/claude/scripted-provider-smoke.mjs /absolute/private-inputs.json
// Inputs: {bun,fabric,node,shim,claude,localPlugin,bootstrap}: each {path,sha256};
// plus claudeVersion (exact --version output). All paths canonical, absolute, content pinned.
// bootstrap must select the adjacent fixture-launcher.mjs; Node >=22.18, native arm64 or x64 macOS binaries.
// shim: manager-verified official v0.112.0 standalone release; darwin-arm64 ZIP SHA256
// ada7afbea165c20e5ec50a190653309bfba528ed9f74ef86a24f2734329ec59c.
// Prefer its bundled Claude; localPlugin is the accepted release's native reference local target.
// Endpoint overrides are NOT OS egress confinement. Run in the controller's isolated worker.
// Known compatibility risk: v0.112.0 assistant messageId/core updates exceed the current bridge subset.
import assert from "node:assert/strict";
import { constants } from "node:fs";
import { access, mkdir, mkdtemp, readFile, realpath, stat, symlink, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { Database } from "bun:sqlite";
import { startScriptedProvider } from "../../scripts/scripted-provider.mjs";
import { reference, install } from "../../sdk/typescript/cli.mjs";
import { directory } from "../../sdk/typescript/package.mjs";
import { fileHash, sha256, VERSION } from "../../plugins/acp/config.mjs";

assert.equal(process.argv.length, 3, "Usage: bun integrations/claude/scripted-provider-smoke.mjs /absolute/private-inputs.json");
assert.equal(process.platform, "darwin", "Socket evidence requires macOS /usr/sbin/lsof");
assert.ok(["arm64", "x64"].includes(process.arch), "Official macOS arm64 or x64 release required");
const here = dirname(fileURLToPath(import.meta.url));
const source = await realpath(join(here, "../.."));
const inputPath = process.argv[2];
assert.ok(isAbsolute(inputPath) && await realpath(inputPath) === inputPath);
const inputStat = await stat(inputPath);
assert.ok(inputStat.isFile() && inputStat.size <= 32768 && inputStat.uid === process.getuid() && !(inputStat.mode & 0o077), "Inputs must be a private owned file");
const inputs = JSON.parse(await readFile(inputPath, "utf8"));
const names = ["bun", "fabric", "node", "shim", "claude", "localPlugin", "bootstrap"];
assert.deepEqual(Object.keys(inputs).sort(), [...names, "claudeVersion"].sort());
assert.ok(typeof inputs.claudeVersion === "string" && /^2\.[0-9]+\.[0-9]+(?: \(Claude Code\))?$/.test(inputs.claudeVersion));
for (const name of names) {
  const pin = inputs[name];
  assert.deepEqual(Object.keys(pin).sort(), ["path", "sha256"]);
  assert.match(pin.sha256, /^[a-f0-9]{64}$/);
  assert.equal(fileHash(pin.path), pin.sha256, `${name}: content pin mismatch`);
  if (name !== "bootstrap") {
    await access(pin.path, constants.X_OK);
    const file = Bun.file(pin.path);
    assert.notEqual(await file.slice(0, 2).text(), "#!", `${name}: native binary required, not a shebang`);
  }
}
assert.equal(inputs.bun.path, await realpath(process.execPath), "Select the actual runner, not another Bun");
assert.equal(inputs.bootstrap.path, await realpath(join(here, "fixture-launcher.mjs")));
await access("/usr/sbin/lsof", constants.X_OK);
process.umask(0o077);
const fixture = await realpath(await mkdtemp("/private/tmp/fabric-scripted-claude-"));
const root = join(fixture, "storage"), repo = join(fixture, "source"), home = join(fixture, "home");
const nativeHome = join(fixture, "native-home"), registry = join(fixture, "registry"), bin = join(fixture, "bin");
for (const path of [home, registry, bin, join(home, "tmp"), ...[".claude", "tmp", "xdg-config", "xdg-data", "xdg-state", "xdg-cache"].map(name => join(nativeHome, name))]) {
  await mkdir(path, { recursive: true, mode: 0o700 });
}
await symlink(inputs.node.path, join(bin, "node"));
const env = { HOME: home, TMPDIR: join(home, "tmp"), LC_ALL: "C", PATH: `${bin}:/usr/bin:/bin`,
  XDG_CONFIG_HOME: home, XDG_DATA_HOME: home, XDG_CACHE_HOME: home, XDG_STATE_HOME: home,
  FABRIC_PLUGIN_REGISTRY: registry, FABRIC_ACP_HOME: nativeHome,
  GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0", OTEL_SDK_DISABLED: "true" };
const model = "claude-sonnet-4-6", key = "fabric-fixture-only-not-a-secret";
const target = "README.md", changed = "scripted native Claude write\n";
const sentinel = "FABRIC_SCRIPTED_COMPLETE", held = "FABRIC_SCRIPTED_HELD", partial = "incomplete fixture";
const route = "/v1/messages?beta=true";
const jsonl = text => text.split("\n").filter(line => line.trim()).map(line => JSON.parse(line));
let commandNumber = 0, phase = "setup", prepared, original, nativeVersion, shimVersion, nodeVersion;
let server, sourceBefore, emergency = false, cleaning = false;
const batches = [], proof = [], errors = [], commands = new Set();
// Emergency stop is a failure, never a restart or evidence of success. Core owns worker cleanup.
const watchdog = setTimeout(() => { emergency = true; for (const child of commands) child.kill("SIGKILL"); }, 360000);
async function run(program, args, cwd = fixture, seconds = 60, environment = env) {
  assert.ok(cleaning || !emergency, "Whole-smoke emergency watchdog expired");
  const prefix = join(fixture, `command-${++commandNumber}`);
  const child = Bun.spawn([program, ...args], { cwd, env: environment, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  commands.add(child);
  let expired = false;
  const timer = setTimeout(() => { expired = true; child.kill("SIGKILL"); }, seconds * 1000);
  async function bounded(stream) {
    const chunks = []; let size = 0;
    for await (const bytes of stream) {
      size += bytes.length;
      if (size > 2097152) { child.kill("SIGKILL"); throw new Error(`Command output ceiling exceeded: ${prefix}`); }
      chunks.push(bytes);
    }
    return Buffer.concat(chunks).toString();
  }
  try {
    const [stdout, stderr, code] = await Promise.all([bounded(child.stdout), bounded(child.stderr), child.exited]);
    await writeFile(`${prefix}.json`, JSON.stringify({ program, args, cwd, code, watchdog_expired: expired }));
    await writeFile(`${prefix}.stdout`, stdout); await writeFile(`${prefix}.stderr`, stderr);
    assert.ok(!expired && (cleaning || !emergency), `Command watchdog expired: ${prefix}`);
    assert.equal(code, 0, `Command failed; private evidence: ${prefix}`);
    return stdout;
  } finally { clearTimeout(timer); commands.delete(child); child.kill(); await child.exited; }
}
const cli = async args => JSON.parse(await run(inputs.fabric.path, ["--root", root, ...args], repo, args[0] === "wait" ? 105 : 60));
async function sourceSnapshot(path) {
  return { status: await run("/usr/bin/git", ["status", "--porcelain=v1", "--untracked-files=all"], path),
    diff: sha256(await run("/usr/bin/git", ["diff", "--binary", "HEAD"], path)) };
}
function frame(type, data) { return `event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`; }
function start(id) {
  return frame("message_start", { message: { id, type: "message", role: "assistant", model, content: [],
    stop_reason: null, stop_sequence: null, usage: { input_tokens: 10, output_tokens: 0 } } });
}
function finish(reason) {
  return frame("content_block_stop", { index: 0 }) + frame("message_delta", {
    delta: { stop_reason: reason, stop_sequence: null }, usage: { output_tokens: 10 } }) + frame("message_stop", {});
}
function textWire(text, complete = true) {
  return start("msg_fixture_text") + frame("content_block_start", { index: 0, content_block: { type: "text", text: "" } }) +
    frame("content_block_delta", { index: 0, delta: { type: "text_delta", text: text.slice(0, 8) } }) +
    frame("content_block_delta", { index: 0, delta: { type: "text_delta", text: text.slice(8) } }) + (complete ? finish("end_turn") : "");
}
function split(wire) { return [wire.slice(0, 7), wire.slice(7, 51), wire.slice(51, -5), wire.slice(-5)]; }
function toolWire(id, name, input) {
  const json = JSON.stringify(input), middle = Math.floor(json.length / 2);
  return start(`msg_${id}`) + frame("content_block_start", { index: 0, content_block: { type: "tool_use", id, name, input: {} } }) +
    frame("content_block_delta", { index: 0, delta: { type: "input_json_delta", partial_json: json.slice(0, middle) } }) +
    frame("content_block_delta", { index: 0, delta: { type: "input_json_delta", partial_json: json.slice(middle) } }) + finish("tool_use");
}
function checkRequest(prompt, expectedTools = []) {
  return ({ method, path, headers, body }) => {
    assert.ok(method === "POST" && path === route, "Unexpected Anthropic route");
    assert.ok(headers["x-api-key"] === key, "Only the dummy fixture API key is allowed");
    for (const header of ["authorization", "cookie", "proxy-authorization", "api-key"]) assert.ok(!headers[header], "Unexpected credential header");
    assert.ok(typeof headers["anthropic-version"] === "string", "Missing Anthropic protocol version");
    assert.ok(body.model === model && body.stream === true, "Wrong model or non-streaming fallback");
    assert.ok(Array.isArray(body.messages) && body.messages.some(message => message.role === "user" && JSON.stringify(message.content).includes(prompt)), "Prompt missing");
    const blocks = body.messages.flatMap(message => Array.isArray(message.content) ? message.content : []);
    const calls = blocks.filter(block => block.type === "tool_use"), results = blocks.filter(block => block.type === "tool_result");
    assert.equal(calls.length, expectedTools.length, "Unexpected/missing native tool call");
    assert.equal(results.length, expectedTools.length, "Unexpected/missing actual tool result");
    for (const [index, name] of expectedTools.entries()) {
      const id = name === "Read" ? "fixture_read" : "fixture_write";
      assert.ok(calls[index].id === id && calls[index].name === name && calls[index].input.file_path === target, "Native conversation changed tool input");
      if (name === "Write") assert.equal(calls[index].input.content, changed);
      const result = results.find(block => block.tool_use_id === id);
      const expected = name === "Write" ? target : original.split("\n").find(line => line.trim());
      assert.ok(result && result.is_error !== true && JSON.stringify(result.content).includes(expected), "Missing successful native tool result");
    }
  };
}
const headers = { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "close" };
const step = (check, chunks, extra = {}) => ({ method: "POST", path: route, check, headers, chunks, ...extra });
let releaseDisconnect;
const disconnectGate = new Promise(resolve => { releaseDisconnect = resolve; });
const scenarios = [
  { name: "split-text", state: "SUCCEEDED", requests: 1, prompt: "Synthetic split-text fixture. Reply with the requested completion marker only.",
    steps(prompt) { return [step(checkRequest(prompt), split(textWire(sentinel)))]; } },
  { name: "native-write", state: "SUCCEEDED", requests: 3,
    prompt: `Synthetic write fixture. Read ${target}, then use native Write to replace it with ${JSON.stringify(changed)}, then report completion. No other tools or files.`,
    steps(prompt) { return [step(request => { checkRequest(prompt)(request);
      for (const name of ["Read", "Write"]) assert.ok(request.body.tools?.some(tool => tool.name === name), `Native ${name} unavailable`);
    }, split(toolWire("fixture_read", "Read", { file_path: target }))),
    step(checkRequest(prompt, ["Read"]), split(toolWire("fixture_write", "Write", { file_path: target, content: changed }))),
    step(checkRequest(prompt, ["Read", "Write"]), split(textWire(sentinel)))]; } },
  { name: "api-error", state: "FAILED", requests: 1, prompt: "Synthetic non-retryable Anthropic API failure fixture.",
    steps(prompt) { return [step(checkRequest(prompt), [JSON.stringify({ type: "error", error: { type: "authentication_error", message: "Synthetic fixture denial" } })],
      { status: 401, headers: { "content-type": "application/json" } })]; } },
  { name: "incomplete-disconnect", state: "FAILED", requests: 1, prompt: "Synthetic interrupted Anthropic stream fixture.",
    steps(prompt) { return [step(checkRequest(prompt), [textWire(partial, false), 'event: content_block_delta\ndata: {"type":'], { wait: disconnectGate, disconnect: true })]; } },
  { name: "cancel-held-stream", state: "CANCELLED", requests: 1, prompt: "Synthetic held Anthropic stream cancellation fixture.",
    steps(prompt) { return [step(checkRequest(prompt), [textWire(held, false)], { wait: new Promise(() => {}) })]; } },
];
async function sockets() {
  return (await run("/usr/sbin/lsof", ["-nP", "-a", "-p", String(process.pid), `-iTCP:${new URL(server.baseUrl).port}`, "-FfT"]))
    .split("\n").filter(line => line.startsWith("TST=")).map(line => line.slice(4));
}
async function observePartial(runId, text) {
  const child = Bun.spawn([inputs.fabric.path, "--root", root, "runs", "logs", runId, "--raw", "stdout", "--follow", "--json"],
    { cwd: repo, env, stdin: "ignore", stdout: "pipe", stderr: "ignore" });
  commands.add(child);
  const timer = setTimeout(() => child.kill("SIGKILL"), 45000);
  let outer = "", raw = "", observed = "", size = 0;
  const decoder = new TextDecoder();
  try {
    for await (const bytes of child.stdout) {
      size += bytes.length; assert.ok(size <= 2097152, "Observer output ceiling exceeded");
      outer += decoder.decode(bytes, { stream: true });
      let newline;
      while ((newline = outer.indexOf("\n")) >= 0) {
        const line = outer.slice(0, newline); outer = outer.slice(newline + 1);
        if (!line.trim()) continue;
        const observation = JSON.parse(line); raw += observation.raw ?? "";
        let end;
        while ((end = raw.indexOf("\n")) >= 0) {
          const line = raw.slice(0, end); raw = raw.slice(end + 1);
          if (!line.trim()) continue;
          const event = JSON.parse(line);
          if (event.type === "acp.text") observed += event.text;
          if (observed.includes(text)) {
            assert.ok(!observation.terminal, "Stream was already terminal");
            await writeFile(join(fixture, `observed-${runId}.json`), JSON.stringify({ run_id: runId, raw_acp_text: observed, terminal: false }));
            return;
          }
        }
        assert.ok(!observation.terminal, "Run terminated before the held raw ACP text");
      }
    }
    throw new Error("No held raw ACP text before observer EOF/watchdog");
  } finally { clearTimeout(timer); commands.delete(child); child.kill(); await child.exited; }
}
async function collect(batchId, runId) {
  const result = await cli(["results", batchId, "--json"]);
  assert.ok(result.terminal === true && result.tasks.length === 1, "Expected one terminal task");
  const task = result.tasks[0], report = task.result, show = await cli(["runs", "show", runId, "--json"]);
  assert.ok(task.run_id === runId && report?.run_id === runId && show.run.run_id === runId && show.run.batch_id === batchId && show.run.harness === "acp", "Run ownership mismatch");
  assert.ok(show.run.state === task.state && report.state === task.state, "Terminal states disagree");
  assert.equal(result.succeeded, task.state === "SUCCEEDED");
  assert.ok(task.source.prepared_state_id === prepared.state_id && task.source.base_commit_sha === prepared.source_commit_sha, "Wrong prepared source");
  assert.ok(show.workspace.run_id === runId && show.workspace.workspace_id === report.workspace_id && show.workspace.managed_path === report.workspace_path, "Workspace ownership mismatch");
  assert.equal(show.workspace.state, "RECLAIMED");
  await assert.rejects(stat(report.workspace_path), { code: "ENOENT" });
  assert.ok(report.cleanup?.action === "reclaimed" && report.cleanup.run_id === runId && report.cleanup.workspace_id === report.workspace_id, "Missing reclaim evidence");
  const receipt = show.launch_receipt;
  assert.ok(receipt?.run_id === runId && receipt.workspace_id === report.workspace_id && receipt.workspace_path === report.workspace_path, "Missing native launch receipt");
  assert.ok(receipt.prepared_state_id === prepared.state_id && receipt.source_commit_sha === prepared.source_commit_sha && receipt.actual_sha256 === receipt.source.prepared_sha256, "Launch source mismatch");
  assert.ok(receipt.workspace_device != null && receipt.workspace_inode != null, "Missing workspace identity");
  assert.equal(show.worker_attempts.length, 1, "Unexpected worker restart");
  const attempt = show.worker_attempts[0];
  assert.ok(attempt.attempt_id === receipt.attempt_id && attempt.run_id === runId && attempt.started_at != null && attempt.finished_at != null && (attempt.exit_code != null || attempt.exit_signal != null), "Missing child wait status");
  assert.equal(attempt.harness_version, VERSION, "Wrong generic ACP bridge version");
  const pid = /^local-process-group:([1-9][0-9]*)$/.exec(show.run.pid_metadata ?? "");
  assert.ok(pid, "Missing owned process group");
  assert.throws(() => process.kill(-Number(pid[1]), 0), { code: "ESRCH" }, "Native process group survived");
  assert.ok(report.capture && !report.capture.truncated && report.capture.event_counts.invalid === 0 && report.capture.event_counts.oversized === 0, "Incomplete raw/native capture");
  assert.ok(report.artifact_manifest_id && task.artifacts.length > 0, "Missing artifact manifest");
  const contents = new Map(); let total = 0;
  for (const artifact of task.artifacts) {
    const path = resolve(root, artifact.relative_path);
    assert.ok(path.startsWith(`${root}${sep}`), "Artifact escaped storage");
    total += artifact.size_bytes;
    assert.ok(artifact.size_bytes >= 0 && total <= 16777216, "Artifact ceiling exceeded");
    assert.equal((await stat(path)).size, artifact.size_bytes);
    const bytes = await readFile(path);
    assert.ok(bytes.length === artifact.size_bytes && sha256(bytes) === artifact.sha256, "Artifact size/hash mismatch");
    assert.ok(!contents.has(artifact.kind), "Duplicate artifact kind"); contents.set(artifact.kind, bytes);
  }
  const manifest = JSON.parse(contents.get("manifest").toString());
  assert.ok(manifest.run_id === runId && manifest.workspace_id === report.workspace_id && manifest.prepared_state_id === prepared.state_id && manifest.base_commit_sha === prepared.source_commit_sha, "Manifest provenance mismatch");
  assert.equal(await readFile(join(repo, target), "utf8"), original, "Fixture source changed");
  assert.deepEqual(await sourceSnapshot(repo), { status: "", diff: sha256("") }, "Fixture source checkout changed");
  const db = new Database(join(root, "fabric.db"), { readonly: true });
  try { assert.equal(db.query("SELECT state FROM workspaces WHERE workspace_id=?").get(report.workspace_id)?.state, "RECLAIMED"); }
  finally { db.close(); }
  return { task, report, contents, summary: { batch_id: batchId, run_id: runId, state: task.state, launch_receipt: receipt,
    worker_attempt: attempt, workspace_reclaimed: true, process_group_reaped: true,
    artifacts: task.artifacts.map(({ kind, sha256, size_bytes }) => ({ kind, sha256, size_bytes })) } };
}
function checkOutcome({ task, report, contents }, scenario) {
  assert.equal(task.state, scenario.state, `${scenario.name}: wrong actual terminal state`);
  const raw = jsonl(contents.get("stdout").toString());
  const events = jsonl(contents.get("events").toString()).map(record => record.event).filter(Boolean);
  const output = events.filter(event => event.type === "output").map(event => event.text).join("");
  if (["incomplete-disconnect", "cancel-held-stream"].includes(scenario.name)) {
    const expected = scenario.state === "CANCELLED" ? held : partial;
    assert.ok(output.includes(expected) && raw.filter(event => event.type === "acp.text").map(event => event.text).join("").includes(expected),
      "Held text missing from retained raw/normalized stream");
  }
  if (scenario.state === "SUCCEEDED") {
    assert.ok(report.exit_code === 0 && !report.error_kind && !report.capture.native_failure && !task.error, "Success contains failure");
    assert.equal(raw.filter(event => event.type === "acp.stop" && event.reason === "end_turn").length, 1, "Missing unique real ACP end_turn");
    assert.ok(raw.filter(event => event.type === "acp.text").map(event => event.text).join("").includes(sentinel), "Missing raw completion text");
    assert.ok(output.includes(sentinel) && !events.some(event => event.type === "failure") && !raw.some(event => event.type === "acp.failure"), "Missing normalized completion or unexpected failure");
  } else {
    assert.ok(!output.includes(sentinel) && !raw.some(event => event.type === "acp.stop"), "Failed/cancelled turn fabricated success");
    assert.ok(!events.some(event => event.type === "completed" && event.success === true), "Normalized false success");
    if (scenario.state === "FAILED") assert.ok(report.capture.native_failure || report.error_kind || report.exit_code !== 0 || events.some(event => event.type === "failure"), "Missing actual failure evidence");
  }
  if (scenario.name === "native-write") {
    const tools = events.filter(event => event.type === "tool");
    for (const call of [1, 2]) {
      assert.ok(tools.some(event => event.name.startsWith(`acp.tool.${call}.`) && event.phase === "start"), "Missing actual native tool start");
      assert.ok(tools.some(event => event.name === `acp.tool.${call}.completed` && event.phase === "end"), "Missing actual successful native tool end");
    }
    assert.ok(tools.every(event => /^acp\.tool\.[12]\./.test(event.name)), "Unexpected extra native tools");
  }
}
try {
  sourceBefore = await sourceSnapshot(source);
  await run("/usr/bin/git", ["clone", "--quiet", "--no-local", "--no-hardlinks", source, repo]);
  assert.deepEqual(await sourceSnapshot(repo), { status: "", diff: sha256("") });
  // Refuse project settings/hooks/plugins rather than overriding or executing them.
  const tracked = (await run("/usr/bin/git", ["ls-files", "-z"], repo)).split("\0");
  assert.ok(!tracked.some(path => /(^|\/)\.claude(?:\/|\.json$)|(^|\/)\.mcp\.json$/.test(path)), "Source contains native config; review isolation before running");
  original = await readFile(join(repo, target), "utf8");
  const steps = scenarios.flatMap(scenario => scenario.steps(scenario.prompt));
  await writeFile(join(fixture, "provider-response-plan.json"), JSON.stringify(steps.map(({ method, path, status, headers, chunks, disconnect }) =>
    ({ method, path, status, headers, chunks, disconnect })), null, 2));
  server = await startScriptedProvider(steps);
  assert.match(server.baseUrl, /^http:\/\/127\.0\.0\.1:[1-9][0-9]*$/);
  const settingsPath = join(nativeHome, ".claude", "settings.json");
  await writeFile(settingsPath, JSON.stringify({ model, promptSuggestionEnabled: false, autoMemoryEnabled: false,
    permissions: { defaultMode: "dontAsk", disableBypassPermissionsMode: "disable", allow: ["Read(./README.md)", "Edit(./README.md)"],
      deny: ["Bash", "WebFetch", "WebSearch", "Agent", "Task", "NotebookEdit"] }, enabledPlugins: {} }));
  await writeFile(join(nativeHome, ".claude.json"), JSON.stringify({ hasCompletedOnboarding: true, autoUpdates: false }));
  nodeVersion = (await run(inputs.node.path, ["--version"])).trim();
  const [major, minor] = nodeVersion.slice(1).split(".").map(Number);
  assert.ok(major > 22 || (major === 22 && minor >= 18), "Node >=22.18 required");
  // Even version probes use private HOME, dummy auth, and the fixture endpoint.
  const probeEnv = { HOME: nativeHome, LC_ALL: "C", PATH: "/usr/bin:/bin", TMPDIR: join(nativeHome, "tmp"),
    CLAUDE_CONFIG_DIR: join(nativeHome, ".claude"), ANTHROPIC_API_KEY: key, ANTHROPIC_BASE_URL: server.baseUrl,
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1", DISABLE_TELEMETRY: "1", DISABLE_AUTOUPDATER: "1",
    DISABLE_UPDATES: "1", DISABLE_ERROR_REPORTING: "1", DISABLE_GROWTHBOOK: "1", OTEL_SDK_DISABLED: "true" };
  nativeVersion = (await run(inputs.claude.path, ["--version"], nativeHome, 30, probeEnv)).trim();
  assert.equal(nativeVersion, inputs.claudeVersion, "Selected native CLI version changed");
  shimVersion = (await run(inputs.shim.path, ["--version"], nativeHome, 30, probeEnv)).trim();
  assert.equal(shimVersion, "0.112.0", "Official reviewed ACP release required");
  const config = { executable: inputs.node, args: [inputs.bootstrap.path, inputs.shim.path, inputs.claude.path, server.baseUrl],
    nativeHome, cwd: "fabric-workspace", requestTimeoutMs: 30000,
    pins: [inputs.bootstrap, inputs.shim, inputs.claude, { path: settingsPath, sha256: fileHash(settingsPath) }] };
  const configPath = join(fixture, "launch-input.json"), pkg = join(fixture, "acp-package");
  await writeFile(configPath, JSON.stringify(config));
  await run(inputs.node.path, [join(source, "plugins/acp/package.mjs"), configPath, pkg, join(fixture, "acp.tgz")]);
  const acp = install(pkg, { registry, fabric: inputs.fabric.path, version: "0.1.0", sha256: directory(pkg).hash, trust: true });
  await symlink(join(registry, "packages", acp.sha256, "fabric-acp-bridge"), join(bin, "fabric-acp-bridge"));
  const local = reference("local", join(fixture, "local-package"), inputs.localPlugin.path);
  install(local, { registry, fabric: inputs.fabric.path, version: "0.1.0", sha256: directory(local).hash, trust: true });
  const recipe = join(fixture, "prepare.toml");
  await writeFile(recipe, 'version=1\n[[steps]]\nargv=["/usr/bin/true"]\n');
  await cli(["project", "add", "--name", "scripted", "--path", repo, "--json"]);
  prepared = await cli(["project", "prepare", "scripted", "--recipe", recipe, "--json"]);
  assert.equal(prepared.status, "READY");
  for (const scenario of scenarios) {
    phase = scenario.name;
    const before = server.requests.length;
    let batchId, runId, collected, settled = false;
    const scenarioErrors = [];
    try {
      const request = join(fixture, `${scenario.name}.json`);
      await writeFile(request, JSON.stringify({ project: "scripted", placement: "local", max_concurrency: 1, tasks: [{
        task_id: scenario.name, harness_id: "acp", prompt: scenario.prompt,
        execution_context: { kind: "explicit", harness_version: VERSION, native_config_refs: [{ kind: "native-home", path: nativeHome }], connection_refs: [] },
        source_ref: { kind: "prepared", state_id: prepared.state_id },
        limits: { timeout_seconds: 90, copy_limit_bytes: 67108864, max_log_bytes: 1048576, max_artifact_bytes: 16777216 },
      }] }));
      const accepted = await cli(["workers", "spawn", "--request", request, "--json"]);
      batchId = accepted.batch_id; if (batchId) batches.push(batchId);
      assert.ok(accepted.accepted === true && batchId && accepted.tasks.length === 1, "Worker not admitted");
      runId = accepted.tasks[0].run_id; assert.ok(runId, "Missing admitted run ID");
      if (scenario.name === "incomplete-disconnect") { await observePartial(runId, partial); releaseDisconnect(); }
      if (scenario.state === "CANCELLED") {
        await observePartial(runId, held);
        assert.equal(server.requests.length - before, 1, "Cancel preceded the provider request");
        assert.ok((await sockets()).includes("ESTABLISHED"), "No held provider socket");
        await cli(["cancel", batchId, "--json"]);
      }
      assert.equal((await cli(["wait", batchId, "--timeout-seconds", "90", "--json"])).terminal, true);
      settled = true; collected = await collect(batchId, runId); checkOutcome(collected, scenario);
      assert.equal(server.requests.length - before, scenario.requests, "Unexpected retry/fallback/request count");
      if (scenario.name === "native-write") {
        const patch = collected.contents.get("tracked_patch");
        assert.ok(patch?.length && patch.toString().includes(target), "Missing captured native write patch");
        assert.equal(collected.contents.get("untracked_paths")?.length, 0, "Unexpected native untracked files");
        const recovery = join(fixture, "restored-write"), patchPath = join(fixture, "verified-write.patch");
        await run("/usr/bin/git", ["clone", "--quiet", "--no-local", "--no-hardlinks", repo, recovery]);
        await run("/usr/bin/git", ["checkout", "--quiet", "--detach", prepared.source_commit_sha], recovery);
        await writeFile(patchPath, patch); await run("/usr/bin/git", ["apply", "--binary", patchPath], recovery);
        assert.equal(await readFile(join(recovery, target), "utf8"), changed, "Restored patch lost native mutation");
        assert.equal((await run("/usr/bin/git", ["diff", "--name-only"], recovery)).trim(), target, "Other tracked files changed");
        collected.summary.patch_restored = true;
      }
      if (scenario.state === "CANCELLED") {
        assert.deepEqual(await sockets(), ["LISTEN"], "Held stream survived cancellation");
        collected.summary.stream_observed_before_cancel = true; collected.summary.server_socket_closed = true;
      }
    } catch (error) { scenarioErrors.push(error); }
    finally {
      if (scenario.name === "incomplete-disconnect") releaseDisconnect();
      cleaning = true;
      if (batchId && !settled) {
        try { await cli(["cancel", batchId, "--json"]);
          assert.equal((await cli(["wait", batchId, "--timeout-seconds", "45", "--json"])).terminal, true); settled = true;
        } catch (error) { scenarioErrors.push(error); }
      }
      if (settled && !collected && runId) {
        try { collected = await collect(batchId, runId); } catch (error) { scenarioErrors.push(error); }
      }
      cleaning = false;
      proof.push({ scenario: scenario.name, outcome: scenarioErrors.length ? "FAIL" : "PASS", model_requests: server.requests.length - before,
        batch_id: batchId, run_id: runId, ...collected?.summary });
    }
    // One exact ordered server: a failed turn cannot shift later scenarios into bogus passes.
    if (scenarioErrors.length) throw new AggregateError(scenarioErrors, `Scenario ${scenario.name} failed`);
  }
} catch (error) { errors.push(error); }
finally {
  cleaning = true; releaseDisconnect();
  if (await Bun.file(join(root, "fabric.db")).exists()) {
    for (const batch of batches) {
      try { await cli(["cancel", batch, "--json"]);
        assert.equal((await cli(["wait", batch, "--timeout-seconds", "45", "--json"])).terminal, true);
      } catch (error) { errors.push(error); }
    }
    try { await cli(["manager", "stop", "--json"]); } catch (error) { errors.push(error); }
  }
  if (server) {
    try { await writeFile(join(fixture, "provider-requests.json"), JSON.stringify(server.requests, null, 2)); }
    catch (error) { errors.push(error); }
    try { server.assertDone(); } catch (error) { errors.push(error); }
    try { await server.close(); } catch (error) { errors.push(error); }
  }
  if (sourceBefore) {
    try { assert.deepEqual(await sourceSnapshot(source), sourceBefore, "Original checkout changed"); }
    catch (error) { errors.push(error); }
  }
  if (emergency) errors.push(new Error("Whole-smoke emergency watchdog expired"));
  clearTimeout(watchdog);
}
const result = { result: errors.length || proof.length !== scenarios.length ? "FAIL" : "PASS", evidence_root: fixture,
  generic_acp_only: true, os_egress_confined_by_script: false, raw_stream: "Fabric ACP bridge JSONL, not private native SDK transcripts",
  inputs, native_version: nativeVersion ?? null, shim_version: shimVersion ?? null, node_version: nodeVersion ?? null,
  source_commit_sha: prepared?.source_commit_sha, prepared_state_id: prepared?.state_id, scenarios: proof,
  ...(errors.length ? { failed_phase: phase, failure_evidence: "failure.txt" } : {}) };
if (errors.length) {
  const describe = error => error instanceof AggregateError ? `${error.stack}\n${error.errors.map(describe).join("\n")}` : String(error?.stack ?? error);
  await writeFile(join(fixture, "failure.txt"), errors.map(describe).join("\n"));
}
await writeFile(join(fixture, "proof.json"), JSON.stringify(result, null, 2));
console.log(JSON.stringify(result));
if (result.result !== "PASS") process.exitCode = 1;
