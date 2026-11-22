// Run with Bun; no installed npm packages and no provider/model calls.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { chmod, mkdtemp, mkdir, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { Database } from "bun:sqlite";
import fabricExtension from "./fabric";
import type { ExtensionAPI, ExecOptions, ExecResult } from "@oh-my-pi/pi-coding-agent";

type ObjectValue = Record<string, unknown>;
type ToolResult = { content: { type: string; text: string }[]; details: unknown; isError: boolean };
type FixtureTool = { name: string; execute(id: string, params: { request: unknown }, signal: AbortSignal, update: undefined, ctx: { cwd: string }): Promise<ToolResult> };
type FixtureCommand = { handler(args: string, ctx: { cwd: string }): Promise<void> };
type FixtureMessage = { display: boolean; content: string };
function object(value: unknown): ObjectValue {
	assert.ok(value !== null && typeof value === "object" && !Array.isArray(value));
	return value as ObjectValue;
}
function text(value: unknown): string { assert.equal(typeof value, "string"); return value as string; }
function array(value: unknown): unknown[] { assert.ok(Array.isArray(value)); return value; }
function response(value: unknown) {
	const document = object(value);
	assert.deepEqual(Object.keys(document).sort(), ["batch", "op", "version"]);
	assert.equal(document.version, 1);
	assert.equal(document.op, "spawn_many");
	const batch = object(document.batch);
	assert.equal(typeof batch.succeeded, "boolean");
	return {
		version: document.version, op: document.op,
		batch: {
			batch_id: text(batch.batch_id), succeeded: batch.succeeded,
			tasks: array(batch.tasks).map(value => {
				const task = object(value);
				const run = object(task.result);
				const attempts = array(run.worker_attempts).map(value => {
					const attempt = object(value);
					assert.equal(typeof attempt.event_cursor, "number");
					return { harness_requested: text(attempt.harness_requested), harness_resolved: text(attempt.harness_resolved), backend_id: text(attempt.backend_id), event_cursor: Number(attempt.event_cursor) };
				});
				return {
					id: text(task.id), run_id: text(task.run_id), error: task.error,
					result: {
						run_id: text(run.run_id), harness: text(run.harness), state: text(run.state),
						workspace_id: text(run.workspace_id), workspace_path: text(run.workspace_path),
						prepared_state_id: text(run.prepared_state_id), artifact_manifest_id: text(run.artifact_manifest_id),
						harness_version: text(run.harness_version), profile: run.profile, worker_attempts: attempts,
					},
				};
			}),
		},
	};
}
function errorKind(value: unknown) { return text(object(object(value).error).kind); }

const binary = resolve(process.argv[2] || "target/debug/fabric");
const fixture = await mkdtemp(join(tmpdir(), "fabric-omp-conformance-"));
const root = join(fixture, "storage ; literal");
const repo = join(fixture, "repo ; $(not-a-shell)");
const previous = { binary: process.env.FABRIC_BINARY, root: process.env.FABRIC_ROOT };
const environment = {
	PATH: "/usr/bin:/bin", HOME: fixture, LC_ALL: "C", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null",
	GIT_AUTHOR_NAME: "Fabric fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid",
	GIT_COMMITTER_NAME: "Fabric fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
};

async function exec(command: string, args: string[], cwd = repo, signal?: AbortSignal) {
	const child = Bun.spawn([command, ...args], { cwd, env: environment, signal, stdout: "pipe", stderr: "pipe" });
	const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
	return { stdout, stderr, code, killed: false };
}
async function cli(args: string[]): Promise<ObjectValue> {
	const result = await exec(binary, ["--root", root, ...args]);
	assert.equal(result.code, 0, `Fabric setup/inspection failed: ${args[0]}`);
	return object(JSON.parse(result.stdout));
}
function query(sql: string, ...params: string[]): ObjectValue[] {
	const database = new Database(join(root, "fabric.db"));
	try { return database.query<ObjectValue, string[]>(sql).all(...params); } finally { database.close(); }
}
function inventory() {
	return ["runs", "workspaces", "batches", "batch_tasks", "worker_attempts"].map(table => query(`SELECT COUNT(*) AS n FROM ${table}`)[0].n);
}
async function writableDirectories(path: string) {
	if (!(await stat(path)).isDirectory()) return;
	await chmod(path, 0o700);
	for (const name of await readdir(path)) await writableDirectories(join(path, name));
}

try {
	await mkdir(repo);
	for (const args of [["init", "--quiet"], ["config", "core.hooksPath", "/dev/null"]]) {
		assert.equal((await exec("/usr/bin/git", args)).code, 0);
	}
	await writeFile(join(repo, "tracked"), "original");
	assert.equal((await exec("/usr/bin/git", ["add", "."])).code, 0);
	assert.equal((await exec("/usr/bin/git", ["commit", "--quiet", "-m", "fixture"])).code, 0);
	await cli(["project", "add", "--name", "demo", "--path", repo, "--json"]);
	const recipe = join(fixture, "prepare.toml");
	await writeFile(recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n");
	const prepared = await cli(["project", "prepare", "demo", "--recipe", recipe, "--json"]);
	process.env.FABRIC_BINARY = binary;
	process.env.FABRIC_ROOT = root;

	let tool: FixtureTool | undefined;
	let command: FixtureCommand | undefined;
	const messages: FixtureMessage[] = [];
	const requests: string[] = [];
	const controller = new AbortController();
	let canned: ExecResult | "throw" | undefined;
	let lastArgs: string[] = [];
	let lastBinary = "";
	let expectedSignal: AbortSignal | undefined = controller.signal;
	const schema = { passthrough() { return this; } };
	const host = {
		zod: { object: () => schema },
		registerTool(value: FixtureTool) { assert.equal(value.name, "fabric"); tool = value; },
		registerCommand(name: string, value: FixtureCommand) { assert.equal(name, "fabric"); command = value; },
		sendMessage(message: FixtureMessage, options: { triggerTurn: boolean }) {
			assert.equal(message.display, true);
			assert.deepEqual(options, { triggerTurn: false });
			messages.push(message);
		},
		async exec(command: string, args: string[], options: ExecOptions) {
			assert.equal(options.cwd, repo);
			assert.equal(options.signal, expectedSignal);
			assert.equal("timeout" in options, false);
			lastArgs = args;
			lastBinary = command;
			const path = args[args.indexOf("--request") + 1];
			assert.ok(path);
			requests.push(path);
			assert.equal((await stat(path)).mode & 0o777, 0o600);
			assert.equal((await stat(dirname(path))).mode & 0o777, 0o700);
			assert.ok(JSON.parse(await readFile(path, "utf8")));
			if (canned === "throw") throw new Error("secret exception must not escape");
			return canned ?? await exec(command, args, options.cwd, options.signal);
		},
	};
	// This deliberately partial in-process host only implements the extension's consumed APIs.
	const api = host as unknown as ExtensionAPI;
	fabricExtension(api);
	assert.ok(tool && command);
	const registeredTool = tool;
	const ctx = { cwd: repo };
	const invoke = (request: unknown) => registeredTool.execute("fixture-call", { request }, controller.signal, undefined, ctx);
	const cleanup = async () => {
		for (const path of requests) {
			assert.equal(await Bun.file(path).exists(), false, "private request survived");
			await assert.rejects(stat(dirname(path)), { code: "ENOENT" });
		}
	};
	const request = {
		version: 1, op: "spawn_many", project: "demo", concurrency: 2,
		tasks: [
			{ id: "json-worker", harness: "fake-a", prompt: JSON.stringify({ required_files: [".deps/ready"], writes: [{ path: "tracked", content: "a" }] }), request: { model: "literal-model", profile: { harness_id: "fake-a", harness_version: "fabric-fake-a 1" } } },
			{ id: "text-worker", harness: "fake-b", prompt: JSON.stringify({ required_files: [".deps/ready"], writes: [{ path: "tracked", content: "b" }] }), request: { profile: { harness_id: "fake-b", harness_version: "fabric-fake-b 1" } } },
		],
		options: { timeout_seconds: 30, copy_limit_bytes: 10485760, max_log_bytes: 1048576, max_artifact_bytes: 10485760 },
	};
	const directPath = join(fixture, "direct-request.json");
	await writeFile(directPath, JSON.stringify(request), { mode: 0o600 });
	const direct = response(await cli(["api", "--request", directPath, "--json"]));
	await rm(directPath);
	const fromTool = await invoke(request);
	assert.equal(fromTool.isError, false);
	assert.deepEqual(JSON.parse(fromTool.content[0].text), fromTool.details);
	const toolResponse = response(fromTool.details);
	assert.deepEqual(lastArgs.slice(0, 4), ["--root", root, "api", "--request"]);
	expectedSignal = undefined;
	await command.handler(JSON.stringify(request), ctx);
	expectedSignal = controller.signal;
	assert.equal(messages.length, 1);
	const fromSlash = response(JSON.parse(messages[0].content));
	const summarize = (response: { batch: { tasks: { id: string; result: { harness: string; state: string } }[]; succeeded: unknown } }) => {
		assert.equal(response.batch.succeeded, true);
		return response.batch.tasks.map(task => ({ id: task.id, harness: task.result.harness, state: task.result.state }));
	};
	assert.deepEqual(summarize(direct), summarize(toolResponse));
	assert.deepEqual(summarize(direct), summarize(fromSlash));
	assert.notEqual(direct.batch.batch_id, toolResponse.batch.batch_id);
	for (const response of [direct, toolResponse, fromSlash]) {
		assert.equal(response.batch.tasks.length, 2);
		assert.notEqual(response.batch.tasks[0].result.workspace_id, response.batch.tasks[1].result.workspace_id);
		for (const [index, task] of response.batch.tasks.entries()) {
			const run = task.result;
			const harness = ["fake-a", "fake-b"][index];
			assert.equal(run.harness, harness);
			assert.equal(run.state, "SUCCEEDED");
			assert.equal(run.prepared_state_id, prepared.state_id);
			assert.equal(run.worker_attempts[0].harness_requested, harness);
			assert.equal(run.worker_attempts[0].harness_resolved, harness);
			assert.equal(run.worker_attempts[0].backend_id, "trusted-local");
			assert.ok(run.worker_attempts[0].event_cursor >= 4);
			assert.equal(query("SELECT state FROM workspaces WHERE workspace_id=?", run.workspace_id)[0].state, "RECLAIMED");
			await assert.rejects(stat(run.workspace_path), { code: "ENOENT" });
			const files = query("SELECT relative_path, sha256, size_bytes FROM artifacts WHERE run_id=?", run.run_id);
			assert.ok(files.some(file => text(file.relative_path).endsWith("/manifest.json")), "sealed manifest missing from artifact records");
			for (const file of files) {
				const bytes = await readFile(join(root, text(file.relative_path)));
				assert.equal(bytes.length, file.size_bytes);
				assert.equal(createHash("sha256").update(bytes).digest("hex"), file.sha256);
			}
			const artifacts = join(root, "artifacts", run.artifact_manifest_id);
			const manifest = object(JSON.parse(await readFile(join(artifacts, "manifest.json"), "utf8")));
			assert.equal(manifest.run_id, run.run_id);
			assert.equal(manifest.harness_version, run.harness_version);
			assert.deepEqual(manifest.profile, run.profile);
			const events = (await readFile(join(artifacts, "events.jsonl"), "utf8")).trim().split("\n").map(line => object(JSON.parse(line))).filter(record => record.event).map(record => object(record.event));
			assert.equal(events[0].type, "started");
			assert.equal(events.at(-1).type, "completed");
			assert.ok((await readFile(join(artifacts, "tracked.patch"), "utf8")).includes(`+${["a", "b"][index]}`));
		}
	}
	assert.equal(await readFile(join(repo, "tracked"), "utf8"), "original");
	assert.equal(await readFile(join(text(prepared.prepared_path), "tracked"), "utf8"), "original");

	for (const unsupported of [{ model: "forbidden" }, { resume: "prior-session" }]) {
		const before = inventory();
		const invalid = structuredClone(request);
		Object.assign(invalid.tasks[1], { request: unsupported });
		const result = await invoke(invalid);
		assert.equal(result.isError, true);
		assert.equal(errorKind(result.details), "unsupported_capability");
		assert.deepEqual(inventory(), before, "unsupported request allocated lifecycle records");
	}
	for (const invalid of [{ ...request, version: 2 }, { ...request, options: { ...request.options, secret: "never-echo-this" } }]) {
		const before = inventory();
		const result = await invoke(invalid);
		assert.equal(result.isError, true);
		assert.equal(errorKind(result.details), "invalid_arguments");
		assert.ok(!result.content[0].text.includes("never-echo-this"));
		assert.deepEqual(inventory(), before);
	}
	const failedRequest = structuredClone(request);
	failedRequest.tasks[1].prompt = JSON.stringify({ exit_code: 9 });
	const failed = await invoke(failedRequest);
	assert.equal(failed.isError, true);
	const failedResponse = response(failed.details);
	assert.equal(failedResponse.version, 1);
	assert.equal(failedResponse.batch.succeeded, false);
	assert.equal(failedResponse.batch.tasks[1].result.state, "FAILED");
	for (const output of [
		{ stdout: "not JSON", stderr: "raw-secret-sentinel", code: 1, killed: false },
		{ stdout: JSON.stringify(direct), stderr: "raw-secret-sentinel", code: -1, killed: true },
		{ stdout: "{}", stderr: "raw-secret-sentinel", code: 0, killed: false },
		"throw" as const,
	]) {
		canned = output;
		const result = await invoke(request);
		assert.equal(result.isError, true);
		assert.equal(object(result.details).batch, undefined);
		assert.ok(!JSON.stringify(result).includes("secret"));
	}
	canned = { stdout: JSON.stringify(direct), stderr: "", code: 0, killed: false };
	delete process.env.FABRIC_ROOT;
	delete process.env.FABRIC_BINARY;
	await invoke(request);
	assert.equal(lastBinary, "fabric");
	assert.equal(lastArgs[0], "api");
	const beforeInvalidSlash = requests.length;
	expectedSignal = undefined;
	await command.handler("not-json-secret", ctx);
	assert.equal(requests.length, beforeInvalidSlash);
	assert.equal(errorKind(JSON.parse(messages[messages.length - 1].content)), "invalid_arguments");
	await cleanup();
	console.log("PASS: real CLI/tool/slash mixed batches, profiles, sealed artifacts/reclaim, typed no-allocation rejection, private request cleanup, safe transport failures (fake OMP host; no provider calls)");
} finally {
	if (previous.binary === undefined) delete process.env.FABRIC_BINARY; else process.env.FABRIC_BINARY = previous.binary;
	if (previous.root === undefined) delete process.env.FABRIC_ROOT; else process.env.FABRIC_ROOT = previous.root;
	await writableDirectories(fixture);
	await rm(fixture, { recursive: true, force: true });
}
