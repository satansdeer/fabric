// Bun integrations/omp/host-smoke.ts [fabric-binary] [omp-binary] [optional-pstack-omp-plugin-directory]
// Uses the installed OMP host, native schemas/registrations and actual tool.execute; never submits a model prompt.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { Database } from "bun:sqlite";

type Document = Record<string, unknown>;
function object(value: unknown): Document { assert.ok(value !== null && typeof value === "object" && !Array.isArray(value)); return value as Document; }
function text(value: unknown): string { assert.equal(typeof value, "string"); return value as string; }
function records(value: unknown): Document[] { assert.ok(Array.isArray(value)); return value.map(object); }
const binary = resolve(process.argv[2] || "target/debug/fabric");
const omp = resolve(process.argv[3] || process.env.OMP_BINARY || join(process.env.HOME || "", ".local/bin/omp"));
const plugin = process.argv[4] || process.env.FABRIC_PSTACK_PLUGIN_DIR;
const fixture = await mkdtemp(join(tmpdir(), "fabric-omp-host-"));
const root = join(fixture, "storage");
const repo = join(fixture, "source");
const environment = {
	PATH: "/usr/bin:/bin", HOME: fixture, LC_ALL: "C", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null",
	GIT_AUTHOR_NAME: "Fabric fixture", GIT_AUTHOR_EMAIL: "fixture@example.invalid",
	GIT_COMMITTER_NAME: "Fabric fixture", GIT_COMMITTER_EMAIL: "fixture@example.invalid",
	PI_CODING_AGENT_DIR: join(fixture, "omp-agent"), PI_NO_TITLE: "1", FABRIC_ROOT: root,
	FABRIC_BINARY: binary,
};
async function exec(command: string, args: string[], cwd = repo) {
	const child = Bun.spawn([command, ...args], { cwd, env: environment, stdout: "pipe", stderr: "pipe" });
	const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
	return { stdout, stderr, code };
}
async function cli(args: string[]) {
	const output = await exec(binary, ["--root", root, ...args]);
	assert.equal(output.code, 0, `Fabric CLI failed: ${args.join(" ")} ${output.stderr}`);
	return object(JSON.parse(output.stdout));
}
async function eventually(condition: () => boolean | Promise<boolean>, reason: string, timeout = 25000) {
	const deadline = Date.now() + timeout;
	while (!await condition()) { assert.ok(Date.now() < deadline, reason); await Bun.sleep(100); }
}
async function writable(path: string) {
	if (!(await stat(path)).isDirectory()) return;
	await chmod(path, 0o700);
	for (const entry of await readdir(path)) await writable(join(path, entry));
}
const clients: { close(): Promise<void> }[] = [];
const batches: string[] = [];
let passed = false;
function controller() {
	const args = ["--mode", "rpc", "--no-ui", "--no-session", "--no-title", "--no-extensions", "--no-rules", "--no-context-files", "--no-lsp",
		"--model", "openai/gpt-5.2", "--api-key", "fabric-control-plane-test-not-a-real-key", "--max-cost", "0.000000001", "-e", join(fixture, "driver.ts")];
	if (plugin) args.push("--plugin-dir", resolve(plugin)); else args.push("--no-skills");
	const child = Bun.spawn([omp, ...args], { cwd: repo, env: environment, stdin: "pipe", stdout: "pipe", stderr: "pipe" });
	const frames: Document[] = [];
	const stderr = new Response(child.stderr).text();
	let failure: Error | undefined;
	let closed = false;
	const output = (async () => {
		let buffered = "";
		const decoder = new TextDecoder();
		for await (const chunk of child.stdout) {
			buffered += decoder.decode(chunk, { stream: true });
			let newline: number;
			while ((newline = buffered.indexOf("\n")) !== -1) {
				const line = buffered.slice(0, newline); buffered = buffered.slice(newline + 1);
				if (!line.trim()) continue;
				try {
					const frame = object(JSON.parse(line)); frames.push(frame);
					if (frame.type === "agent_start" || frame.type === "extension_error" || frame.type === "rpc_frame_error") {
						failure = new Error(`Forbidden host event: ${JSON.stringify(frame)}`); child.kill();
					}
				} catch (error) { failure = error instanceof Error ? error : new Error(String(error)); child.kill(); }
			}
		}
		assert.equal(buffered.trim(), "", "OMP emitted a partial RPC frame");
	})().catch(error => { failure = error instanceof Error ? error : new Error(String(error)); child.kill(); });
	let sequence = 0;
	async function request(type: string, fields: Document = {}) {
		const id = `proof-${++sequence}`;
		child.stdin.write(`${JSON.stringify({ id, type, ...fields })}\n`);
		await eventually(() => {
			if (failure) throw failure;
			return frames.some(frame => frame.type === "response" && frame.id === id);
		}, `OMP did not answer ${type}`);
		const response = frames.find(frame => frame.type === "response" && frame.id === id)!;
		assert.equal(response.success, true, JSON.stringify(response));
		if (type === "prompt") {
			const data = response.data ? object(response.data) : {};
			if (data.agentInvoked !== false) {
				await eventually(() => frames.some(frame => frame.type === "prompt_result" && frame.id === id), "slash command did not finish locally");
				const done = frames.find(frame => frame.type === "prompt_result" && frame.id === id)!;
				assert.equal(done.agentInvoked, false); assert.equal(done.status, "completed"); assert.equal(done.sessionSettled, true);
			}
		}
		if (failure) throw failure;
		return response.data ? object(response.data) : {};
	}
	async function messages() { return records((await request("get_messages")).messages); }
	async function invoke(tool: string, params: Document, validate = false) {
		const call = `call-${sequence + 1}`;
		await request("prompt", { message: `/fabric-proof ${JSON.stringify({ call, tool, params, validate })}` });
		let answer: Document | undefined;
		await eventually(async () => {
			answer = (await messages()).find(message => message.customType === "fabric-proof-answer" && object(message.details).call === call);
			return !!answer;
		}, "actual host omitted the slash-command tool result");
		return object(object(answer!.details).result);
	}
	const value = {
		request, invoke, messages,
		async close() {
			if (closed) return;
			closed = true; child.stdin.end();
			const deadline = setTimeout(() => child.kill(), 10000);
			let code: number;
			try { code = await child.exited; } finally { clearTimeout(deadline); }
			await output;
			const diagnostics = await stderr;
			await writeFile(join(fixture, `host-${clients.indexOf(value) + 1}.jsonl`), frames.map(frame => JSON.stringify(frame)).join("\n") + "\n");
			await writeFile(join(fixture, `host-${clients.indexOf(value) + 1}.stderr`), diagnostics);
			assert.equal(code, 0, diagnostics);
			if (failure) throw failure;
			assert.equal(frames.some(frame => frame.type === "agent_start"), false, "provider invocation attempted");
		},
	};
	clients.push(value); return value;
}

const errors: unknown[] = [];
try {
	await mkdir(repo);
	const version = await exec(omp, ["--version"]);
	assert.equal(version.code, 0); assert.equal(version.stdout.trim(), "omp/19.1.0", "smoke requires the pinned installed OMP 19.1.0");
	for (const args of [["init", "--quiet"], ["config", "core.hooksPath", "/dev/null"]]) assert.equal((await exec("/usr/bin/git", args)).code, 0);
	await writeFile(join(repo, "tracked"), "original\n");
	assert.equal((await exec("/usr/bin/git", ["add", "."])).code, 0);
	assert.equal((await exec("/usr/bin/git", ["commit", "--quiet", "-m", "fixture"])).code, 0);
	await cli(["project", "add", "--name", "demo", "--path", repo, "--json"]);
	const recipe = join(fixture, "prepare.toml");
	await writeFile(recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n");
	const prepared = await cli(["project", "prepare", "demo", "--recipe", recipe, "--json"]);
	const fake = join(dirname(binary), "fabric-fake-harness");
	const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
	await writeFile(join(fixture, "gated-fake"), `#!/bin/sh
set -eu
for arg in "$@"; do if [ "$arg" = --version ]; then exec ${quote(fake)} "$@"; fi; done
case "$1" in --protocol-a|--protocol-b) prompt=$(cat "$3");; *) exit 89;; esac
case "$prompt" in *'worker-one'*) id=one;; *'worker-two'*) id=two;; *'worker-three'*) id=three;; *'worker-four'*) id=four;; *) exit 90;; esac
root=${quote(fixture)}
touch "$root/started.$id"
trap 'exit 143' TERM INT
i=0
while [ ! -f "$root/release.$id" ] && [ ! -f "$root/release-all" ]; do
 i=$((i+1)); test "$i" -lt 3000; sleep 0.02
done
${quote(fake)} "$@"
`, { mode: 0o700 });
	// Only intercept registration to retain definitions and inject the fake adapter binary into real pi.exec.
	// Every definition/schema/message is forwarded to the actual installed host, unchanged.
	await writeFile(join(fixture, "driver.ts"), `import fabric from ${JSON.stringify(join(import.meta.dir, "fabric.ts"))};
import type { ExtensionAPI, ExecOptions, ToolDefinition } from "@oh-my-pi/pi-coding-agent";
type NativeSchema = {parse(value:unknown):Record<string,unknown>;safeParse(value:unknown):{success:boolean}};
export default function proof(pi:ExtensionAPI) {
 const tools=new Map<string,ToolDefinition>();
 const forwarded=new Proxy(pi,{get(target,key){
  if(key==="registerTool") return (definition:ToolDefinition)=>{tools.set(definition.name,definition);target.registerTool(definition);};
  if(key==="exec") return (command:string,args:string[],options?:ExecOptions)=>target.exec(command,args.includes("spawn")?[...args,"--fake-binary",${JSON.stringify(join(fixture, "gated-fake"))}]:args,options);
  const value=Reflect.get(target,key);return typeof value==="function"?value.bind(target):value;
 }});
 fabric(forwarded);
 pi.registerCommand("fabric-proof",{description:"Disposable deterministic provider-free actual-tool proof",async handler(args,ctx){
  const input=JSON.parse(args);
  if(input.tool==="__active") {
   const active=pi.getActiveTools();
   const result={active};
   pi.sendMessage({customType:"fabric-proof-answer",content:JSON.stringify(result),display:true,details:{call:input.call,result}},{triggerTurn:false});
   return;
  }
  const tool=tools.get(input.tool);if(!tool) throw new Error("Missing actual registered tool "+input.tool);
  const schema=tool.parameters as unknown as NativeSchema;
  const result=input.validate?{valid:schema.safeParse(input.params).success}:await tool.execute(input.call,schema.parse(input.params),new AbortController().signal,undefined,ctx);
  pi.sendMessage({customType:"fabric-proof-answer",content:JSON.stringify(result),display:true,details:{call:input.call,result}},{triggerTurn:false});
 }});
}
`);
	const first = controller();
	const available = records((await first.request("get_available_commands")).commands);
	assert.ok(available.some(command => command.name === "fabric-proof"));
	assert.ok(available.some(command => command.name === "fabric"));
	if (plugin) assert.ok(available.some(command => command.name === "skill:swarm"), "actual installed OMP did not discover the staged pstack /skill:swarm");
	const active = await first.invoke("__active", {});
	const typed = ["fabric_spawn", "fabric_status", "fabric_results", "fabric_cancel", "fabric_wait"];
	assert.ok(Array.isArray(active.active) && active.active.includes("bash"), "default command projection requires active bash");
	for (const name of typed) {
		assert.ok(active.active.includes(name), `original typed tool is not active: ${name}`);
	}
	console.log(`PASS: actual installed-host active original typed names (native schemas validated below): ${typed.join(", ")}`);
	assert.equal((await first.invoke("fabric_wait", { batch_id: "batch", timeout_seconds: 11 }, true)).valid, false, "native wait schema is unbounded");
	assert.equal((await first.invoke("fabric_spawn", { project: "demo", tasks: [{ task_id: "x", harness_id: "fake-a", prompt: "literal", unexpected: true }] }, true)).valid, false, "native task schema accepts unknown intent");
	for (const [fields, valid] of [
		[{ execution_context: { kind: "native" }, source_ref: { kind: "current-head" } }, true],
		[{ execution_context: { kind: "explicit", harness_version: "fixture", native_config_refs: [{ kind: "config", path: "literal" }], connection_refs: [{ kind: "profile", name: "local" }] }, source_ref: { kind: "commit", sha: "abc" }, limits: { max_cost_usd: 0 } }, true],
		[{ execution_context: { kind: "native", harness_version: "unexpected" } }, false],
		[{ execution_context: { kind: "explicit" } }, false],
		[{ source_ref: { kind: "prepared", state_id: "prepared", sha: "unexpected" } }, false],
		[{ source_ref: { kind: "unsupported" } }, false],
		[{ limits: { timeout_seconds: 0.5 } }, false],
		[{ limits: { max_cost_usd: -1 } }, false],
	] as const) {
		const result = await first.invoke("fabric_spawn", { project: "demo", tasks: [{ task_id: "schema", harness_id: "fake-a", prompt: "literal", ...fields }] }, true);
		assert.equal(result.valid, valid, `native typed schema validation: ${JSON.stringify(fields)}`);
	}
	const ids = ["one", "two", "three", "four"];
	const tasks = ids.map((id, index) => ({ task_id: id, harness_id: index % 2 ? "fake-b" : "fake-a", source_ref: { kind: "prepared", state_id: prepared.state_id }, limits: { timeout_seconds: 60 },
		prompt: JSON.stringify({ required_files: [".deps/ready"], writes: [{ path: "tracked", content: `worker-${id} ; $(touch INJECTED) 'quoted'\n` }], ...(id === "three" ? { exit_code: 9 } : {}) }) }));
	const remote = await first.invoke("fabric_spawn", { project: "demo", tasks, placement: "remote" });
	assert.equal(remote.isError, true); assert.equal(object(object(remote.details).error).kind, "unsupported_placement");
	const spawn = await first.invoke("fabric_spawn", { project: "demo", tasks, max_concurrency: 4, idempotency_key: "actual-host-four" });
	assert.equal(spawn.isError, false); const accepted = object(spawn.details);
	const batch = text(accepted.batch_id); batches.push(batch);
	assert.equal(accepted.accepted, true);
	assert.equal(new Set(records(accepted.tasks).map(task => text(task.run_id))).size, 4);
	for (const id of ids) await eventually(() => Bun.file(join(fixture, `started.${id}`)).exists(), `worker ${id} never started`);
	const waiting = object((await first.invoke("fabric_wait", { batch_id: batch, timeout_seconds: 1 })).details);
	assert.equal(waiting.terminal, false);
	assert.equal(records(waiting.tasks).length, 4);
	assert.ok(records(waiting.tasks).every(task => task.terminal === false), "spawn returned only after a gated worker completed");
	await writeFile(join(fixture, "release.one"), ""); await writeFile(join(fixture, "release.three"), "");
	let progress: Document | undefined;
	await eventually(async () => {
		progress = (await first.messages()).find(message => message.customType === "fabric-progress" && message.excludeFromContext !== true
			&& object(message.details).batch_id === batch && records(object(message.details).tasks).some(task => task.task_id === "one" && task.state === "SUCCEEDED")
			&& records(object(message.details).tasks).some(task => task.task_id === "two" && task.terminal === false));
		if (!progress) await Bun.sleep(400);
		return !!progress;
	}, "actual host did not append partial progress to ordinary controller context");
	assert.equal(progress!.display, true, "controller progress was not visible");
	await first.close();
	const second = controller();
	const partial = object((await second.invoke("fabric_results", { batch_id: batch })).details);
	assert.equal(partial.terminal, false); assert.equal(records(partial.tasks).find(task => task.task_id === "one")?.state, "SUCCEEDED");
	await writeFile(join(fixture, "release-all"), "");
	for (let attempt = 0; attempt < 5; attempt++) {
		if (object((await second.invoke("fabric_wait", { batch_id: batch, timeout_seconds: 10 })).details).terminal) break;
		assert.ok(attempt < 4, "durable workers never completed after controller reconnect");
	}
	const results = object((await second.invoke("fabric_results", { batch_id: batch })).details);
	assert.equal(results.terminal, true); assert.equal(results.succeeded, false);
	assert.deepEqual(records(results.tasks).map(task => task.state), ["SUCCEEDED", "SUCCEEDED", "FAILED", "SUCCEEDED"]);
	const workspaces = new Set<string>();
	const database = new Database(join(root, "fabric.db"));
	try {
		for (const task of records(results.tasks)) {
			assert.equal(object(task.source).prepared_state_id, prepared.state_id);
			assert.equal(object(task.source).base_commit_sha, prepared.source_commit_sha);
			const run = object(task.result); const workspace = text(run.workspace_id); workspaces.add(workspace);
			assert.equal(records(run.worker_attempts)[0].model_requested, null);
			assert.equal(database.query<Document, string[]>("SELECT state FROM workspaces WHERE workspace_id=?").get(workspace)?.state, "RECLAIMED");
			await assert.rejects(stat(text(run.workspace_path)), { code: "ENOENT" });
			const artifacts = records(task.artifacts); assert.ok(artifacts.length);
			for (const artifact of artifacts) {
				const bytes = await readFile(join(root, text(artifact.relative_path)));
				assert.equal(bytes.length, artifact.size_bytes); assert.equal(createHash("sha256").update(bytes).digest("hex"), artifact.sha256);
			}
			const patch = artifacts.find(artifact => text(artifact.relative_path).endsWith("tracked.patch")); assert.ok(patch);
			assert.ok((await readFile(join(root, text(patch.relative_path)), "utf8")).includes(`+worker-${task.task_id}`));
		}
	} finally { database.close(); }
	assert.equal(workspaces.size, 4); assert.equal(await readFile(join(repo, "tracked"), "utf8"), "original\n");
	assert.equal(await readFile(join(text(prepared.prepared_path), "tracked"), "utf8"), "original\n");
	assert.equal(await Bun.file(join(repo, "INJECTED")).exists(), false);
	await rm(join(fixture, "release-all")); await rm(join(fixture, "release.one")); await rm(join(fixture, "started.one"));
	const cancel = object((await second.invoke("fabric_spawn", { project: "demo", tasks: [tasks[0]] })).details);
	const cancelBatch = text(cancel.batch_id); batches.push(cancelBatch);
	await eventually(() => Bun.file(join(fixture, "started.one")).exists(), "cancellation worker never started");
	assert.equal(object((await second.invoke("fabric_cancel", { batch_id: cancelBatch })).details).acknowledged, true);
	for (let attempt = 0; attempt < 3; attempt++) if (object((await second.invoke("fabric_wait", { batch_id: cancelBatch, timeout_seconds: 10 })).details).terminal) break;
	assert.equal(records(object((await second.invoke("fabric_results", { batch_id: cancelBatch })).details).tasks)[0].state, "CANCELLED");
	await second.close();
	passed = true;
	console.log(`PASS: actual installed OMP ${version.stdout.trim()}, native schemas/tools, early four-worker handles, visible ordinary-context progress, controller close/reconnect, independent real failure, digests/reclaim and cancellation; no agent/provider turn${plugin ? "; actual /skill:swarm discovered" : ""}. Live model routing/authentication NOT_VERIFIED.`);
} catch (error) {
	errors.push(error);
} finally {
	for (const client of clients) {
		try { await client.close(); } catch (error) { errors.push(error); }
	}
	try {
		if (await Bun.file(join(root, "fabric.db")).exists()) {
			await writeFile(join(fixture, "release-all"), "");
			for (const batch of batches) {
				await cli(["cancel", batch, "--json"]);
				assert.equal((await cli(["wait", batch, "--timeout-seconds", "60", "--json"])).terminal, true, "cleanup left workers running");
			}
			await cli(["manager", "stop", "--json"]);
		}
	} catch (error) {
		errors.push(error);
	} finally {
		if (passed && !errors.length && process.env.FABRIC_SMOKE_KEEP !== "1") { await writable(fixture); await rm(fixture, { recursive: true, force: true }); }
		else console.error(`Actual-host smoke evidence retained: ${fixture}`);
	}
}
if (errors.length) throw new AggregateError(errors, "Actual OMP host smoke or owned cleanup failed");
