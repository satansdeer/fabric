# Fabric control surfaces (HN-06)

Fabric's CLI JSON API and native OMP extension submit one versioned request to
`fabric_core::api::spawn_many`, which delegates to the existing worker pool.
The control agent, worker harness and execution backend are separate choices.
Neither surface replaces v0.1 preparation, durable storage, supervision, capture
or safe workspace reclaim. Execution is currently foreground, trusted-local,
macOS-first; this is not a daemon, remote executor or multitenant sandbox.

## Native OMP setup

Requires OMP **19.1.0**, a built Fabric CLI and its sibling
`fabric-fake-harness` for the deterministic example. No npm install is required;
OMP provides `ExtensionAPI` and `pi.zod`, and imports of that package are type-only.
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

The default export registers:

- Tool **`fabric`**, label **Fabric**, parameters `{ "request": <JSON object> }`.
  The wrapper deliberately does not duplicate Rust's versioned request schema.
- Slash **`/fabric <literal JSON request>`**. Arguments are parsed as JSON, not
  rewritten as a prompt. Both registrations invoke the same helper.

The helper creates a fresh mode-0700 temporary directory, writes `request.json`
mode 0600 with exclusive creation, and removes the directory in `finally` on
normal completion, rejection, interruption or an exception. It calls exactly:

```text
pi.exec(binary, ["--root", root, "api", "--request", privatePath, "--json"], {cwd, signal})
```

The `--root` pair is omitted unless explicitly configured. No shell expansion,
stdin transport, adapter-specific flags or credentials are added. The tool's
AbortSignal and context cwd are forwarded. There is **no outer exec timeout**:
Fabric's per-worker resource limits own deadlines. A killed process or malformed
response is a transport/protocol error, never a fabricated completed batch. Raw
stderr and exception text are not displayed; well-formed typed Fabric error JSON
can be displayed. Accepted failed/partial batches retain their actual response
and return `isError: true`; successful batches return `isError: false`.

The tool returns JSON text plus the same document in `details`. Slash execution
uses `pi.sendMessage` with `display: true` and `triggerTurn: false`, without
`deliverAs`, `sendUserMessage`, or a follow-up model turn. A user can invoke the
slash command without a model call; a model choosing the tool is still an
ordinary controller model turn.

## Version 1 API

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

For automated, no-new-dependency conformance, with Bun and both built binaries:

```sh
bun integrations/omp/conformance.ts "$PWD/target/debug/fabric"
```

This script creates its own disposable Git source, preparation recipe and
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

## pstack `/swarm` as a generic consumer

A pstack controller can prepare one versioned Fabric document like the example
above: implement on fake-a with its model/profile, independently review on
fake-b with another profile, and submit one `fabric` tool call. No worker needs
a harness-specific launch script; adapters choose their native argv/protocol.
The incompatible fake-b model/resume case is an explicit preflight failure, not
a silent fallback. The conformance scenario exercises this generic controller
boundary, not pstack's native scheduler.

There are **no native pstack task-schema, role or `/swarm` implementation
changes**. Native pstack subagents remain opaque harness-internal work. Fabric
workers are separate durable runs with their own private workspaces; one native
subagent is not automatically one Fabric worker.

For an opt-in real-harness variation, change the two task harnesses to `omp` and
`codex`, use ordinary prompt strings, remove fake-a's `model` override, and use
separate explicit profiles, for example:

```json
[
  {"harness_id":"omp","harness_version":"omp/19.1.0","connection_refs":[{"kind":"env","name":"OPENAI_API_KEY"}]},
  {"harness_id":"codex","harness_version":"codex-cli 0.171.0","connection_refs":[{"kind":"env","name":"CODEX_API_KEY"}]}
]
```

Assign each complete profile object to its own task's `request.profile`; this
array is explanatory, not the API document. Values must already be configured
and explicitly authorized; never paste credentials into JSON or controller
messages. Native config references are also nonsecret, adapter-specific opt-ins;
see README's profile rules. Do not infer equivalent models, permissions, native
subagent isolation or portable config. OMP/Codex worker execution remains
**NOT_VERIFIED live** here (source/fixture conformance only). Installed OMP
extension loading/slash verification is control-plane evidence, not provider
or live-worker validation.

Verified on installed OMP 19.1.0: an isolated RPC session loaded this extension,
enumerated `/fabric`, and completed the mixed fake-a/fake-b request. Its separate
`prompt_result` frame reported `agentInvoked:false`, `status:completed` and
`sessionSettled:true`; the initial prompt acknowledgement alone is not completion.
Both runs succeeded, all 28 registered artifact hashes/sizes matched, and both
workspaces were reclaimed. No model/provider request was made.

## Foreground and cancellation limits

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
