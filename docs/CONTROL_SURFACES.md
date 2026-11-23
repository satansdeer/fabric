# Fabric control surfaces

The native OMP extension exposes durable generic-worker controls through the
existing CLI/manager. A submit returns early batch/run handles; the local manager
owns admitted workers independently of the controller session. The legacy v1
foreground API remains available for debugging and compatibility.
Controller, worker harness and execution backend are separate choices.
Execution is macOS-first trusted-local, selected through an installed target driver, not remote/cloud or a security sandbox.

## Native OMP setup

Requires OMP **19.1.0**, a built Fabric CLI and its siblings
`fabric-fake-harness` and `fabric-local-target` for the deterministic example.
No npm install is required; OMP provides `ExtensionAPI`, `pi.zod`, and type-only imports.
From the Fabric checkout, build when needed:

```sh
cargo build -p fabric-cli --bins
export FABRIC_BINARY="$PWD/target/debug/fabric"
export FABRIC_ROOT="$HOME/.local/share/fabric"
omp -e "$PWD/integrations/omp/fabric.ts"
```

`FABRIC_BINARY` explicitly selects one executable (not a shell command). Without
it the extension executes `fabric` through PATH. `FABRIC_ROOT` explicitly selects
storage; when unset the extension omits `--root`, retaining Fabric's normal
XDG/home defaults. Relative paths are interpreted from the OMP invocation cwd;
absolute overrides are recommended. Prepare/register a project before submitting
workers. These overrides do not select worker profiles or authorize credentials.

The default export registers five typed tools with `loadMode: "essential"`, so
command-capable OMP hosts retain their original top-level names and schemas.
The generic `fabric` tool retains its default load mode:

| Tool | Arguments and behavior |
| --- | --- |
| `fabric_spawn` | `project`, inline `tasks`; optional `max_concurrency`, `placement`, `idempotency_key`. Returns early durable batch/run IDs. Placement selects an installed target ID, default `local`; unsupported/missing remote targets reject without local fallback. |
| `fabric_status` | `batch_id`; current durable states/events. |
| `fabric_results` | `batch_id`; persisted task results, source provenance, artifacts and actual failures. A successful retrieval does not mean the workers succeeded. |
| `fabric_cancel` | `batch_id`; explicit cancellation request, not a controller-disconnect side effect. |
| `fabric_wait` | `batch_id`, integer `timeout_seconds` from 0 through 10; bounded wait, not a full-batch blocking spawn. |

Each task has `task_id`, `harness_id`, and a complete independent `prompt`.
Omit `execution_context` for the connected native context and `model` for the
worker harness's own default. Native context requires prior explicit consent;
these tools never connect, prepare, authorize credentials or guess a model.
Optional `source_ref` is `{kind:"current-head"}`,
`{kind:"prepared",state_id:"..."}`, or `{kind:"commit",sha:"..."}`.
A READY exact base is required; no automatic preparation or dirty-change transfer.
Optional per-task `limits` carries the generic worker limits documented in README.

Foreground, batch and durable execution use the same `target.execute` boundary.
Core persists the accepted target/executable pin, retains source/prepared pins,
lease and workspace ownership, and independently observes process status and
seals/verifies artifacts. Reconnect reads durable evidence, not plugin readiness.
Installed targets choose request-scoped concrete primitives and receive observed
completion evidence; receipts are not success evidence. Core validates launch
authority before start dispatch and spawn. Reclaim requires apply, validated
ownership and durable intent; dry-run never invokes target reclaim.
Unavailable, changed, crashed or malformed target drivers cannot report success
or authorize cleanup. Committed host exit/capture, ownership, artifact and deletion
evidence survives failed receipts; failures are durably recorded separately.
Recovery refuses unknown evidence but can reclaim independently verified effects
under the existing policy; a failed target receipt is never cleanup proof.
The bundled `fabric-local-target` is used only when no registry local entry exists;
a revoked or broken installed entry never triggers a bundled/local fallback.
Unsupported PTY, persistent-environment and infrastructure-evidence capabilities
fail explicitly. See [the target contract](PLUGIN_PROTOCOL.md#installed-execution-targets).

The legacy tool `fabric` accepts `{ "request": <v1 JSON object> }`;
`/fabric <literal JSON request>` parses the same document without rewriting it
as a prompt. Both remain foreground debugging controls, not the normal swarm route.

The helper creates a fresh mode-0700 temporary directory, writes `request.json`
mode 0600 with exclusive creation, and removes the directory in `finally` on
normal completion, rejection, interruption or an exception. Legacy v1 calls use:

```text
pi.exec(binary, ["--root", root, "api", "--request", privatePath, "--json"], {cwd, signal})
```

Typed spawn uses `workers spawn --request privatePath`; the other typed controls
invoke their corresponding `workers` subcommands without a request file.

The `--root` pair is omitted unless explicitly configured. No shell expansion,
stdin transport, adapter-specific flags or credentials are added. The tool's
AbortSignal and context cwd are forwarded. There is **no outer exec timeout**:
Fabric's per-worker resource limits own deadlines. A killed process or malformed
response is a transport/protocol error, never a fabricated completed batch. Raw
stderr and exception text are not displayed; well-formed typed Fabric error JSON
can be displayed. Legacy foreground failed/partial batches retain their actual
response and return `isError: true`; successful batches return `isError: false`.
For typed durable tools, operation failure is separate from worker outcome:
successfully retrieved failed-worker evidence is not a transport error.

The tool returns JSON text plus the same document in `details`. Slash execution
uses `pi.sendMessage` with `display: true` and `triggerTurn: false`, without
`deliverAs`, `sendUserMessage`, or a follow-up model turn. A user can invoke the
slash command without a model call; a model choosing the tool is still an
ordinary controller model turn.

After an accepted spawn, a managed watcher uses bounded waits and appends changed
progress plus terminal results as visible ordinary untrusted controller-context
data. It never uses `deliverAs:"aside"`, starts an idle model turn, or substitutes
UI-only output for context. An idle controller consumes updates on its next turn.
Session switch/shutdown aborts watchers/timers only; admitted workers continue.
Reconnect with `fabric_results` and the saved batch ID; never blindly resubmit
after an uncertain transport error. Resume observation with a necessary status
or wait call. Do not hot-poll or loop zero-timeout waits.

Storage measurement failures remain fail-closed (`storage_meter_failed`). Terminal
results optionally include `storage_meter_failure`; `runs logs --json` exposes the
same evidence under `progress.storage_meter_failure`. Evidence contains an
allowlisted measurement `stage` and cause `code`; I/O causes add an allowlisted
`operation` and optional numeric `os_code`, never paths, raw errors, contents or
native logs. Legacy records without this evidence have an unknown cause. This
does not change storage caps, authorize replay, or identify the cause of the
earlier issue #38 failure; inspect evidence from the next released runtime first.

## Legacy version 1 foreground API

```sh
fabric --root /absolute/storage api --request /private/request.json --json
```

A request (the same JSON is passed as tool `request` or after `/fabric`):

```json
{
  "version": 1,
  "op": "spawn_many",
  "project": "demo",
  "concurrency": 2,
  "default_harness": "fake-a",
  "tasks": [
    {
      "id": "implement",
      "harness": "fake-a",
      "prompt": "{\"required_files\":[\".deps/ready\"],\"writes\":[{\"path\":\"tracked\",\"content\":\"implementation\"}]}",
      "request": {
        "model": "literal-model",
        "profile": {"harness_id":"fake-a","harness_version":"fabric-fake-a 1"}
      }
    },
    {
      "id": "review",
      "harness": "fake-b",
      "prompt": "{\"required_files\":[\".deps/ready\"],\"writes\":[{\"path\":\"review.txt\",\"content\":\"independent review\"}]}",
      "request": {
        "profile": {"harness_id":"fake-b","harness_version":"fabric-fake-b 1"}
      }
    }
  ],
  "options": {
    "timeout_seconds": 60,
    "copy_limit_bytes": 10485760,
    "max_log_bytes": 1048576,
    "max_artifact_bytes": 10485760
  }
}
```

For fake adapters, the inline prompt is their deterministic JSON fixture
protocol. Real adapters receive an ordinary task prompt string, not that fixture
JSON. Rust bounds the total request to 1 MiB and bounds tasks/prompts, validates
before allocating a batch/run/workspace, then owns private inline-prompt files
through completion. The extension independently caps serialized requests at
1 MiB. Original request/profile documents are not added as raw artifacts; the
existing private prompt snapshots and harness logs remain sensitive artifacts.

Only `version: 1` and `op: "spawn_many"` are accepted. Project must be nonempty;
concurrency must be positive. Tasks have `id`, `prompt`, optional `harness` and
optional/null `request`. `default_harness` is optional when every task supplies
its own. A missing/null task request uses the default HarnessRequest; an object
is a complete per-task override, not a merge. Existing HarnessRequest fields are
`profile`, `model`, `required_capabilities`, `placement`, `access`,
`hard_read_only`, `resume`, and `max_cost_usd`, subject to adapter capability and
profile policy. There is no secret/key field.

`options` and its four fields are optional. Omitted fields retain existing
RunOptions defaults (copy allowance zero, 10 MiB logs, 100 MiB artifacts, existing
worker timeout). Unknown fields are rejected, including background/remote
features not implemented here. Copy allowance is a whole-tree fallback
permission, not a quota; storage headroom/retention policy stays unchanged.
`--fake-binary /absolute/path` is available on `fabric api` only for selecting a
compatible deterministic fixture executable; the extension does not invent a
corresponding adapter flag.

Accepted execution returns exactly one JSON document, including failed batches.
The envelope and nested existing report shape are:

```ts
{
  version: 1,
  op: "spawn_many",
  batch: {
    batch_id: string,
    succeeded: boolean,
    tasks: Array<{
      id: string,
      run_id: string,
      result: RunReport | null,
      error: string | null
    }>
  }
}
```

`RunReport` is the existing full report, not a new control-surface summary. It
retains harness identity, worker attempts, prepared revision, profile evidence,
exit/capture, artifact manifest and cleanup evidence. Failed/partial accepted
batches exit nonzero but keep this same envelope. Preflight rejection returns
`{"error":{"kind":"<typed-kind>","message":"<safe-diagnostic>"}}` and exits
nonzero, without an accepted batch response or lifecycle allocation. CLI
preflight errors use stderr; accepted batch responses use stdout. Consumers
must not require successful exit to read an accepted failed batch. Separate
executions have separate batch/run/workspace IDs; schema and semantics, not IDs,
are identical across surfaces.

## Runnable deterministic scenario

No provider, account or paid model call is involved. Save the request above as
`request.json`, then use a disposable registered source:

```sh
export FABRIC_BINARY="$(pwd)/target/debug/fabric"
export FABRIC_ROOT="$(mktemp -d)/storage"
SOURCE="$(mktemp -d)"
git -C "$SOURCE" init --quiet
printf original > "$SOURCE/tracked"
git -C "$SOURCE" add tracked
git -C "$SOURCE" -c user.name=Fixture -c user.email=fixture@example.invalid commit --quiet -m fixture
printf "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n" > "$SOURCE/prepare.toml"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" project add --name demo --path "$SOURCE"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" project prepare demo --recipe "$SOURCE/prepare.toml" --json
"$FABRIC_BINARY" --root "$FABRIC_ROOT" api --request "$PWD/request.json" --json
omp -e "$PWD/integrations/omp/fabric.ts"
```

Inside OMP, paste `/fabric ` followed by the literal request JSON (a single line
is convenient). Or let an already running controller call tool `fabric` with
`{"request": <the same document>}`. Each call is a new batch. Never send an
unregistered slash command as a nonbilling test: an unknown slash can become a
model prompt. In RPC mode, first verify `fabric` via `get_available_commands`,
then require command dispatch with `agentInvoked: false`.

For automated provider-free checks, with Bun and both built binaries:

```sh
bun integrations/omp/conformance.ts "$PWD/target/debug/fabric"
bun integrations/omp/typed-conformance.ts "$PWD/target/debug/fabric"
bun integrations/omp/host-smoke.ts "$PWD/target/debug/fabric" /absolute/path/to/omp /absolute/path/to/om-pstack/omp
```

`conformance.ts` creates its own disposable Git source, preparation recipe and
storage, then uses a tiny fake ExtensionAPI with **real Fabric subprocesses**.
It submits the identical mixed fake-a/fake-b document directly, through the
tool and through slash; checks schema, harness/profile identities, attempts,
sealed artifact hashes and reclaimed workspaces; and verifies the source/base
remain unchanged. Model override or resume on fake-b must produce typed
`unsupported_capability` with unchanged counts in `batches`, `batch_tasks`,
`runs`, `workspaces`, and `worker_attempts`. Invalid version/unknown options also
must allocate nothing. It checks private requestfile modes/deletion and safe
handling of killed/nonzero/invalid responses and exceptions. It uses no shell
wrapper, new package, daemon or provider credential. A fake extension host does
not prove OMP's actual loader/tool-schema/RPC behavior; that control-plane proof
is a separate installed-OMP check recorded by the coordinating verification.

`typed-conformance.ts` checks early durable handles, bounded wait, ordinary
progress delivery, controller reconnect, independent worker failure/artifacts,
cancel and fail-closed remote rejection. `host-smoke.ts` drives the installed
OMP 19.1.0 RPC host with private HOME and real tool registration/schema parsing;
it asserts the five original active names with Bash enabled and validates their
registered native parameter schemas; it proves four-worker handles, progress,
reconnect, artifact digests/reclaim and
cancellation without an agent/provider turn. Passing the generated pstack plugin
directory also checks actual `/skill:swarm` discovery. This does not prove a model
chooses the route or that a live worker can authenticate. The public darwin-arm64
OMP 19.1.0 binary reports all five originals active even before the explicit load
mode, and its `getAllTools()` metadata omits schemas. This check uses the actual
registered native parsers, not that metadata; it does not attest provider-wire
schemas or reproduce the source-tag command partition in this binary.

## Explicit local pstack swarm

The generated [om-pstack OMP plugin](https://github.com/satansdeer/om-pstack)
keeps ordinary `/skill:swarm` on native OMP `task`. It does not monkeypatch that
tool, replace the native scheduler or treat a native subagent as a Fabric run.
Load its generated `omp` directory with OMP's `--plugin-dir` alongside this
extension. For an already initialized/connected project, explicitly request:

```text
/skill:swarm --fabric-local --fabric-project demo --fabric-harness omp <brief>
```

This real-harness route can incur provider spending and requires separate
authorization; command discovery alone does not execute or verify it.
The arguments are interpreted by the skill/controller, not a new executable.
Select an explicit registered project (or actually discovered single unambiguous
registration); never infer it from a directory basename. Harness defaults to
`omp` only when omitted. Missing tools/consent/READY source are blockers, not
permission for an implicit setup, model substitution or native-task fallback.

The controller creates distinct inline tasks, uses `fabric_spawn`, saves early
IDs, continues independent work, then retrieves/aggregates results through typed
controls. Coverage/race rules, exact source SHAs, verification methods,
`PASS`/`ISSUES`/`BLOCKED`, retries and explicit gaps remain the skill's contract.
Worker self-reports and durable handles alone are not PASS evidence.
Required remote/cloud placement or credential/security isolation is `BLOCKED`
before local allocation, never silently downgraded.

Provider-free checks verify generation, native skill discovery and typed
execution with real local fake workers. They do not prove a model selected the
route, native OMP/Codex authentication, live worker execution or cloud parity.
Those live gates remain `NOT_VERIFIED`/`BLOCKED` without separate authorization.

## Legacy foreground cancellation limits

The CLI must remain alive until the foreground batch finishes. Fabric handles
SIGINT/SIGTERM through its existing cancellation token and worker supervision.
OMP 19.1.0 exec AbortSignal cancellation sends SIGTERM with roughly a one-second
grace before hard kill; an OMP exec timeout instead immediately hard-kills.
That is why the extension passes the signal but sets no exec timeout. Ordinary
cancel can reach Fabric's existing cleanup, but forced kill, extension-host
termination or controller loss cannot guarantee sealed final evidence or
completion of temporary-file `finally` cleanup. Use existing durable run
inspection, reconciliation and strict reclaim; do not automatically resubmit an
uncertain batch. There is no background/resume/retry guarantee and no new
lifecycle owner.

These foreground limits apply to `fabric`/`/fabric`, not accepted typed durable
submissions. A typed client interruption or controller close does not itself
cancel admitted workers. Save the early IDs and inspect their durable state.

## Generic MCP mapping — DESIGN ONLY, not shipped

A future MCP tool could accept `{request: <versioned Fabric JSON object>}` and
map directly to `fabric_core::api::spawn_many`, returning the same BatchResponse
or typed preflight error. It must not duplicate scheduling, profiles, policy,
workspace ownership or deletion. A stdio transport is a candidate for local
controllers. HTTP would be a separate, explicitly authenticated future service,
not an unauthenticated wrapper or an advertised endpoint today.

MCP process ambient credentials must not authorize workers. Connections require
explicit scoped authorization/profile references; raw credentials do not belong
in tool arguments, output or server logs. Before shipping, define how transport
cancellation maps to the existing cancellation token, how controller loss is
reported without promising completion, and how callers inspect durable results
without claiming resumable sessions. Neither the MCP transport nor any harness
adapter becomes the lifecycle/reclaim owner. No executable MCP placeholder or
server installation is included.

## Pinned upstream source evidence

- [OMP 19.1.0 extension guide](https://github.com/can1357/oh-my-pi/blob/v19.1.0/docs/extensions.md): default factory, `pi.zod`, native tool/command registration.
- [Extension API types](https://github.com/can1357/oh-my-pi/blob/v19.1.0/packages/coding-agent/src/extensibility/extensions/types.ts): execute argument order, context cwd, `sendMessage` display/no-turn behavior and literal `exec(command,args,options)`.
- [Exec implementation](https://github.com/can1357/oh-my-pi/blob/v19.1.0/packages/coding-agent/src/exec/exec.ts): signal/cwd/timeout options, stdout/stderr/code/killed result; no stdin option.
- [Process-tree cancellation](https://github.com/can1357/oh-my-pi/blob/v19.1.0/packages/utils/src/ptree.ts): graceful abort versus immediate timeout kill.
- [Extension loading](https://github.com/can1357/oh-my-pi/blob/v19.1.0/docs/extension-loading.md): explicit `-e` paths and `--no-extensions` explicit-only behavior.

These are oh-my-pi sources, not the om-pstack fork's domain/task schema. Source
contracts and deterministic tests do not imply verified paid harness behavior.
