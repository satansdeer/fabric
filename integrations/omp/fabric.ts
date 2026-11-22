import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

type JsonObject = Record<string, unknown>;
type BatchResponse = {
	version: 1;
	op: "spawn_many";
	batch: { batch_id: string; succeeded: boolean; tasks: JsonObject[] };
};
type ErrorResponse = { error: { kind: string; message: string } };
type Result = { value: BatchResponse | ErrorResponse; isError: boolean };

function object(value: unknown): value is JsonObject {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

function parse(text: string): unknown {
	try {
		return JSON.parse(text);
	} catch {
		return undefined;
	}
}

function batchResponse(value: unknown): value is BatchResponse {
	if (!object(value) || value.version !== 1 || value.op !== "spawn_many" || !object(value.batch)) return false;
	const batch = value.batch;
	return typeof batch.batch_id === "string" && typeof batch.succeeded === "boolean" && Array.isArray(batch.tasks)
		&& batch.tasks.every(task => object(task) && typeof task.id === "string" && typeof task.run_id === "string"
			&& (task.result === null || object(task.result)) && (task.error === null || typeof task.error === "string"));
}

function errorResponse(value: unknown): value is ErrorResponse {
	return object(value) && object(value.error) && typeof value.error.kind === "string"
		&& typeof value.error.message === "string";
}

function failure(kind: string, message: string): Result {
	return { value: { error: { kind, message } }, isError: true };
}

export default function fabricExtension(pi: ExtensionAPI) {
	async function invoke(request: unknown, cwd: string, signal?: AbortSignal): Promise<Result> {
		if (!object(request)) return failure("invalid_arguments", "Fabric request must be a JSON object.");
		try {
			const serialized = JSON.stringify(request);
			if (Buffer.byteLength(serialized) > 1024 * 1024) {
				return failure("invalid_arguments", "Fabric request exceeds the 1 MiB API limit.");
			}
			const directory = await mkdtemp(join(tmpdir(), "fabric-omp-"));
			try {
				await chmod(directory, 0o700);
				const path = join(directory, "request.json");
				await writeFile(path, serialized, { mode: 0o600, flag: "wx" });
				await chmod(path, 0o600);
				const args = process.env.FABRIC_ROOT === undefined ? [] : ["--root", process.env.FABRIC_ROOT];
				args.push("api", "--request", path, "--json");
				// OMP's outer timeout hard-kills; Fabric owns per-worker deadlines and cleanup.
				const output = await pi.exec(process.env.FABRIC_BINARY || "fabric", args, { cwd, signal });
				if (output.killed || signal?.aborted) {
					return failure("fabric_interrupted", "Fabric was interrupted; final batch evidence is not guaranteed. Inspect durable runs before retrying.");
				}
				const value = parse(output.stdout);
				if (batchResponse(value)) return { value, isError: output.code !== 0 || !value.batch.succeeded };
				// Only typed Fabric errors are exposed, never raw stderr or exception text.
				const rejection = errorResponse(value) ? value : parse(output.stderr);
				if (output.code !== 0 && errorResponse(rejection)) return { value: rejection, isError: true };
				return failure("fabric_protocol_error", "Fabric did not return a valid API response; no batch result can be claimed.");
			} finally {
				await rm(directory, { recursive: true, force: true });
			}
		} catch {
			return failure("fabric_transport_error", "Fabric could not complete the API exchange; inspect durable runs before retrying.");
		}
	}

	const z = pi.zod;
	pi.registerTool({
		name: "fabric",
		label: "Fabric",
		description: "Run a versioned Fabric API request. Fabric validates the generic JSON document and owns worker lifecycle, profiles, artifacts and reclaim. Never include credentials.",
		strict: false,
		parameters: z.object({ request: z.object({}).passthrough() }),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const result = await invoke(params.request, ctx.cwd, signal);
			return { content: [{ type: "text", text: JSON.stringify(result.value) }], details: result.value, isError: result.isError };
		},
	});

	pi.registerCommand("fabric", {
		description: "Execute a literal versioned Fabric JSON request (no model turn)",
		async handler(args, ctx) {
			const result = await invoke(parse(args), ctx.cwd);
			pi.sendMessage({ customType: "fabric", content: JSON.stringify(result.value), display: true, details: result.value }, { triggerTurn: false });
		},
	});
}
