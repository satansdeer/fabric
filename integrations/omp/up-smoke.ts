// Bun up-smoke.ts /absolute/native-bundle/bin/fabric /absolute/omp [reviewed-plugin-dir content-sha256]
// Provider-free: local slash handlers execute the SAME registered typed tools. Never sends a model prompt.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, realpath, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Database } from "bun:sqlite";

type Doc = Record<string, any>;
const binary = resolve(process.argv[2]);
const omp = resolve(process.argv[3]);
const fixture = await realpath(await mkdtemp(join(tmpdir(), "fabric-up-host-")));
const evidence = resolve(process.env.FABRIC_SMOKE_EVIDENCE || fixture);
await mkdir(evidence, { recursive: true });
const repo = join(fixture, "demo"), root = join(fixture, "storage"), bin = join(fixture, "bin");
const selectedAgent = process.env.FABRIC_SMOKE_NATIVE_AGENT_DIR;
assert.ok(selectedAgent, "Set FABRIC_SMOKE_NATIVE_AGENT_DIR to an explicitly selected configured native OMP agent directory. No credentials are copied and no model turn runs.");
const agent = await realpath(selectedAgent);
for (const dir of [repo, bin]) await mkdir(dir, { mode: 0o700 });
await symlink(omp, join(bin, "omp"));
const env = { PATH: `${bin}:/usr/bin:/bin`, HOME: fixture, PI_CODING_AGENT_DIR: agent, PI_NO_TITLE: "1", LC_ALL: "C",
 GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_AUTHOR_NAME: "Fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid", GIT_COMMITTER_NAME: "Fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid" };
let commandNumber = 0;
async function run(program: string, args: string[], expect = 0) {
 const prefix = join(evidence, `command-${++commandNumber}`), start = new Date().toISOString();
 const child = Bun.spawn([program, ...args], { cwd: repo, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
 const [stdout, stderr, exit] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
 await writeFile(`${prefix}.json`, JSON.stringify({ cwd: repo, start_utc: start, end_utc: new Date().toISOString(), program, args, exit }));
 await writeFile(`${prefix}.stdout`, stdout); await writeFile(`${prefix}.stderr`, stderr);
 assert.equal(exit, expect, `${program} ${args.join(" ")}\n${stderr}`);
 return stdout;
}
async function cli(args: string[]) { return JSON.parse(await run(binary, ["--root", root, ...args])); }
const clients: { close(): Promise<void> }[] = [];
const batches: string[] = [];
async function eventually(test: () => Promise<boolean>, reason: string) {
 const end = Date.now() + 30000;
 while (!await test()) { assert.ok(Date.now() < end, reason); await Bun.sleep(100); }
}
function host() {
 const start = new Date().toISOString();
 const child = Bun.spawn([binary, "--root", root, "controller", "--rpc"], { cwd: repo, env, stdin: "pipe", stdout: "pipe", stderr: "pipe" });
 const frames: Doc[] = []; let failure: Error | undefined, closed = false, sequence = 0;
 const stderr = new Response(child.stderr).text();
 const output = (async () => {
  let pending = ""; const decoder = new TextDecoder();
  for await (const chunk of child.stdout) {
   pending += decoder.decode(chunk, { stream: true });
   let n: number;
   while ((n = pending.indexOf("\n")) >= 0) {
    const line = pending.slice(0, n); pending = pending.slice(n + 1); if (!line.trim()) continue;
    const frame = JSON.parse(line); frames.push(frame);
    if (["agent_start", "extension_error", "rpc_frame_error"].includes(frame.type)) throw new Error(`Forbidden host event ${line}`);
   }
  }
  assert.equal(pending.trim(), "");
 })().catch(error => { failure = error; child.kill(); });
 async function request(type: string, fields: Doc = {}) {
  const id = `up-${++sequence}`;
  child.stdin.write(JSON.stringify({ id, type, ...fields }) + "\n");
  await eventually(async () => { if (failure) throw failure; return frames.some(f => f.id === id && f.type === "response"); }, `No RPC response: ${type}`);
  const response = frames.find(f => f.id === id && f.type === "response")!;
  assert.equal(response.success, true, JSON.stringify(response));
  if (type === "prompt" && response.data?.agentInvoked !== false) {
   await eventually(async () => frames.some(f => f.id === id && f.type === "prompt_result"), "local command did not finish");
   const result = frames.find(f => f.id === id && f.type === "prompt_result")!;
   assert.equal(result.agentInvoked, false); assert.equal(result.status, "completed");
  }
  return response.data || {};
 }
 async function local(command: string) {
  const before = (await request("get_messages")).messages.length;
  await request("prompt", { message: `/fabric ${command}` });
  let answer: Doc | undefined;
  await eventually(async () => { answer = (await request("get_messages")).messages.slice(before).find((m: Doc) => m.customType === "fabric"); return !!answer; }, "missing local tool result");
  return answer!.details;
 }
 const value = { request, local, invoke: (name: string, params: Doc) => local(`${name} ${JSON.stringify(params)}`), async close() {
  if (closed) return; closed = true; child.stdin.end();
  const timer = setTimeout(() => child.kill(), 10000);
  let exit: number; try { exit = await child.exited; } finally { clearTimeout(timer); }
  await output; const errors = await stderr; const prefix = join(evidence, `host-${clients.indexOf(value) + 1}`);
  await writeFile(`${prefix}.json`, JSON.stringify({ cwd: repo, start_utc: start, end_utc: new Date().toISOString(), program: binary, args: ["--root", root, "controller", "--rpc"], exit }));
  await writeFile(`${prefix}.stdout`, frames.map(f => JSON.stringify(f)).join("\n") + "\n"); await writeFile(`${prefix}.stderr`, errors);
  assert.equal(exit, 0, errors); if (failure) throw failure;
 } };
 clients.push(value); return value;
}
try {
 await run("/usr/bin/git", ["init", "--quiet"]);
 await writeFile(join(repo, "tracked"), "original\n");
 await run("/usr/bin/git", ["add", "."]); await run("/usr/bin/git", ["commit", "--quiet", "-m", "fixture"]);
 const options = ["up", "--controller", "--source-only", "--yes", "--json"];
 if (process.argv[4]) options.push("--plugin-dir", resolve(process.argv[4]), "--plugin-sha256", process.argv[5]);
 // Noninteractive denial must not create storage, execute plugins or read authentication contents.
await run(binary, ["--root", root, "up", "--controller", "--source-only", "--json"], 1);
 await assert.rejects(stat(root), { code: "ENOENT" });
 const up = await cli(options), grant = await readFile(join(root, ".native-context-omp.json"));
 const again = await cli(options);
 assert.equal(up.prepared_state_id, again.prepared_state_id); assert.equal(up.preparation_key, again.preparation_key);
 assert.deepEqual(await readFile(join(root, ".native-context-omp.json")), grant);
 assert.ok(up.next_command.includes("controller")); assert.equal(up.project, "demo");
 const first = host();
 const commands = (await first.request("get_available_commands")).commands;
 assert.ok(commands.some((c: Doc) => c.name === "fabric")); assert.ok(commands.some((c: Doc) => c.name === "swarm"));
 if (process.argv[4]) assert.ok(commands.some((c: Doc) => c.name === "skill:swarm"), "pinned external skill was not discovered");
 const tools = (await first.local("tools")).tools;
 for (const name of ["fabric_spawn", "fabric_status", "fabric_results", "fabric_wait", "fabric_cancel"]) assert.ok(tools.some((t: Doc) => t.name === name && t.sourceInfo), `Actual host omitted ${name}`);
 // Empty /swarm is local help. The task-bearing route deliberately remains uninvoked.
 await first.request("prompt", { message: "/swarm" });
 const limits = { timeout_seconds: 60, copy_limit_bytes: 134217728, max_log_bytes: 1048576, max_artifact_bytes: 1048576 };
 const tasks = ["one", "two", "three", "four"].map((id, index) => ({ task_id: id, harness_id: "fake-a", source_ref: { kind: "prepared", state_id: up.prepared_state_id }, limits,
  prompt: JSON.stringify({ writes: [{ path: "tracked", content: `worker-${id}\n` }], ...(index === 2 ? { exit_code: 9 } : {}) }) }));
 const remote = await first.invoke("spawn", { project: "demo", tasks, placement: "remote" });
 assert.equal(remote.isError, true); assert.equal(remote.details.error.kind, "unsupported_placement");
 const start = Date.now(), accepted = await first.invoke("spawn", { project: "demo", tasks, max_concurrency: 4 });
 assert.equal(accepted.isError, false); assert.ok(Date.now() - start < 5000, "spawn did not return fast durable handles");
 const batch = accepted.details.batch_id; batches.push(batch); assert.equal(new Set(accepted.details.tasks.map((t: Doc) => t.run_id)).size, 4);
 assert.equal((await first.invoke("status", { batch_id: batch })).isError, false);
 await first.close();
 const second = host(); let result: Doc;
 await eventually(async () => { result = await second.invoke("wait", { batch_id: batch, timeout_seconds: 10 }); return result.details.terminal; }, "four fakes never completed");
 result = (await second.invoke("results", { batch_id: batch })).details;
 assert.deepEqual(result.tasks.map((t: Doc) => t.state), ["SUCCEEDED", "SUCCEEDED", "FAILED", "SUCCEEDED"]);
 const db = new Database(join(root, "fabric.db"));
 try {
  for (const task of result.tasks) {
   assert.equal(task.source.prepared_state_id, up.prepared_state_id); assert.equal(task.source.base_commit_sha, up.source_commit_sha);
   assert.equal(task.result.capture.truncated, false);
   assert.equal(task.result.capture.limit_bytes, 1048576);
   assert.equal(db.query("SELECT state FROM workspaces WHERE workspace_id=?").get(task.result.workspace_id)?.state, "RECLAIMED");
   await assert.rejects(stat(task.result.workspace_path), { code: "ENOENT" });
   assert.ok(task.artifacts.length);
   for (const artifact of task.artifacts) {
    const bytes = await readFile(join(root, artifact.relative_path));
    assert.equal(bytes.length, artifact.size_bytes); assert.equal(createHash("sha256").update(bytes).digest("hex"), artifact.sha256);
   }
  }
 } finally { db.close(); }
 assert.equal(await readFile(join(repo, "tracked"), "utf8"), "original\n");
 const cancellation = await second.invoke("spawn", { project: "demo", tasks: [{ ...tasks[0], prompt: JSON.stringify({ process_tree: { ignore_child_term: false } }) }] });
 assert.equal(cancellation.isError, false);
 const cancelBatch = cancellation.details.batch_id; batches.push(cancelBatch);
 await eventually(async () => (await second.invoke("status", { batch_id: cancelBatch })).details.tasks[0].state === "RUNNING", "cancellation worker did not start");
 await second.invoke("cancel", { batch_id: cancelBatch });
 await eventually(async () => (await second.invoke("wait", { batch_id: cancelBatch, timeout_seconds: 10 })).details.terminal, "cancel never became terminal");
 const cancelled = (await second.invoke("results", { batch_id: cancelBatch })).details.tasks[0];
 assert.equal(cancelled.state, "CANCELLED");
 await assert.rejects(stat(cancelled.result.workspace_path), { code: "ENOENT" });
 const cancelDb = new Database(join(root, "fabric.db"));
 try { assert.equal(cancelDb.query("SELECT state FROM workspaces WHERE workspace_id=?").get(cancelled.result.workspace_id)?.state, "RECLAIMED"); }
 finally { cancelDb.close(); }
 await second.close();
 console.log(JSON.stringify({ result: "PASS", fixture, evidence, source_sha: up.source_commit_sha, ready: up.prepared_state_id, batch, registered_tools: tools.map((t: Doc) => t.name), inference: "NOT_RUN", native_defaults: "untouched" }));
} finally {
 for (const client of clients) {
  try { await client.close(); } catch (error) { console.error("Owned host cleanup:", error); }
 }
 if (await Bun.file(join(root, "fabric.db")).exists()) {
  for (const batch of batches) await cli(["cancel", batch, "--json"]);
  await cli(["manager", "stop", "--json"]);
 }
 console.error(`Owned fixture retained: ${fixture}`);
}
