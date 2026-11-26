// Bun only. Real Fabric + native OMP; the only model endpoint is the shared loopback fixture.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { access, mkdir, mkdtemp, readFile, realpath, stat, symlink, writeFile } from "node:fs/promises";
import { isAbsolute, join, resolve, sep } from "node:path";
import { Database } from "bun:sqlite";
import { startScriptedProvider } from "../../scripts/scripted-provider.mjs";
import { reference, install } from "../../sdk/typescript/cli.mjs";
import { directory } from "../../sdk/typescript/package.mjs";

assert.equal(process.argv.length, 5, "Usage: bun integrations/omp/scripted-provider-smoke.mjs /absolute/fabric /absolute/omp /absolute/reference-plugin");
assert.equal(process.platform, "darwin", "Socket-close evidence currently requires macOS /usr/sbin/lsof");
const [fabric, omp, referencePlugin] = await Promise.all(process.argv.slice(2).map(async path => {
  assert.ok(isAbsolute(path), "Binary arguments must be absolute paths");
  await access(path, constants.X_OK);
  assert.ok((await stat(path)).isFile(), "Binary argument must name an executable file");
  return realpath(path);
}));
await access("/usr/sbin/lsof", constants.X_OK);
process.umask(0o077);
// Do not use an inherited TMPDIR (or native HOME/configuration) to place the fixture.
const fixture = await realpath(await mkdtemp("/private/tmp/fabric-scripted-omp-"));
const repo = join(fixture, "source"), root = join(fixture, "storage"), bin = join(fixture, "bin");
const home = join(fixture, "home"), agent = join(home, ".omp", "agent");
for (const directory of [repo, bin, agent, "xdg-config", "xdg-data", "xdg-cache", "xdg-state", "codex", "tmp"].map(
  path => isAbsolute(path) ? path : join(fixture, path))) await mkdir(directory, { recursive: true, mode: 0o700 });
await symlink(omp, join(bin, "omp"));
await symlink(process.execPath, join(bin, "bun"));
const env = {
  PATH: `${bin}:/usr/bin:/bin`, HOME: home, PI_CODING_AGENT_DIR: agent, CODEX_HOME: join(fixture, "codex"),
  XDG_CONFIG_HOME: join(fixture, "xdg-config"), XDG_DATA_HOME: join(fixture, "xdg-data"),
  XDG_CACHE_HOME: join(fixture, "xdg-cache"), XDG_STATE_HOME: join(fixture, "xdg-state"), TMPDIR: join(fixture, "tmp"),
  LC_ALL: "C", PI_NO_TITLE: "1", OTEL_SDK_DISABLED: "true",
  GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0",
  GIT_AUTHOR_NAME: "Fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid",
  GIT_COMMITTER_NAME: "Fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
};
const model = "synthetic-model", providerName = "fabric-scripted", key = "fabric-fixture-only-not-a-secret";
const original = "original\n", changed = "scripted native write\n";
const sentinel = "FABRIC_SCRIPTED_COMPLETE", held = "FABRIC_SCRIPTED_HELD";
// Exact v19.1.1 catalog IDs plus implicit local discovery/context sources. No wildcard semantics.
// Source pin and the upgrade ceiling are documented in docs/LIVE_SMOKES.md.
const disabledProviders = `abliteration aiand aimlapi alibaba-coding-plan alibaba-token-plan amazon-bedrock
anthropic antigravity-cli apple azure baseten bedrock-mantle bifrost cerebras charm-hyper chutes cline-pass
cloudflare-ai-gateway cloudflare-workers-ai commandcode coralbricks coreweave cursor deepinfra deepseek devin
factory-droid firepass fireworks github-copilot gitlab-duo gitlab-duo-agent gmi-cloud google google-antigravity
google-vertex groq helmcode huggingface inception kilo kimi-code kiro liquid litellm lm-studio local meta
minimax minimax-code minimax-code-cn mistral modal modular moonshot muse-code nace nanogpt novita nvidia
ollama ollama-cloud openai openai-codex opencode-go opencode-zen openrouter otari qianfan qwen-portal sakana
sambanova scaleway siliconflow siliconflow-cn singularityapi-dev singularityapi-tech snowflake stepfun synthetic
together typesafe umans venice vercel-ai-gateway vllm wafer-serverless web xai xai-oauth xiaomi
xiaomi-token-plan-ams xiaomi-token-plan-cn xiaomi-token-plan-sgp yolo-auto zai zenmux zhipu-coding-plan
llama.cpp native claude codex gemini github opencode agents-md`.split(/\s+/);
const settings = {
  modelRoles: { default: `${providerName}/${model}` }, enabledModels: [`${providerName}/${model}`], disabledProviders,
  models: { offline: true, noJudge: true, noSpotlight: true },
  retry: { enabled: false, modelFallback: false, fallbackChains: { default: [] } },
  startup: { checkUpdate: false }, update: { auto: false }, marketplace: { autoUpdate: "off" },
  telemetry: { otlpExportEnabled: false }, lsp: { enabled: false },
  tools: { commands: false, approvalMode: "write" },
};
async function configure(directory, baseUrl) {
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const db = new Database(join(directory, "agent.db"));
  try { db.exec("PRAGMA user_version=0"); } finally { db.close(); }
  // JSON is YAML; no serializer or user-provider override is needed.
  await writeFile(join(directory, "config.yml"), JSON.stringify(settings));
  await writeFile(join(directory, "models.yml"), JSON.stringify({ providers: { [providerName]: {
    baseUrl: `${baseUrl}/v1`, api: "openai-completions", apiKey: key, auth: "apiKey", authHeader: true,
    models: [{ id: model, name: "Synthetic fixture", reasoning: false, input: ["text"], supportsTools: true,
      contextWindow: 131072, maxTokens: 1024, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } }],
  } } }));
}
let commandNumber = 0, phase = "setup", prepared, nativeVersion;
const proof = [], batches = [], errors = [];
const sha256 = bytes => createHash("sha256").update(bytes).digest("hex");
async function run(program, args, cwd = repo, seconds = 120) {
  const prefix = join(fixture, `command-${++commandNumber}`);
  const child = Bun.spawn([program, ...args], { cwd, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  let expired = false;
  const watchdog = setTimeout(() => { expired = true; child.kill("SIGKILL"); }, seconds * 1000);
  try {
    const [stdout, stderr, code] = await Promise.all([
      new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited,
    ]);
    await writeFile(`${prefix}.json`, JSON.stringify({ program, args, cwd, code, watchdog_expired: expired }));
    await writeFile(`${prefix}.stdout`, stdout); await writeFile(`${prefix}.stderr`, stderr);
    assert.ok(!expired, `Command watchdog expired; private evidence: ${prefix}`);
    assert.equal(code, 0, `Command failed; private evidence: ${prefix}`);
    return stdout;
  } finally { clearTimeout(watchdog); }
}
const cli = async args => JSON.parse(await run(fabric, ["--root", root, ...args]));
function checkRequest(prompt, continuation = false) {
  return ({ method, path, headers, body }) => {
    assert.ok(method === "POST" && path === "/v1/chat/completions", "Unexpected model route");
    assert.ok(headers.authorization === `Bearer ${key}`, "Only fixture Authorization is allowed");
    for (const name of ["x-api-key", "api-key", "cookie", "proxy-authorization"]) assert.ok(!headers[name], "Unexpected credential header");
    assert.ok(body.model === model && body.stream === true, "Wrong model or non-streaming request");
    assert.ok(Array.isArray(body.messages), "Missing conversation");
    assert.ok(body.messages.some(message => message.role === "user" && JSON.stringify(message.content).includes(prompt)), "Missing fixture prompt");
    if (continuation) {
      const calls = body.messages.filter(message => message.role === "assistant").flatMap(message => message.tool_calls ?? []);
      const call = calls.find(call => call.id === "fixture_write");
      assert.ok(calls.length === 1 && call?.function?.name === "write", "Native conversation lost or added a tool call");
      const args = JSON.parse(call.function.arguments);
      assert.ok(args.path === "tracked.txt" && args.content === changed, "Write arguments changed");
      const result = body.messages.find(message => message.role === "tool" && message.tool_call_id === call.id);
      assert.ok(body.messages.filter(message => message.role === "tool").length === 1, "Unexpected extra tool result");
      assert.ok(result && JSON.stringify(result.content).includes(`Successfully wrote ${Buffer.byteLength(changed)} bytes to tracked.txt`), "Missing actual successful native write result");
    } else {
      assert.ok(!body.messages.some(message => message.role === "tool"), "Unexpected earlier tool result");
    }
  };
}
function frame(delta, finish_reason = null) {
  return `data: ${JSON.stringify({ id: "fixture-completion", object: "chat.completion.chunk", created: 1, model,
    choices: [{ index: 0, delta, finish_reason }] })}\n\n`;
}
const sseHeaders = { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "close" };
function textChunks(text) {
  const wire = frame({ role: "assistant", content: "" }) + frame({ content: text.slice(0, 9) }) +
    frame({ content: text.slice(9) }) + frame({}, "stop") + "data: [DONE]\n\n";
  return [wire.slice(0, 7), wire.slice(7, 43), wire.slice(43, -5), wire.slice(-5)];
}
const jsonl = text => text.split("\n").filter(line => line.trim()).map(line => JSON.parse(line));
async function sockets(baseUrl) {
  const output = await run("/usr/sbin/lsof", ["-nP", "-a", "-p", String(process.pid), `-iTCP:${new URL(baseUrl).port}`, "-FfT"]);
  return output.split("\n").filter(line => line.startsWith("TST=")).map(line => line.slice(4));
}
async function observePartial(runId, text) {
  const child = Bun.spawn([fabric, "--root", root, "runs", "logs", runId, "--raw", "stdout", "--follow", "--json"],
    { cwd: repo, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  const stderr = new Response(child.stderr).text();
  const watchdog = setTimeout(() => child.kill("SIGKILL"), 90000);
  let outer = "", raw = "";
  try {
    const decoder = new TextDecoder();
    for await (const bytes of child.stdout) {
      outer += decoder.decode(bytes, { stream: true });
      let newline;
      while ((newline = outer.indexOf("\n")) >= 0) {
        const line = outer.slice(0, newline); outer = outer.slice(newline + 1);
        if (!line.trim()) continue;
        const observation = JSON.parse(line);
        raw += observation.raw ?? "";
        let end;
        while ((end = raw.indexOf("\n")) >= 0) {
          const nativeLine = raw.slice(0, end); raw = raw.slice(end + 1);
          if (!nativeLine.trim()) continue;
          const event = JSON.parse(nativeLine);
          if (event.type === "message_update" && event.assistantMessageEvent?.type === "text_delta" &&
              event.assistantMessageEvent.delta === text) return;
        }
        assert.ok(!observation.terminal, "Native run terminated before the held streaming delta");
      }
    }
    throw new Error("Native stdout ended before the held streaming delta (or emergency watchdog fired)");
  } finally {
    clearTimeout(watchdog); child.kill();
    const [, diagnostic] = await Promise.all([child.exited, stderr]);
    await writeFile(join(fixture, `observer-${runId}.stderr`), diagnostic);
  }
}
async function collect(batchId, runId) {
  const result = await cli(["results", batchId, "--json"]);
  assert.ok(result.terminal === true && result.tasks.length === 1, "Batch is not terminal with exactly one task");
  const task = result.tasks[0], report = task.result;
  assert.ok(task.run_id === runId && report?.run_id === runId, "Run ownership mismatch");
  const show = await cli(["runs", "show", runId, "--json"]);
  assert.ok(show.run.batch_id === batchId && show.run.run_id === runId && show.run.harness === "omp", "Wrong batch/harness owner");
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
  const contents = new Map();
  for (const artifact of task.artifacts) {
    const path = resolve(root, artifact.relative_path);
    assert.ok(path.startsWith(`${root}${sep}`), "Artifact escaped managed storage");
    const bytes = await readFile(path);
    assert.ok(bytes.length === artifact.size_bytes && sha256(bytes) === artifact.sha256, "Artifact checksum mismatch");
    contents.set(artifact.kind, bytes);
  }
  const manifest = JSON.parse(contents.get("manifest").toString());
  assert.ok(manifest.run_id === runId && manifest.workspace_id === report.workspace_id && manifest.prepared_state_id === prepared.state_id && manifest.base_commit_sha === prepared.source_commit_sha, "Artifact manifest provenance mismatch");
  assert.equal(await readFile(join(repo, "tracked.txt"), "utf8"), original, "Source checkout changed");
  const db = new Database(join(root, "fabric.db"), { readonly: true });
  try { assert.equal(db.query("SELECT state FROM workspaces WHERE workspace_id=?").get(report.workspace_id)?.state, "RECLAIMED"); }
  finally { db.close(); }
  return { task, report, contents, summary: { batch_id: batchId, run_id: runId, state: task.state,
    workspace_id: report.workspace_id, workspace_reclaimed: true, child_reaped: true,
    artifacts: task.artifacts.map(({ kind, sha256, size_bytes }) => ({ kind, sha256, size_bytes })) } };
}
function checkOutcome(collected, scenario) {
  const { task, report, contents } = collected;
  assert.equal(task.state, scenario.state, `${scenario.name}: unexpected native terminal state`);
  const native = jsonl(contents.get("stdout").toString());
  const events = jsonl(contents.get("events").toString()).map(record => record.event).filter(Boolean);
  const terminal = native.filter(event => event.type === "agent_end" && event.isTerminal === true);
  const output = events.filter(event => event.type === "output").map(event => event.text).join("");
  if (scenario.state === "SUCCEEDED") {
    assert.ok(report.exit_code === 0 && !report.error_kind && !report.capture.native_failure && !task.error, "Native success contains a failure");
    assert.ok(terminal.length === 1, "Missing unique real terminal agent_end");
    const assistant = terminal[0].messages.filter(message => message.role === "assistant").at(-1);
    assert.ok(assistant?.stopReason === "stop", "Native terminal assistant did not stop successfully");
    assert.ok(output.includes(sentinel), "Completion sentinel missing from actual normalized native output");
    assert.ok(events.some(event => event.type === "completed" && event.success === true), "No successful native completion event");
    assert.ok(!events.some(event => event.type === "failure"), "Unexpected normalized failure");
  } else {
    assert.ok(!output.includes(sentinel), "Failed/cancelled stream fabricated a completion sentinel");
    assert.ok(!events.some(event => event.type === "completed" && event.success === true), "Failed/cancelled stream fabricated native success");
    if (scenario.state === "FAILED") assert.ok(report.capture.native_failure || report.error_kind || report.exit_code !== 0 ||
      events.some(event => event.type === "failure"), "Missing actual native/API failure evidence");
  }
}
const scenarios = [
  { name: "split-text", state: "SUCCEEDED", requests: 1, prompt: "Synthetic split-text fixture. Reply with the requested completion marker only.",
    steps: check => [{ method: "POST", path: "/v1/chat/completions", check, headers: sseHeaders, chunks: textChunks(sentinel) }] },
  { name: "native-write", state: "SUCCEEDED", requests: 2, prompt: "Synthetic write fixture. Write the supplied fixture content to tracked.txt using write, then report completion. No other tools or files.",
    steps: (check, next) => [{ method: "POST", path: "/v1/chat/completions", check: request => {
      check(request);
      assert.ok(request.body.tools?.some(tool => tool.function?.name === "write"), "Native write tool is not available");
    }, headers: sseHeaders, chunks: [frame({ role: "assistant", tool_calls: [{ index: 0, id: "fixture_write", type: "function",
      function: { name: "write", arguments: JSON.stringify({ path: "tracked.txt", content: changed }) } }] }), frame({}, "tool_calls"), "data: [DONE]\n\n"] },
    { method: "POST", path: "/v1/chat/completions", check: next, headers: sseHeaders, chunks: textChunks(sentinel) }] },
  { name: "api-error", state: "FAILED", requests: 1, prompt: "Synthetic non-retryable API failure fixture.",
    steps: check => [{ method: "POST", path: "/v1/chat/completions", check, status: 401, headers: { "content-type": "application/json" },
      chunks: [JSON.stringify({ error: { message: "Synthetic fixture denial", type: "invalid_request_error", code: "fixture_denied" } })] }] },
  { name: "incomplete-disconnect", state: "FAILED", requests: 1, prompt: "Synthetic incomplete streaming fixture.",
    steps: (check, _next, gate) => [{ method: "POST", path: "/v1/chat/completions", check, headers: sseHeaders,
      chunks: [frame({ role: "assistant", content: "incomplete fixture" }), "data: {\"choices\":["], wait: gate, disconnect: true }] },
  { name: "cancel-held-stream", state: "CANCELLED", requests: 1, prompt: "Synthetic held streaming cancellation fixture.",
    steps: check => [{ method: "POST", path: "/v1/chat/completions", check, headers: sseHeaders,
      chunks: [frame({ role: "assistant", content: "" }), frame({ content: held })], wait: new Promise(() => {}) }] },
];
try {
  // Even version/probe invocations see only a disposable dummy-provider configuration.
  await configure(agent, "http://127.0.0.1:1");
  nativeVersion = (await run(omp, ["--version"])).trim();
  assert.equal(nativeVersion, "omp/19.1.1", "Review settings/discovery isolation before accepting another OMP version");
  await run("/usr/bin/git", ["init", "--quiet"]);
  await writeFile(join(repo, "tracked.txt"), original);
  await run("/usr/bin/git", ["add", "tracked.txt"]);
  await run("/usr/bin/git", ["commit", "--quiet", "-m", "Disposable scripted provider fixture"]);
  env.FABRIC_PLUGIN_REGISTRY = join(fixture, "registry");
  await mkdir(env.FABRIC_PLUGIN_REGISTRY, { mode: 0o700 });
  const plugin = reference("omp", join(fixture, "reference-omp"), referencePlugin);
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
    const server = await startScriptedProvider(scenario.steps(checkRequest(scenario.prompt), checkRequest(scenario.prompt, true), disconnectGate));
    let batchId, runId, collected, settled = false;
    const scenarioErrors = [];
    try {
      assert.match(server.baseUrl, /^http:\/\/127\.0\.0\.1:[1-9][0-9]*$/);
      const scenarioHome = join(fixture, scenario.name, "home"), scenarioAgent = join(scenarioHome, ".omp", "agent");
      await configure(scenarioAgent, server.baseUrl);
      const request = join(fixture, `${scenario.name}.json`);
      await writeFile(request, JSON.stringify({ project: "scripted", placement: "local", max_concurrency: 1, tasks: [{
        task_id: scenario.name, harness_id: "omp", prompt: scenario.prompt,
        execution_context: { kind: "explicit", harness_version: nativeVersion,
          native_config_refs: [{ kind: "native-home", path: scenarioHome }, { kind: "native-agent-dir", path: scenarioAgent }],
          connection_refs: [{ kind: "native-auth", name: "native-home" }] },
        source_ref: { kind: "prepared", state_id: prepared.state_id },
        limits: { timeout_seconds: 120, copy_limit_bytes: 10485760, max_log_bytes: 1048576, max_artifact_bytes: 10485760 },
      }] }));
      const accepted = await cli(["workers", "spawn", "--request", request, "--json"]);
      // Record ownership before asserting admission details, so finally can always cancel an admitted batch.
      batchId = accepted.batch_id;
      if (batchId) batches.push(batchId);
      assert.ok(accepted.accepted === true && batchId && accepted.tasks.length === 1, "Worker not admitted");
      runId = accepted.tasks[0].run_id;
      assert.ok(runId, "Admission omitted run ID");
      if (scenario.name === "incomplete-disconnect") {
        await observePartial(runId, "incomplete fixture");
        releaseDisconnect();
      }
      if (scenario.state === "CANCELLED") {
        await observePartial(runId, held);
        assert.equal(server.requests.length, 1, "Cancellation happened before the actual model request");
        assert.ok((await sockets(server.baseUrl)).includes("ESTABLISHED"), "Held stream has no accepted server socket");
        await cli(["cancel", batchId, "--json"]);
      }
      assert.equal((await cli(["wait", batchId, "--timeout-seconds", "90", "--json"])).terminal, true, "Worker did not terminate");
      settled = true;
      collected = await collect(batchId, runId);
      checkOutcome(collected, scenario);
      assert.equal(server.requests.length, scenario.requests, "Unexpected actual model request count");
      if (scenario.name === "native-write") {
        const patch = collected.contents.get("tracked_patch");
        assert.ok(patch?.length && patch.toString().includes("tracked.txt"), "Missing captured write patch");
        assert.equal(collected.contents.get("untracked_paths")?.length, 0, "Native write created unexpected untracked files");
        const events = jsonl(collected.contents.get("events").toString()).map(record => record.event).filter(Boolean);
        const tools = events.filter(event => event.type === "tool");
        assert.ok(tools.length === 2 && tools[0].name === "write" && tools[0].phase === "start" &&
          tools[1].name === "write" && tools[1].phase === "end", "Expected exactly one actual native write");
        const recovery = join(fixture, "recovered-write");
        await run("/usr/bin/git", ["clone", "--quiet", "--no-local", "--no-hardlinks", repo, recovery]);
        await run("/usr/bin/git", ["checkout", "--quiet", "--detach", prepared.source_commit_sha], recovery);
        const patchPath = join(fixture, "verified-write.patch"); await writeFile(patchPath, patch);
        await run("/usr/bin/git", ["apply", "--binary", patchPath], recovery);
        assert.equal(await readFile(join(recovery, "tracked.txt"), "utf8"), changed, "Artifact restore lost native write");
        assert.equal((await run("/usr/bin/git", ["diff", "--name-only"], recovery)).trim(), "tracked.txt", "Native tool edited other tracked files");
        collected.summary.patch_restored = true;
      }
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
      proof.push({ scenario: scenario.name, outcome: scenarioErrors.length ? "FAIL" : "PASS", model_requests: server.requests.length,
        batch_id: batchId, run_id: runId, ...collected?.summary });
    }
    if (scenarioErrors.length) errors.push(new AggregateError(scenarioErrors, `Scenario ${scenario.name} failed`));
  }
} catch (error) { errors.push(error); }
finally {
  if (await Bun.file(join(root, "fabric.db")).exists()) {
    // Manager owns all native children/workspaces; never kill a stored PID or rm a managed workspace.
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
  prepared_state_id: prepared?.state_id, source_commit_sha: prepared?.source_commit_sha, scenarios: proof,
  ...(errors.length ? { failed_phase: proof.find(scenario => scenario.outcome === "FAIL")?.scenario ?? phase, failure_evidence: "failure.txt" } : {}) };
if (errors.length) {
  const describe = error => error instanceof AggregateError ? `${error.stack}\n${error.errors.map(describe).join("\n")}` : String(error?.stack ?? error);
  await writeFile(join(fixture, "failure.txt"), errors.map(describe).join("\n"));
  process.exitCode = 1;
}
await writeFile(join(fixture, "proof.json"), JSON.stringify(result, null, 2));
console.log(JSON.stringify(result));
