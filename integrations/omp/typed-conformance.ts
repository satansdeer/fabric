// Provider-free acceptance against the real generic workers CLI, with an in-process OMP host.
// Run: bun integrations/omp/typed-conformance.ts "$PWD/target/debug/fabric"
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { chmod, mkdtemp, mkdir, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { Database } from "bun:sqlite";
import type { ExtensionAPI, ExecOptions, ExecResult } from "@oh-my-pi/pi-coding-agent";
import fabricExtension from "./fabric";

type Document = Record<string, unknown>;
type ToolResult = { content: { type: string; text: string }[]; details: unknown; isError?: boolean };
type Invocation = Omit<ToolResult, "details"> & { details: Document };
type FixtureContext = {
	cwd: string;
	setTimeout(callback: () => unknown, delay: number): NodeJS.Timeout;
	setInterval(callback: () => unknown, delay: number): NodeJS.Timeout;
	clearTimer(timer: NodeJS.Timeout): void;
};
type Tool = { name: string; execute(id: string, params: Document, signal: AbortSignal, update: undefined, ctx: FixtureContext): Promise<ToolResult> };
type Controller = {
	tools: Map<string, Tool>;
	commands: Map<string, unknown>;
	messages: { payload: Document; options: Document }[];
	timers: Set<NodeJS.Timeout>;
	invoke(name: string, params: Document): Promise<Invocation>;
	event(name: string): Promise<void>;
	clear(): void;
};
function object(value: unknown): Document {
	assert.ok(value !== null && typeof value === "object" && !Array.isArray(value));
	return value as Document;
}
function array(value: unknown): unknown[] { assert.ok(Array.isArray(value)); return value; }
function records(value: unknown): Document[] { return array(value).map(object); }
function text(value: unknown): string { assert.equal(typeof value, "string"); return value as string; }
const binary = resolve(process.argv[2] || "target/debug/fabric");
const fixture = await mkdtemp(join(tmpdir(), "fabric-omp-typed-"));
const root = join(fixture, "storage ; literal");
const repo = join(fixture, "repo ; $(not-a-shell)");
const previous = { binary: process.env.FABRIC_BINARY, root: process.env.FABRIC_ROOT };
const environment = {
	PATH: "/usr/bin:/bin", HOME: fixture, LC_ALL: "C", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null",
	GIT_AUTHOR_NAME: "Fabric fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid",
	GIT_COMMITTER_NAME: "Fabric fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
};
const batches: string[] = [];
const requestPaths: string[] = [];
let canned: ExecResult | "throw" | undefined;
const calls: { args: string[]; signal?: AbortSignal; done: boolean }[] = [];

async function exec(command: string, args: string[], signal?: AbortSignal) {
	const child = Bun.spawn([command, ...args], { cwd: repo, env: environment, signal, stdout: "pipe", stderr: "pipe" });
	const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
	return { stdout, stderr, code, killed: false };
}
async function cli(args: string[]) {
	const output = await exec(binary, ["--root", root, ...args]);
	assert.equal(output.code, 0, `CLI failed: ${args.join(" ")} ${output.stderr}`);
	return object(JSON.parse(output.stdout));
}
function query(sql: string, ...params: string[]) {
	const database = new Database(join(root, "fabric.db"));
	try { return database.query<Document, string[]>(sql).all(...params); } finally { database.close(); }
}
function inventory() {
	return ["runs", "workspaces", "batches", "batch_tasks", "worker_attempts"].map(table => query(`SELECT COUNT(*) AS n FROM ${table}`)[0].n);
}
async function eventually(condition: () => boolean | Promise<boolean>, reason: string, timeout = 20000) {
	const deadline = Date.now() + timeout;
	while (!await condition()) {
		assert.ok(Date.now() < deadline, reason);
		await Bun.sleep(25);
	}
}
// Only schema construction is substituted here; native schemas are exercised by the installed-host proof.
type SchemaStub = { (...args: unknown[]): SchemaStub; [key: string]: SchemaStub };
// This proxy intentionally implements arbitrary fluent schema constructors, not validation.
const schema = new Proxy(function () { return schema; }, { get: () => schema, apply: () => schema }) as SchemaStub;
function controller(): Controller {
	const tools = new Map<string, Tool>();
	const commands = new Map<string, unknown>();
	const handlers = new Map<string, ((event: Document, ctx: FixtureContext) => unknown)[]>();
	const timers = new Set<NodeJS.Timeout>();
	const messages: { payload: Document; options: Document }[] = [];
	const host = {
		zod: schema,
		registerTool(tool: Tool) { assert.equal(tools.has(tool.name), false); tools.set(tool.name, tool); },
		registerCommand(name: string, command: unknown) { commands.set(name, command); },
		on(name: string, handler: (event: Document, ctx: FixtureContext) => unknown) { handlers.set(name, [...handlers.get(name) || [], handler]); },
		sendMessage(payload: Document, options: Document = {}) {
			assert.notEqual(options.deliverAs, "aside", "aside may start a background model turn");
			assert.notEqual(options.triggerTurn, true, "background progress must not invoke a provider");
			messages.push({ payload, options });
		},
		async exec(command: string, args: string[], options: ExecOptions) {
			assert.equal(command, binary);
			assert.equal(options.cwd, repo);
			assert.equal("timeout" in options, false, "Fabric owns worker deadlines");
			const call = { args: [...args], signal: options.signal, done: false };
			calls.push(call);
			try {
				const index = args.indexOf("--request");
				if (index >= 0) {
					const path = args[index + 1];
					requestPaths.push(path);
					assert.equal((await stat(path)).mode & 0o777, 0o600);
					assert.equal((await stat(dirname(path))).mode & 0o777, 0o700);
					const request = object(JSON.parse(await readFile(path, "utf8")));
					if (Array.isArray(request.tasks) && records(request.tasks).some(task => task.harness_id)) {
						assert.ok(records(request.tasks).every(task => !("model" in task)), "omitted model must stay omitted");
					}
				}
				if (canned === "throw") throw new Error("secret transport sentinel");
				if (canned) return canned;
				// Fixture-only adapter selection, injected outside production extension transport.
				const actual = [...args];
				if (args.includes("spawn")) actual.push("--fake-binary", join(fixture, "gated-fake"));
				return await exec(command, actual, options.signal);
			} finally { call.done = true; }
		},
	};
	const ctx: FixtureContext = {
		cwd: repo,
		setTimeout(callback, delay) {
			const timer = setTimeout(async () => { timers.delete(timer); await callback(); }, delay);
			timers.add(timer); return timer;
		},
		setInterval(callback, delay) {
			assert.ok(delay >= 250, "no hot interval polling");
			const timer = setInterval(() => callback(), delay); timers.add(timer); return timer;
		},
		clearTimer(timer) { clearTimeout(timer); clearInterval(timer); timers.delete(timer); },
	};
	fabricExtension(host as unknown as ExtensionAPI);
	return {
		tools, commands, messages, timers,
		async invoke(name: string, params: Document) {
			const tool = tools.get(name);
			assert.ok(tool, `missing typed OMP tool ${name}`);
			const result = await tool.execute("acceptance-call", params, new AbortController().signal, undefined, ctx);
			assert.deepEqual(JSON.parse(result.content[0].text), result.details, "visible and controller result disagree");
			return { ...result, details: object(result.details) };
		},
		async event(name: string) { for (const handler of handlers.get(name) || []) await handler({ reason: "switch", previousSessionFile: "old" }, ctx); },
		clear() { for (const timer of timers) { clearTimeout(timer); clearInterval(timer); } timers.clear(); },
	};
}
const controllers: Controller[] = [];
function connect() { const value = controller(); controllers.push(value); return value; }
async function cleanupRequests() {
	for (const path of requestPaths) {
		assert.equal(await Bun.file(path).exists(), false, "private request survived exchange");
		await assert.rejects(stat(dirname(path)), { code: "ENOENT" });
	}
}
async function writable(path: string) {
	if (!(await stat(path)).isDirectory()) return;
	await chmod(path, 0o700);
	for (const entry of await readdir(path)) await writable(join(path, entry));
}

try {
	process.env.FABRIC_BINARY = binary;
	process.env.FABRIC_ROOT = root;
	const first = connect();
	for (const name of ["fabric", "fabric_spawn", "fabric_status", "fabric_results", "fabric_cancel", "fabric_wait"]) {
		assert.ok(first.tools.has(name), `missing typed OMP tool ${name}`);
	}
	assert.ok(first.commands.has("fabric"), "legacy /fabric command must remain");
	await mkdir(repo);
	for (const args of [["init", "--quiet"], ["config", "core.hooksPath", "/dev/null"]]) assert.equal((await exec("/usr/bin/git", args)).code, 0);
	await writeFile(join(repo, "tracked"), "original\n");
	assert.equal((await exec("/usr/bin/git", ["add", "."])).code, 0);
	assert.equal((await exec("/usr/bin/git", ["commit", "--quiet", "-m", "fixture"])).code, 0);
	await cli(["project", "add", "--name", "demo", "--path", repo, "--json"]);
	const recipe = join(fixture, "prepare.toml");
	await writeFile(recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n");
	const prepared = await cli(["project", "prepare", "demo", "--recipe", recipe, "--json"]);
	const fake = join(dirname(binary), "fabric-fake-harness");
	await writeFile(join(fixture, "gated-fake"), `#!/bin/sh
set -eu
for arg in "$@"; do if [ "$arg" = --version ]; then exec '${fake}' "$@"; fi; done
case "$1" in --protocol-a|--protocol-b) prompt=$(cat "$3");; --prompt-file) prompt=$(cat "$2");; *) exit 89;; esac
case "$prompt" in *'worker-one'*) id=one;; *'worker-two'*) id=two;; *'worker-three'*) id=three;; *'worker-four'*) id=four;; *) exit 90;; esac
root='${fixture}'
touch "$root/started.$id"
trap 'touch "$root/stopped.$id"' EXIT
trap 'exit 143' TERM INT
i=0
while [ ! -f "$root/release.$id" ] && [ ! -f "$root/release-all" ]; do
 i=$((i+1)); test "$i" -lt 1500; sleep 0.02
done
'${fake}' "$@"
`, { mode: 0o700 });
	const ids = ["one", "two", "three", "four"];
	const tasks = ids.map((id, index) => ({
		task_id: id, harness_id: index % 2 ? "fake-b" : "fake-a",
		prompt: JSON.stringify({ required_files: [".deps/ready"], writes: [{ path: "tracked", content: `worker-${id} ; $(touch INJECTED) 'quoted'\n` }], ...(id === "three" ? { exit_code: 9 } : {}) }),
		source_ref: { kind: "prepared", state_id: prepared.state_id },
		limits: { timeout_seconds: 45, copy_limit_bytes: 10485760 },
	}));
	const beforeRemote = inventory();
	const remote = await first.invoke("fabric_spawn", { project: "demo", tasks, placement: "remote" });
	assert.equal(remote.isError, true);
	assert.equal(object(remote.details.error).kind, "unsupported_placement");
	assert.deepEqual(inventory(), beforeRemote, "remote request silently allocated local work");
	const started = Date.now();
	const spawned = await first.invoke("fabric_spawn", { project: "demo", tasks, max_concurrency: 4, idempotency_key: "omp-four-workers" });
	assert.equal(spawned.isError, false);
	assert.equal(spawned.details.accepted, true);
	assert.ok(Date.now() - started < 5000, "spawn waited for workers instead of returning durable handles");
	const batch = text(spawned.details.batch_id);
	batches.push(batch);
	assert.deepEqual(records(spawned.details.tasks).map(task => task.task_id), ids);
	assert.equal(new Set(records(spawned.details.tasks).map(task => text(task.run_id))).size, 4);
	for (const id of ids) await eventually(() => Bun.file(join(fixture, `started.${id}`)).exists(), `worker ${id} never started`);
	const status = await first.invoke("fabric_status", { batch_id: batch });
	assert.equal(status.details.terminal, false);
	assert.equal(array(status.details.tasks).length, 4);
	assert.ok(array(status.details.events).length > 0, "status omitted durable progress");
	const waitStarted = Date.now();
	const waiting = await first.invoke("fabric_wait", { batch_id: batch, timeout_seconds: 1 });
	assert.equal(waiting.details.terminal, false);
	assert.ok(Date.now() - waitStarted >= 750, "wait hot-polled instead of bounded waiting");
	assert.ok(Date.now() - waitStarted < 4000, "wait ignored caller bound");
	const tooLong = await first.invoke("fabric_wait", { batch_id: batch, timeout_seconds: 11 });
	assert.equal(tooLong.isError, true, "tool must bound each wait to at most ten seconds");
	await writeFile(join(fixture, "release.one"), "");
	await writeFile(join(fixture, "release.three"), "");
	await eventually(() => first.messages.some(({ payload }) => {
		if (payload.excludeFromContext === true || payload.display !== true) return false;
		if (!payload.details) return false;
		const details = object(payload.details);
		return details.batch_id === batch && records(details.tasks).some(task => task.task_id === "one" && task.state === "SUCCEEDED")
			&& records(details.tasks).some(task => task.task_id === "two" && task.terminal === false);
	}), "partial progress never reached ordinary controller context while siblings were running");
	await first.event("session_switch");
	await eventually(() => calls.every(call => call.done), "session switch left a wait client running");
	assert.equal(first.timers.size, 0, "session switch retained progress watchers");
	const afterSwitch = first.messages.length;
	const second = connect();
	const partial = await second.invoke("fabric_results", { batch_id: batch });
	assert.equal(partial.details.terminal, false);
	assert.equal(records(partial.details.tasks).find(task => task.task_id === "one")?.state, "SUCCEEDED");
	await writeFile(join(fixture, "release-all"), "");
	for (let attempt = 0; attempt < 5; attempt++) {
		const result = await second.invoke("fabric_wait", { batch_id: batch, timeout_seconds: 10 });
		if (result.details.terminal) break;
		assert.ok(attempt < 4, "workers never settled after reconnect");
	}
	const results = await second.invoke("fabric_results", { batch_id: batch });
	assert.equal(results.details.terminal, true);
	assert.equal(results.details.succeeded, false, "real exit failure was hidden");
	assert.deepEqual(records(results.details.tasks).map(task => task.state), ["SUCCEEDED", "SUCCEEDED", "FAILED", "SUCCEEDED"]);
	const workspaces = new Set<string>();
	for (const task of records(results.details.tasks)) {
		const source = object(task.source);
		assert.equal(source.prepared_state_id, prepared.state_id);
		assert.equal(source.base_commit_sha, prepared.source_commit_sha);
		const artifacts = records(task.artifacts);
		assert.ok(artifacts.length, "durable artifacts missing");
		for (const file of artifacts) {
			const bytes = await readFile(join(root, text(file.relative_path)));
			assert.equal(bytes.length, file.size_bytes);
			assert.equal(createHash("sha256").update(bytes).digest("hex"), file.sha256);
		}
		assert.ok(task.result, "settled worker result missing");
		const run = object(task.result);
		const workspaceId = text(run.workspace_id);
		workspaces.add(workspaceId);
		assert.equal(records(run.worker_attempts)[0].model_requested, null);
		assert.equal(query("SELECT state FROM workspaces WHERE workspace_id=?", workspaceId)[0].state, "RECLAIMED");
		await assert.rejects(stat(text(run.workspace_path)), { code: "ENOENT" });
		const patch = artifacts.find(file => text(file.relative_path).endsWith("tracked.patch"));
		assert.ok(patch, "tracked patch missing");
		assert.ok((await readFile(join(root, text(patch.relative_path)), "utf8")).includes(`+worker-${task.task_id}`));
	}
	assert.equal(workspaces.size, 4, "workers did not have independent workspaces");
	assert.equal(await readFile(join(repo, "tracked"), "utf8"), "original\n");
	assert.equal(await readFile(join(text(prepared.prepared_path), "tracked"), "utf8"), "original\n");
	assert.equal(await Bun.file(join(repo, "INJECTED")).exists(), false);
	assert.equal(first.messages.length, afterSwitch, "old controller received messages after switching sessions");
	await rm(join(fixture, "release-all"));
	await rm(join(fixture, "release.one"));
	const cancelSpawn = await second.invoke("fabric_spawn", { project: "demo", tasks: [tasks[0]] });
	const cancelBatch = text(cancelSpawn.details.batch_id); batches.push(cancelBatch);
	const cancelled = await second.invoke("fabric_cancel", { batch_id: cancelBatch });
	assert.equal(cancelled.details.acknowledged, true);
	for (let attempt = 0; attempt < 3; attempt++) {
		if ((await second.invoke("fabric_wait", { batch_id: cancelBatch, timeout_seconds: 10 })).details.terminal) break;
	}
	assert.equal(records((await second.invoke("fabric_results", { batch_id: cancelBatch })).details.tasks)[0].state, "CANCELLED");
	await second.event("session_shutdown");
	await eventually(() => calls.every(call => call.done), "shutdown left a wait client running");
	assert.equal(second.timers.size, 0);
	for (const output of [
		{ stdout: "not JSON", stderr: "secret stderr sentinel", code: 1, killed: false },
		{ stdout: "{}", stderr: "secret stderr sentinel", code: 0, killed: false },
		{ stdout: JSON.stringify(status.details), stderr: "secret stderr sentinel", code: -1, killed: true },
		"throw" as const,
	]) {
		canned = output;
		const result = await second.invoke("fabric_status", { batch_id: batch });
		assert.equal(result.isError, true);
		assert.ok(text(object(result.details.error).kind).startsWith("fabric_"));
		assert.ok(!JSON.stringify(result).includes("secret"), "raw transport diagnostics leaked");
	}
	canned = undefined;
	await cleanupRequests();
	const waitCalls = calls.filter(call => call.args.includes("wait"));
	assert.ok(waitCalls.length > 0, "progress did not use bounded CLI wait");
	for (const call of waitCalls) {
		const timeout = Number(call.args[call.args.indexOf("--timeout-seconds") + 1]);
		assert.ok(timeout >= 0 && timeout <= 10, "unbounded CLI wait");
	}
	console.log("PASS: typed OMP tools, early durable four-worker handles, ordinary context progress, bounded wait, independent failure/artifacts/reclaim, reconnect/cancel, remote rejection and safe transport (in-process host; no provider calls)");
} finally {
	canned = undefined;
	for (const controller of controllers) { await controller.event("session_shutdown"); controller.clear(); }
	if (previous.binary === undefined) delete process.env.FABRIC_BINARY; else process.env.FABRIC_BINARY = previous.binary;
	if (previous.root === undefined) delete process.env.FABRIC_ROOT; else process.env.FABRIC_ROOT = previous.root;
	if (await Bun.file(join(root, "fabric.db")).exists()) {
		await writeFile(join(fixture, "release-all"), "");
		for (const batch of batches) await cli(["cancel", batch, "--json"]);
		await cli(["manager", "stop", "--json"]);
	}
	await writable(fixture);
	await rm(fixture, { recursive: true, force: true });
}
