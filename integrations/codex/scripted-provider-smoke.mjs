// Bun/macOS only. Usage: bun integrations/codex/scripted-provider-smoke.mjs /absolute/FABRIC /absolute/CODEX /absolute/REFERENCE_PLUGIN
// Supply the accepted Fabric CLI, its prebuilt reference plugin (no SDK source bootstrap), and private codex-cli 0.171.0.
// Manager must checksum-verify the matching official rust-v0.171.0 macOS archive:
// arm64: codex-aarch64-apple-darwin.tar.gz SHA256 9414c8c967f110b11e8324c0cfa2b20f012094ef890110a561c92ee535a2681a.
// x64: codex-x86_64-apple-darwin.tar.gz with the manager's independent archive pin (not locally exercised).
// Requires Bun, /usr/bin/git and /usr/sbin/lsof. Never installs/downloads or uses inherited auth/config.
// Config/wire contract: https://github.com/openai/codex/tree/rust-v0.171.0/codex-rs
// See models-manager/models.json, models-manager/src/model_info.rs, core/src/tools/spec_plan.rs,
// core/src/tools/handlers/apply_patch_spec.rs, and features/src/lib.rs. Endpoint isolation is NOT OS egress confinement;
// machine-managed Codex policy and native sandbox availability remain manager-run prerequisites.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { access, mkdir, mkdtemp, readFile, realpath, stat, symlink, writeFile } from "node:fs/promises";
import { isAbsolute, join, resolve, sep } from "node:path";
import { Database } from "bun:sqlite";
import { startScriptedProvider } from "../../scripts/scripted-provider.mjs";
import { reference, install } from "../../sdk/typescript/cli.mjs";
import { directory } from "../../sdk/typescript/package.mjs";

assert.equal(process.argv.length, 5, "Usage: bun integrations/codex/scripted-provider-smoke.mjs /absolute/FABRIC /absolute/CODEX /absolute/REFERENCE_PLUGIN");
assert.equal(process.platform, "darwin", "Native sandbox/socket proof requires macOS");
assert.ok(["arm64", "x64"].includes(process.arch), "Pinned private releases support macOS arm64 and x64");
const [fabric, codex, referencePlugin] = await Promise.all(process.argv.slice(2).map(async path => {
  assert.ok(isAbsolute(path), "Binary arguments must be absolute paths");
  await access(path, constants.X_OK);
  assert.ok((await stat(path)).isFile(), "Binary argument must name an executable file");
  return realpath(path);
}));
await access("/usr/bin/git", constants.X_OK);
await access("/usr/sbin/lsof", constants.X_OK);
process.umask(0o077);
const fixture = await realpath(await mkdtemp("/private/tmp/fabric-scripted-codex-"));
const repo = join(fixture, "source"), root = join(fixture, "storage"), bin = join(fixture, "bin");
const home = join(fixture, "home"), codexHome = join(fixture, "codex");
for (const path of [repo, bin, home, codexHome, ...["xdg-config", "xdg-data", "xdg-cache", "xdg-state", "tmp"].map(name => join(fixture, name))]) {
  await mkdir(path, { recursive: true, mode: 0o700 });
}
await symlink(codex, join(bin, "codex"));
await symlink(process.execPath, join(bin, "bun"));
// An allowlist, not {...process.env}: no host auth, proxies, model endpoints, or native configuration.
const env = {
  PATH: `${bin}:/usr/bin:/bin`, HOME: home, CODEX_HOME: codexHome,
  XDG_CONFIG_HOME: join(fixture, "xdg-config"), XDG_DATA_HOME: join(fixture, "xdg-data"),
  XDG_CACHE_HOME: join(fixture, "xdg-cache"), XDG_STATE_HOME: join(fixture, "xdg-state"), TMPDIR: join(fixture, "tmp"),
  LC_ALL: "C", OTEL_SDK_DISABLED: "true",
  GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0",
  GIT_AUTHOR_NAME: "Fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid",
  GIT_COMMITTER_NAME: "Fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
};
// rust-v0.171.0 bundles gpt-5.5 with freeform apply_patch and no forced code mode.
// Unknown slugs fall back to metadata without apply_patch; the removed apply_patch_freeform flag cannot enable it.
const model = "gpt-5.5", providerName = "fabric-scripted", key = "fabric-fixture-only-not-a-secret";
const original = "original\n", changed = "scripted native write\n";
const sentinel = "FABRIC_SCRIPTED_COMPLETE", held = "FABRIC_SCRIPTED_HELD", incomplete = "FABRIC_SCRIPTED_INCOMPLETE";
const patchInput = "*** Begin Patch\n*** Update File: tracked.txt\n@@\n-original\n+scripted native write\n*** End Patch\n";
const artifactLimit = 10485760;
async function configure(path, baseUrl) {
  assert.match(baseUrl, /^http:\/\/127\.0\.0\.1:[1-9][0-9]*$/);
  await mkdir(path, { recursive: true, mode: 0o700 });
  // Only this fixture-created dummy credential exists. Fabric checks metadata, not auth contents.
  await writeFile(join(path, "auth.json"), JSON.stringify({ OPENAI_API_KEY: key }), { mode: 0o600 });
  await writeFile(join(path, "config.toml"), `model = "${model}"
model_provider = "${providerName}"
model_context_window = 200000
approval_policy = "never"
cli_auth_credentials_store = "file"
check_for_update_on_startup = false
web_search = "disabled"
allow_login_shell = false
[model_providers.${providerName}]
name = "Fabric scripted loopback"
base_url = "${baseUrl}/v1"
wire_api = "responses"
experimental_bearer_token = "${key}"
requires_openai_auth = false
supports_websockets = false
supports_standalone_web_search = false
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 60000
[features]
responses_websockets = false
responses_websockets_v2 = false
system_proxy_fallback = false
api_key_model_discovery = false
unbounded_connection_retries = false
respect_system_proxy = false
apps = false
plugins = false
code_mode = false
code_mode_only = false
multi_agent = false
multi_agent_v2 = false
shell_tool = false
unified_exec = false
apply_patch_streaming_events = false
[agents]
enabled = false
[shell_environment_policy]
inherit = "none"
[sandbox_workspace_write]
network_access = false
[analytics]
enabled = false
[feedback]
enabled = false
[otel]
exporter = "none"
trace_exporter = "none"
metrics_exporter = "none"
`, { mode: 0o600 });
}
let commandNumber = 0, phase = "setup", prepared, nativeVersion;
const proof = [], batches = [], errors = [];
const sha256 = bytes => createHash("sha256").update(bytes).digest("hex");
async function run(program, args, cwd = repo, seconds = 120) {
  const prefix = join(fixture, `command-${++commandNumber}`);
  const child = Bun.spawn([program, ...args], { cwd, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  let expired = false, overflow = false;
  const watchdog = setTimeout(() => { expired = true; child.kill("SIGKILL"); }, seconds * 1000);
  async function capture(stream) {
    const chunks = []; let size = 0;
    for await (const chunk of stream) {
      size += chunk.length;
      if (size <= 16 * 1024 * 1024) chunks.push(chunk);
      else { overflow = true; child.kill("SIGKILL"); }
    }
    return Buffer.concat(chunks).toString();
  }
  try {
    const [stdout, stderr, code] = await Promise.all([capture(child.stdout), capture(child.stderr), child.exited]);
    await writeFile(`${prefix}.json`, JSON.stringify({ program, args, cwd, code, watchdog_expired: expired, output_limit_exceeded: overflow }));
    await writeFile(`${prefix}.stdout`, stdout); await writeFile(`${prefix}.stderr`, stderr);
    assert.ok(!expired && !overflow, `Command exceeded emergency time/output bound; private evidence: ${prefix}`);
    assert.equal(code, 0, `Command failed; private evidence: ${prefix}`);
    return stdout;
  } finally { clearTimeout(watchdog); }
}
const cli = async args => JSON.parse(await run(fabric, ["--root", root, ...args]));
function checkRequest(prompt, continuation = false) {
  return ({ method, path, headers, body }) => {
    assert.ok(method === "POST" && path === "/v1/responses", "Unexpected model route");
    assert.ok(headers.authorization === `Bearer ${key}`, "Only fixture Authorization is allowed");
    for (const name of ["x-api-key", "api-key", "cookie", "proxy-authorization"]) assert.ok(!headers[name], "Unexpected credential header");
    assert.ok(body.model === model && body.stream === true, "Wrong model or non-streaming request");
    assert.ok(Array.isArray(body.input), "Missing Responses conversation");
    assert.ok(body.input.some(item => item.role === "user" && JSON.stringify(item.content).includes(prompt)), "Missing fixture prompt");
    const calls = body.input.filter(item => ["custom_tool_call", "function_call"].includes(item.type));
    const results = body.input.filter(item => ["custom_tool_call_output", "function_call_output"].includes(item.type));
    if (continuation) {
      assert.ok(calls.length === 1 && calls[0].type === "custom_tool_call" && calls[0].call_id === "fixture_write" &&
        calls[0].name === "apply_patch" && calls[0].input === patchInput, "Native conversation lost or changed the apply_patch call");
      assert.ok(results.length === 1 && results[0].type === "custom_tool_call_output" && results[0].call_id === "fixture_write", "Missing actual native tool result");
      const output = typeof results[0].output === "string" ? results[0].output : JSON.stringify(results[0].output);
      assert.ok(output.includes("Success. Updated the following files:") && output.includes("M tracked.txt"), "Native patch did not report a successful tracked-file update");
    } else {
      assert.equal(calls.length, 0, "Unexpected earlier tool call");
      assert.equal(results.length, 0, "Unexpected earlier tool result");
    }
  };
}
function frame(type, data = {}) { return `event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`; }
const sseHeaders = { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "close" };
const message = (text, phase) => ({ id: "msg_fixture", type: "message", role: "assistant", phase, status: "completed", content: [{ type: "output_text", text }] });
function textChunks(text, complete = true) {
  const item = message(text, complete ? "final_answer" : "commentary");
  const delta = frame("response.output_text.delta", { item_id: item.id, output_index: 0, content_index: 0, delta: text.slice(0, 7) });
  return [frame("response.created", { response: { id: "resp_fixture" } }),
    frame("response.output_item.added", { output_index: 0, item: { ...item, status: "in_progress", content: [] } }),
    // Split inside an SSE JSON frame as well as splitting the text across distinct delta events.
    delta.slice(0, 23), delta.slice(23),
    frame("response.output_text.delta", { item_id: item.id, output_index: 0, content_index: 0, delta: text.slice(7) }),
    frame("response.output_item.done", { output_index: 0, item }),
    ...(complete ? [completed(true)] : [])];
}
function completed(endTurn) {
  return frame("response.completed", { response: { id: "resp_fixture", end_turn: endTurn,
    usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15 } } });
}
function toolChunks() {
  const call = { type: "custom_tool_call", id: "ctc_fixture", call_id: "fixture_write", name: "apply_patch", input: patchInput };
  return [frame("response.created", { response: { id: "resp_fixture" } }),
    frame("response.output_item.added", { output_index: 0, item: { ...call, input: "" } }),
    frame("response.custom_tool_call_input.delta", { output_index: 0, item_id: call.id, delta: patchInput }),
    frame("response.output_item.done", { output_index: 0, item: call }), completed(false)];
}
const jsonl = text => text.split("\n").filter(line => line.trim()).map(line => JSON.parse(line));
async function sockets(baseUrl) {
  const output = await run("/usr/sbin/lsof", ["-nP", "-a", "-p", String(process.pid), `-iTCP:${new URL(baseUrl).port}`, "-FfT"]);
  return output.split("\n").filter(line => line.startsWith("TST=")).map(line => line.slice(4)).sort();
}
async function observePartial(runId, text) {
  const child = Bun.spawn([fabric, "--root", root, "runs", "logs", runId, "--raw", "stdout", "--follow", "--json"],
    { cwd: repo, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const stderr = new Response(child.stderr).text();
  const watchdog = setTimeout(() => child.kill("SIGKILL"), 45000);
  let outer = "", raw = "", observed = "", size = 0;
  try {
    const decoder = new TextDecoder();
    for await (const bytes of child.stdout) {
      size += bytes.length;
      assert.ok(size <= 2 * 1024 * 1024, "Raw observation exceeded 2 MiB");
      const chunk = decoder.decode(bytes, { stream: true }); observed += chunk; outer += chunk;
      let newline;
      while ((newline = outer.indexOf("\n")) >= 0) {
        const line = outer.slice(0, newline); outer = outer.slice(newline + 1);
        if (!line.trim()) continue;
        const observation = JSON.parse(line);
        assert.ok(!observation.terminal, "Native turn terminated before observing the held response");
        raw += observation.raw ?? "";
        let end;
        while ((end = raw.indexOf("\n")) >= 0) {
          const nativeLine = raw.slice(0, end); raw = raw.slice(end + 1);
          if (!nativeLine.trim()) continue;
          const event = JSON.parse(nativeLine);
          assert.ok(event.type !== "turn.completed", "Native turn completed before fixture release");
          // exec --json omits text deltas. A message item can complete while its Responses turn is still open.
          if (event.type === "item.completed" && event.item?.type === "agent_message" && event.item.text === text) return;
        }
      }
    }
    throw new Error("Native stdout ended before the held message item (or emergency watchdog fired)");
  } finally {
    clearTimeout(watchdog); child.kill();
    const [, diagnostic] = await Promise.all([child.exited, stderr]);
    await writeFile(join(fixture, `observer-${runId}.stdout`), observed);
    await writeFile(join(fixture, `observer-${runId}.stderr`), diagnostic);
  }
}
async function collect(batchId, runId) {
  const result = await cli(["results", batchId, "--json"]);
  assert.ok(result.terminal === true && result.tasks.length === 1, "Batch is not terminal with exactly one task");
  const task = result.tasks[0], report = task.result;
  assert.ok(task.run_id === runId && report?.run_id === runId, "Run ownership mismatch");
  const show = await cli(["runs", "show", runId, "--json"]);
  assert.ok(show.run.batch_id === batchId && show.run.run_id === runId && show.run.harness === "codex", "Wrong batch/harness owner");
  assert.ok(show.run.state === task.state && report.state === task.state, "Terminal states disagree");
  assert.equal(result.succeeded, task.state === "SUCCEEDED", "Batch success disagrees with actual run state");
  assert.ok(task.source.prepared_state_id === prepared.state_id && task.source.base_commit_sha === prepared.source_commit_sha, "Prepared source mismatch");
  assert.ok(show.workspace.run_id === runId && show.workspace.workspace_id === report.workspace_id, "Workspace ownership mismatch");
  assert.equal(show.workspace.state, "RECLAIMED", "Workspace was not reclaimed");
  assert.equal(show.workspace.managed_path, report.workspace_path, "Workspace path mismatch");
  await assert.rejects(stat(report.workspace_path), { code: "ENOENT" });
  assert.ok(report.cleanup?.action === "reclaimed" && report.cleanup.run_id === runId && report.cleanup.workspace_id === report.workspace_id, "Missing terminal reclaim evidence");
  const receipt = show.launch_receipt;
  assert.ok(receipt?.run_id === runId && receipt.workspace_id === report.workspace_id && receipt.workspace_path === report.workspace_path, "Missing native launch ownership receipt");
  assert.ok(receipt.prepared_state_id === prepared.state_id && receipt.source_commit_sha === prepared.source_commit_sha, "Launch source mismatch");
  assert.ok(receipt.workspace_device != null && receipt.workspace_inode != null && receipt.actual_sha256 === receipt.source.prepared_sha256, "Missing verified workspace identity");
  assert.ok(show.worker_attempts.length === 1, "Unexpected worker attempts/restart");
  const attempt = show.worker_attempts[0];
  assert.ok(attempt.run_id === runId && attempt.attempt_id === receipt.attempt_id && attempt.started_at != null && attempt.finished_at != null, "Native child was not observed started and finished");
  assert.ok(attempt.exit_code != null || attempt.exit_signal != null, "Native child has no observed wait status");
  assert.equal(attempt.harness_version, nativeVersion, "Wrong native version");
  const pid = /^local-process-group:([1-9][0-9]*)$/.exec(show.run.pid_metadata ?? "");
  assert.ok(pid, "Missing native process-group ownership");
  assert.throws(() => process.kill(-Number(pid[1]), 0), { code: "ESRCH" }, "Native process group still exists");
  assert.ok(report.capture && !report.capture.truncated && report.capture.event_counts.invalid === 0 && report.capture.event_counts.oversized === 0, "Incomplete native capture");
  assert.ok(report.artifact_manifest_id && task.artifacts.length > 0, "No registered artifact manifest");
  const contents = new Map(); let artifactBytes = 0;
  for (const artifact of task.artifacts) {
    const path = resolve(root, artifact.relative_path);
    assert.ok(path.startsWith(`${root}${sep}`), "Artifact escaped managed storage");
    artifactBytes += artifact.size_bytes;
    assert.ok(Number.isSafeInteger(artifact.size_bytes) && artifact.size_bytes >= 0 && artifactBytes <= artifactLimit, "Artifacts exceeded fixture bound");
    assert.equal((await stat(path)).size, artifact.size_bytes, "Artifact size mismatch");
    const bytes = await readFile(path);
    assert.ok(bytes.length === artifact.size_bytes && sha256(bytes) === artifact.sha256, "Artifact checksum mismatch");
    assert.ok(!contents.has(artifact.kind), "Duplicate artifact kind");
    contents.set(artifact.kind, bytes);
  }
  const manifest = JSON.parse(contents.get("manifest").toString());
  assert.ok(manifest.run_id === runId && manifest.workspace_id === report.workspace_id && manifest.prepared_state_id === prepared.state_id && manifest.base_commit_sha === prepared.source_commit_sha, "Artifact manifest provenance mismatch");
  assert.equal(await readFile(join(repo, "tracked.txt"), "utf8"), original, "Source checkout changed");
  assert.equal((await run("/usr/bin/git", ["status", "--porcelain=v1", "--untracked-files=all"])).trim(), "", "Source checkout is dirty");
  assert.equal((await run("/usr/bin/git", ["rev-parse", "HEAD"])).trim(), prepared.source_commit_sha, "Source HEAD changed");
  const db = new Database(join(root, "fabric.db"), { readonly: true });
  try { assert.equal(db.query("SELECT state FROM workspaces WHERE workspace_id=?").get(report.workspace_id)?.state, "RECLAIMED"); }
  finally { db.close(); }
  return { task, report, contents, summary: { batch_id: batchId, run_id: runId, state: task.state,
    workspace_id: report.workspace_id, workspace_reclaimed: true, child_reaped: true, source_unchanged: true,
    artifacts: task.artifacts.map(({ kind, sha256, size_bytes }) => ({ kind, sha256, size_bytes })) } };
}
function checkOutcome(collected, scenario) {
  const { task, report, contents } = collected;
  assert.equal(task.state, scenario.state, `${scenario.name}: unexpected native terminal state`);
  const native = jsonl(contents.get("stdout").toString());
  const events = jsonl(contents.get("events").toString()).map(record => record.event).filter(Boolean);
  assert.ok(native.some(event => event.type === "thread.started") && native.some(event => event.type === "turn.started"), "Missing actual native thread/turn");
  const terminal = native.filter(event => event.type === "turn.completed");
  const output = events.filter(event => event.type === "output").map(event => event.text).join("");
  if (scenario.state === "SUCCEEDED") {
    assert.ok(report.exit_code === 0 && !report.error_kind && !report.capture.native_failure && !task.error, "Native success contains a failure");
    assert.equal(terminal.length, 1, "Missing unique real native turn.completed");
    assert.ok(native.some(event => event.type === "item.completed" && event.item?.type === "agent_message" && event.item.text === sentinel), "Missing raw native final text");
    assert.ok(output.includes(sentinel), "Completion sentinel missing from normalized output");
    assert.ok(events.some(event => event.type === "usage" && event.input_tokens === 10 * scenario.requests &&
      event.output_tokens === 5 * scenario.requests), "Missing normalized cumulative Responses usage");
    assert.ok(events.some(event => event.type === "completed" && event.success === true), "No successful native completion event");
    assert.ok(!events.some(event => event.type === "failure") && !native.some(event => ["turn.failed", "error"].includes(event.type)), "Unexpected native/normalized failure");
  } else {
    assert.equal(terminal.length, 0, "Failed/cancelled stream fabricated native turn completion");
    assert.ok(!output.includes(sentinel), "Failed/cancelled stream fabricated a completion sentinel");
    assert.ok(!events.some(event => event.type === "completed" && event.success === true), "Failed/cancelled stream fabricated normalized success");
    if (scenario.state === "FAILED") {
      assert.ok(native.some(event => ["turn.failed", "error"].includes(event.type)), "Missing raw native API/stream failure");
      assert.ok(report.capture.native_failure || report.error_kind || report.exit_code !== 0 || events.some(event => event.type === "failure"), "Missing actual failure evidence");
    }
    if (scenario.name === "incomplete-disconnect" || scenario.state === "CANCELLED") {
      const marker = scenario.state === "CANCELLED" ? held : incomplete;
      assert.ok(native.some(event => event.type === "item.completed" && event.item?.type === "agent_message" && event.item.text === marker) && output.includes(marker), "Observed partial item missing from retained raw/normalized capture");
    }
  }
}
const scenarios = [
  { name: "split-text", state: "SUCCEEDED", requests: 1, prompt: "Synthetic split-text fixture. Reply with the requested completion marker only.",
    steps: check => [{ method: "POST", path: "/v1/responses", check, headers: sseHeaders, chunks: textChunks(sentinel) }] },
  { name: "native-write", state: "SUCCEEDED", requests: 2, prompt: "Synthetic write fixture. Use apply_patch to replace original with scripted native write in tracked.txt, then report completion. No other tools or files.",
    steps: (check, next) => [{ method: "POST", path: "/v1/responses", check: request => {
      check(request);
      assert.ok(request.body.tools?.some(tool => tool.type === "custom" && tool.name === "apply_patch"), "Native freeform apply_patch is unavailable");
    }, headers: sseHeaders, chunks: toolChunks() },
    { method: "POST", path: "/v1/responses", check: next, headers: sseHeaders, chunks: textChunks(sentinel) }] },
  { name: "api-error", state: "FAILED", requests: 1, prompt: "Synthetic non-retryable API failure fixture.",
    steps: check => [{ method: "POST", path: "/v1/responses", check, status: 401, headers: { "content-type": "application/json" },
      chunks: [JSON.stringify({ error: { message: "Synthetic fixture denial", type: "invalid_request_error", code: "fixture_denied" } })] }] },
  { name: "incomplete-disconnect", state: "FAILED", requests: 1, prompt: "Synthetic incomplete Responses streaming fixture.",
    steps: (check, _next, gate) => [{ method: "POST", path: "/v1/responses", check, headers: sseHeaders,
      chunks: textChunks(incomplete, false), wait: gate, disconnect: true }] },
  { name: "cancel-held-stream", state: "CANCELLED", requests: 1, prompt: "Synthetic held Responses cancellation fixture.",
    steps: check => [{ method: "POST", path: "/v1/responses", check, headers: sseHeaders,
      chunks: textChunks(held, false), wait: new Promise(() => {}) }] },
];
try {
  // Even version/probe invocations see only private dummy-provider configuration.
  await configure(codexHome, "http://127.0.0.1:1");
  nativeVersion = (await run(codex, ["--version"])).trim();
  assert.equal(nativeVersion, "codex-cli 0.171.0", "Do not relax the native adapter's exact release pin");
  // Git operations below affect only this disposable fixture, never the implementation checkout.
  await run("/usr/bin/git", ["init", "--quiet"]);
  await writeFile(join(repo, "tracked.txt"), original);
  await run("/usr/bin/git", ["add", "tracked.txt"]);
  await run("/usr/bin/git", ["commit", "--quiet", "-m", "Disposable scripted provider fixture"]);
  env.FABRIC_PLUGIN_REGISTRY = join(fixture, "registry");
  await mkdir(env.FABRIC_PLUGIN_REGISTRY, { mode: 0o700 });
  const plugin = reference("codex", join(fixture, "reference-codex"), referencePlugin);
  install(plugin, { registry: env.FABRIC_PLUGIN_REGISTRY, fabric, version: "0.1.0", sha256: directory(plugin).hash, trust: true });
  const recipe = join(fixture, "prepare.toml");
  await writeFile(recipe, 'version=1\n[[steps]]\nargv=["/usr/bin/true"]\n');
  await cli(["project", "add", "--name", "scripted", "--path", repo, "--json"]);
  prepared = await cli(["project", "prepare", "scripted", "--recipe", recipe, "--json"]);
  assert.equal(prepared.status, "READY");
  for (const scenario of scenarios) {
    phase = scenario.name;
    let releaseDisconnect;
    const disconnectGate = new Promise(resolve => { releaseDisconnect = resolve; });
    const requestFailures = [];
    const steps = scenario.steps(checkRequest(scenario.prompt), checkRequest(scenario.prompt, true), disconnectGate);
    const server = await startScriptedProvider(steps.map((step, index) => ({ ...step, check: async request => {
      try { await step.check?.(request); }
      catch (error) {
        // Only fixed assertion messages and structural counts/booleans; never headers, prompts, arguments or outputs.
        const tools = Array.isArray(request.body.tools) ? request.body.tools : [];
        requestFailures.push({ step: index + 1,
          assertion: error instanceof assert.AssertionError ? error.message.slice(0, 512) : "Request check threw a non-assertion error",
          model_matches: request.body.model === model, streaming: request.body.stream === true,
          input_items: Array.isArray(request.body.input) ? request.body.input.length : null, tool_count: tools.length,
          custom_tools: tools.filter(tool => tool.type === "custom").length,
          function_tools: tools.filter(tool => tool.type === "function").length,
          namespace_tools: tools.filter(tool => tool.type === "namespace").length,
          apply_patch_advertised: tools.some(tool => tool.type === "custom" && tool.name === "apply_patch") });
        throw error;
      }
    } })));
    let batchId, runId, collected, settled = false;
    const scenarioErrors = [];
    try {
      assert.match(server.baseUrl, /^http:\/\/127\.0\.0\.1:[1-9][0-9]*$/);
      const scenarioConfig = join(fixture, scenario.name, "codex");
      await configure(scenarioConfig, server.baseUrl);
      const request = join(fixture, `${scenario.name}.json`);
      await writeFile(request, JSON.stringify({ project: "scripted", placement: "local", max_concurrency: 1, tasks: [{
        task_id: scenario.name, harness_id: "codex", prompt: scenario.prompt,
        execution_context: { kind: "explicit", harness_version: nativeVersion,
          native_config_refs: [{ kind: "native-config-dir", path: scenarioConfig }],
          connection_refs: [{ kind: "native-auth", name: "native-config-dir" }] },
        source_ref: { kind: "prepared", state_id: prepared.state_id },
        limits: { timeout_seconds: 120, copy_limit_bytes: artifactLimit, max_log_bytes: 1048576, max_artifact_bytes: artifactLimit },
      }] }));
      const accepted = await cli(["workers", "spawn", "--request", request, "--json"]);
      // Record ownership before asserting admission details, so finally can cancel any admitted batch.
      batchId = accepted.batch_id;
      if (batchId) batches.push(batchId);
      assert.ok(accepted.accepted === true && batchId && accepted.tasks.length === 1, "Worker not admitted");
      runId = accepted.tasks[0].run_id;
      assert.ok(runId, "Admission omitted run ID");
      if (scenario.name === "incomplete-disconnect") {
        await observePartial(runId, incomplete);
        releaseDisconnect();
      }
      if (scenario.state === "CANCELLED") {
        await observePartial(runId, held);
        assert.equal(server.requests.length, 1, "Cancellation happened before the actual model request");
        assert.ok((await sockets(server.baseUrl)).includes("ESTABLISHED"), "Held response has no accepted server socket");
        await cli(["cancel", batchId, "--json"]);
      }
      assert.equal((await cli(["wait", batchId, "--timeout-seconds", "90", "--json"])).terminal, true, "Worker did not terminate");
      settled = true;
      collected = await collect(batchId, runId);
      checkOutcome(collected, scenario);
      assert.equal(server.requests.length, scenario.requests, "Unexpected actual model request count");
      if (scenario.name === "native-write") {
        const patch = collected.contents.get("tracked_patch");
        assert.ok(patch?.length && patch.toString().includes("tracked.txt"), "Missing captured native patch");
        assert.equal(collected.contents.get("untracked_paths")?.length, 0, "Native patch created unexpected untracked files");
        const native = jsonl(collected.contents.get("stdout").toString());
        const writes = native.filter(event => event.type === "item.completed" && event.item?.type === "file_change");
        assert.ok(writes.length === 1 && writes[0].item.status === "completed" && writes[0].item.changes.length === 1 &&
          writes[0].item.changes[0].kind === "update" && (writes[0].item.changes[0].path === "tracked.txt" ||
          writes[0].item.changes[0].path.endsWith("/tracked.txt")), "Missing actual successful native file_change");
        const tools = jsonl(collected.contents.get("events").toString()).map(record => record.event).filter(event => event?.type === "tool");
        // Compare the real native phases; never require a tool-start event absent from exec's capture.
        const nativeTools = native.filter(event => ["item.started", "item.updated", "item.completed"].includes(event.type) &&
          event.item?.type === "file_change").map(event => ({ type: "tool", name: "file_change",
          phase: { "item.started": "start", "item.updated": "update", "item.completed": "end" }[event.type] }));
        assert.deepEqual(tools, nativeTools, "Normalized tools disagree with the actual native file_change phases");
        const recovery = join(fixture, "recovered-write");
        await run("/usr/bin/git", ["clone", "--quiet", "--no-local", "--no-hardlinks", repo, recovery]);
        await run("/usr/bin/git", ["checkout", "--quiet", "--detach", prepared.source_commit_sha], recovery);
        const patchPath = join(fixture, "verified-write.patch"); await writeFile(patchPath, patch);
        await run("/usr/bin/git", ["apply", "--binary", patchPath], recovery);
        assert.equal(await readFile(join(recovery, "tracked.txt"), "utf8"), changed, "Artifact restore lost native write");
        assert.equal((await run("/usr/bin/git", ["diff", "--name-only"], recovery)).trim(), "tracked.txt", "Native tool edited other tracked files");
        collected.summary.patch_restored = true;
      }
      if (scenario.name === "incomplete-disconnect") collected.summary.stream_observed_before_disconnect = true;
      if (scenario.state === "CANCELLED") {
        assert.deepEqual(await sockets(server.baseUrl), ["LISTEN"], "Accepted server socket survived native cancellation");
        collected.summary.stream_observed_before_cancel = true;
        collected.summary.server_socket_closed = true;
      }
    } catch (error) { scenarioErrors.push(error); }
    finally {
      releaseDisconnect();
      if (batchId && !settled) {
        try {
          await cli(["cancel", batchId, "--json"]);
          assert.equal((await cli(["wait", batchId, "--timeout-seconds", "60", "--json"])).terminal, true, "Cleanup left worker nonterminal");
          settled = true;
        } catch (error) { scenarioErrors.push(error); }
      }
      if (settled && !collected && runId) {
        try { collected = await collect(batchId, runId); } catch (error) { scenarioErrors.push(error); }
      }
      try { server.assertDone(); } catch (error) { scenarioErrors.push(error); }
      try { await server.close(); } catch (error) { scenarioErrors.push(error); }
      if (requestFailures.length) {
        try { await writeFile(join(fixture, `${scenario.name}-request-failures.json`), JSON.stringify(requestFailures, null, 2)); }
        catch (error) { scenarioErrors.push(error); }
      }
      proof.push({ scenario: scenario.name, outcome: scenarioErrors.length ? "FAIL" : "PASS", model_requests: server.requests.length,
        batch_id: batchId, run_id: runId, ...collected?.summary,
        ...(requestFailures.length ? { request_failure_evidence: `${scenario.name}-request-failures.json` } : {}) });
    }
    if (scenarioErrors.length) errors.push(new AggregateError(scenarioErrors, `Scenario ${scenario.name} failed`));
  }
} catch (error) { errors.push(error); }
finally {
  if (await Bun.file(join(root, "fabric.db")).exists()) {
    // Manager owns native children/workspaces. Never kill a stored PID or rm a managed workspace.
    for (const batch of batches) {
      try {
        await cli(["cancel", batch, "--json"]);
        assert.equal((await cli(["wait", batch, "--timeout-seconds", "60", "--json"])).terminal, true, "Final cleanup left a nonterminal batch");
      } catch (error) { errors.push(error); }
    }
    try { await cli(["manager", "stop", "--json"]); } catch (error) { errors.push(error); }
  }
}
const result = { result: errors.length ? "FAIL" : "PASS", native_version: nativeVersion ?? null, evidence_root: fixture,
  platform: process.platform, arch: process.arch, model,
  prepared_state_id: prepared?.state_id, source_commit_sha: prepared?.source_commit_sha, scenarios: proof,
  isolation_limit: "Private environment/config and loopback endpoint; not OS egress confinement or a bypass of machine-managed Codex policy.",
  ...(errors.length ? { failed_phase: proof.find(scenario => scenario.outcome === "FAIL")?.scenario ?? phase, failure_evidence: "failure.txt" } : {}) };
if (errors.length) {
  const describe = error => `${error.stack ?? String(error)}${error.errors ? `\n${error.errors.map(describe).join("\n")}` : ""}`;
  await writeFile(join(fixture, "failure.txt"), errors.map(describe).join("\n\n"));
  process.exitCode = 1;
}
await writeFile(join(fixture, "proof.json"), JSON.stringify(result, null, 2));
console.log(JSON.stringify(result));
