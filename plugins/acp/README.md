# ACP v1 transport and external native bridge

The offline-packaged SDK plugin reuses the ACP v1 transport, with an explicit
initialize-only inspection command (#101). No runtime dependencies; Node >=22.18,
ESM. One connection and one text turn, not another scheduler.
**Provider-free integration is not native provider support or completion of #95.**
Native initialization and live-turn gates require separate approval and evidence.

## Release profile-authoring kit (v0.1.22 candidate)

The macOS archive ships these genuine authoring sources at `plugins/acp/`,
including this README, the packager, bridge, inspector, config and transport.
It reuses the archive's single `sdk/`; it does not ship a preconfigured
installable ACP profile or a second SDK. `release.json.acp_profile_kit` identifies
the kit/version/protocol and its authoring-only meaning; `release.json.files`
pins every kit, SDK and labelled fake-peer payload.

From the extracted archive root, use `plugins/acp/package.mjs` as below and
`sdk/cli.mjs` for installation.
Use only an owner-reviewed absolute Node and explicit local launch input.
Packaging is offline and never executes the selected native program. No native
executable, HOME path, credentials, settings, account/provider/model or supported
status is prebound; the owner explicitly binds executable/runtime/settings
hashes and native HOME while packaging their own profile.

The shipped `fixtures/turn-agent.mjs` is a **deterministic fake ACP peer**, not
Cline, Cursor or native provider support. The extracted release smoke copies it
into an owned temporary profile, packages/installs via shipped files, checks
initialize-only inspection, then proves an isolated real-host text/edit turn,
sealed artifact hashes, unchanged source and owned child/workspace cleanup.
No checkout imports, compilation, Bun, npm hooks, native install/update/login,
inherited credentials or network/model calls participate in that ACP proof.
Missing kit files, broken SDK relocation or invalid success/artifact evidence
fail the smoke rather than skipping coverage. Runnable candidate regression:

```sh
/absolute/node /absolute/extracted/scripts/smoke-release.mjs --archive \
  /absolute/fabric-v0.1.22-macos-arm64.tar.gz "$HASH" "$SOURCE_SHA" /absolute/new-install
```

This command also retains the ordinary OMP/Codex/local release smoke and its
public OMP version download; only the ACP portion is offline. For a locally
packaged, unreviewed candidate use `--local` instead of `--archive`.
v0.1.22 publication/downloaded acceptance remains manager-owned. Recorded native
Cline 3.0.75 initialization is not native session/model/controller/worker proof.
Cursor second-native-profile proof and remaining #95 gates stay **OPEN**;
neither kit shipment nor fake/initialize-only success completes stage one.


## API

```js
import { launchAcp } from './transport.mjs';

const connection = launchAcp({
  executable: approvedAbsoluteExecutable,
  args: approvedLiteralArguments,
  cwd: approvedAbsoluteDirectory,
  env: approvedExplicitEnvironment,
  onNotification(message, signal) {
    // Deliver untrusted data to the caller's consumer. Do not log raw content.
    // Asynchronous consumers should stop when signal is aborted.
    return consumeNotification(message, signal);
  },
  signal: callerAbortSignal, // optional, owns the entire connection
});
try {
  const metadata = await connection.initialize();
  // metadata is untrusted, not authentication readiness or launch authority.
} finally {
  await connection.close();
}
```

`launchAcp(options)` launches exactly the supplied absolute executable with literal
`args`, `shell: false`, the supplied absolute `cwd`, and an environment built only
from own entries of `env` (no inherited process environment). The child runtime/OS
may add its own entries. All four launch fields and `onNotification` are required.
Invalid options or synchronous spawn failure throw a fixed diagnostic before
returning; asynchronous spawn failure terminates the returned connection.
No PATH discovery, installation, login, environment dumping, auth-store copying,
shell interpolation, remote transport or detached process.
The executable and its arguments can themselves perform side effects: caller
policy/consent must approve them **before launch**. Initialization only describes
this module's protocol exchange, not a guarantee about arbitrary child behavior.

- `initialize({ signal } = {})` sends exactly one `initialize`, protocolVersion
  `1`, empty `clientCapabilities`, and client identity. Concurrent/repeated calls
  return the same promise, never another exchange. Returns
  `{ protocolVersion, agentCapabilities, authMethods, agentInfo }`. Missing
  capabilities/auth methods become `{}`/`[]`; omitted capability fields mean
  unsupported. Missing/null agent identity becomes `null`.
- Known capability shapes, agent identity and agent auth methods are validated.
  Unknown capability/negotiation fields fail closed; extensions may be carried
  in opaque object/null `_meta` fields. Nullable session/logout capability
  objects are supported. Terminal auth is rejected because the client does not
  advertise terminal authentication. No metadata, including `_meta`, grants
  local capabilities, establishes identity, or implies authentication readiness.
- `request(method, params = {}, { signal, timeoutMs } = {})` returns a promise
  for a matching result. Available only after successful initialization; cannot
  reinitialize. Multiple calls may complete out of order. IDs are connection-local
  increasing safe integers; agent request IDs occupy the opposite direction.
  This is a raw RPC API, **not** permission to invoke authentication/session/prompt
  methods; higher layers must supply that policy. Nothing invokes them implicitly.
  Explicit `timeoutMs: null` disables only that RPC's timer and requires an
  `AbortSignal` owner; framing, pending limits, disconnect and cancellation remain active.
- `notify(method, params = {})` sends a notification after initialization, with
  no fabricated response. A caller may explicitly send `session/cancel`; the
  module does not infer ACP sessions or prompt-turn cancellation semantics.
- `onNotification(message, signal, requestPendingAtReceipt)` receives the full
  notification envelope, including unknown methods. The third argument snapshots
  whether any outbound RPC was pending at receipt, before deferred callbacks or
  promise continuations; the single-prompt bridge uses it for phase validation.
  Delivery starts in wire order without awaiting earlier callbacks; async
  completions may be unordered. Replies continue while callbacks wait. Callback
  throws/rejections fail the connection. Consumers must sanitize strings/URIs
  before display and never act on unknown methods as grants.
- All incoming agent **requests** receive JSON-RPC `-32601`, `Method not found`.
  There is no request handler/permission auto-approval. In particular
  `cursor/ask_question`, `cursor/create_plan`, `session/request_permission`, file
  and terminal requests cannot block on or receive fake human answers.
  Optional `failOnPeerRequest: true` also terminates with
  `acp_unsupported_request`; default `false` preserves transport-only behavior.
  The bridge enables it, so denying a blocking request cannot become success.
- `cancel()`, `close()`, request/initialization abort, or launch-signal abort
  terminate the connection. Await these methods or `closed` to wait for child
  reaping. `closed` always resolves `{ code }`, retaining the first terminal reason;
  `pid` identifies only this owned child for lifecycle observation.

## Exact resource and failure bounds

Numeric timeout options stay bounded; only a signal-owned per-request override can omit its timer:

| Option | Default | Allowed range |
| --- | ---: | ---: |
| `maxRecordBytes` | 1,048,576 | 256–16,777,216 |
| `maxPendingRequests` | 64 | 1–1,024 |
| `maxNotificationCallbacks` | 64 | 1–1,024 |
| `requestTimeoutMs` | 10,000 | 1–300,000 |
| `initializeTimeoutMs` | 10,000 | 1–300,000 |
| `terminationGraceMs` | 250 | 0–5,000 |

Numeric per-request `timeoutMs` has the same allowed range as `requestTimeoutMs`.
Deadlines cover RPC write/response, not arbitrary caller work. There is no idle
or whole-worker timer; timers exist only for active RPCs and owned termination.
Event-loop scheduling and teardown can extend wall-clock completion.

Records are UTF-8 JSON objects terminated by LF. `maxRecordBytes` excludes LF,
includes CR if present, and applies both directions. A fixed receive buffer of
that size holds fragmented bytes, including split multibyte UTF-8; decoding is
fatal for invalid UTF-8. Exceeding the bound fails immediately even without LF.
Empty, malformed, truncated, batched and mixed request/response frames fail closed.
JSON-RPC envelopes reject extra fields. Incoming IDs must be strings or safe
integers (not null, boolean or fractional); response matching is type-sensitive.
Invalid, unknown or duplicate response IDs are terminal protocol errors.

The stdin write queue is limited to `maxRecordBytes + 1` bytes (including LF);
a new write exceeding it terminates rather than creating an unbounded queue.
Serialization of caller-owned outgoing values happens before the size check:
callers should not construct unbounded payloads. At most `maxPendingRequests`
RPCs and `maxNotificationCallbacks` unsettled notification deliveries are held.
A pending-limit rejection does not close the connection; notification/write
queue overflow does, without silently dropping notifications. Callbacks that
ignore abort cannot be forcibly stopped or awaited by this module; their resource
ownership remains with the caller. Incoming retained payload memory is bounded
by the record size and pending/delivery counts, excluding caller-retained data.

EOF, child exit, malformed frames, failed/overdue initialization, deadline,
abort, and close reject outstanding calls with the same first terminal Error.
A completed call cannot be retroactively rejected by a later duplicate response.
Ordinary valid RPC error replies reject only their call with `acp_remote_error`;
an initialize error is terminal `acp_initialize_failed`. All local Errors have
fixed identical `.code`/`.message`, no child-supplied message/data/cause. Other
codes are `acp_invalid_options`, `acp_spawn_failed`, `acp_not_ready`,
`acp_invalid_request`, `acp_pending_limit`, `acp_id_exhausted`,
`acp_invalid_initialize`, `acp_unsupported_protocol`, `acp_malformed_frame`,
`acp_truncated_frame`, `acp_frame_too_large`, `acp_invalid_envelope`,
`acp_invalid_response_id`, `acp_callback_failed`, `acp_callback_overflow`,
`acp_write_overflow`, `acp_deadline`, `acp_aborted`, `acp_closed`,
`acp_unsupported_request`, and `acp_disconnected`. There is no stderr/payload logging or raw capture API.
Stderr is drained and discarded. No retries, version fallback, replay or reconnect.

Terminal cleanup destroys owned pipes, sends SIGTERM to the launched child,
escalates to SIGKILL after the grace period if still alive, and waits for Node's
child `close` event. No killing by name, process discovery or detached process
group. **Trusted-local, same-user execution, not a sandbox.** This standalone
module owns/reaps the direct child only; it cannot contain a malicious agent or
promise cleanup of escaped descendants. The Fabric bridge retains
Core's process-group ownership and leaves the bridge and agent in that group.

## Installable native SDK bridge

`package.mjs` builds a deterministic directory/tarball using the existing SDK
packager; no downloads, lifecycle scripts, compilation or provider probes.
`fabric-acp-bridge` is the discoverable executable, with an absolute,
package-bound Node shebang (not `/usr/bin/env`, a shell, or a PATH-selected Node).
The native SDK control endpoint and turn entrypoint are the same executable.
Core must discover that installed file; `command.program` is exactly
`context.binary`. Its turn arguments are `--turn` and one `{prompt:true}`;
Core substitutes literal text, never shell syntax. A lookalike elsewhere on PATH
is rejected before launching the ACP child.

### Explicit offline setup

Review an absolute executable and literal arguments yourself. Hashes must come
from independently approved files, not agent metadata. Example nonsecret
`launch-input.json` for a native binary:

```json
{
  "executable": {"path": "/approved/absolute/agent", "sha256": "<64 lowercase hex>"},
  "args": ["acp"],
  "nativeHome": "/approved/absolute/native-home",
  "cwd": "fabric-workspace",
  "pins": [],
  "requestTimeoutMs": 60000
}
```

All paths must already exist, be absolute and canonical. Executable/runtime
files are SHA-256 checked during packaging, probe/context/plan/command and again
before the child launches. Explicit additional `pins` bind script/resource files.
Shebang ACP entrypoints are rejected: select an approved absolute runtime and
literal absolute script argument instead, pinning that script. When that runtime
is the bridge's Node, its first argument must be a pinned script (no loader flags).
This is not transitive dependency attestation or a sandbox: review the executable,
arguments and its dependencies. Same-user filesystem replacement races are not
atomically excluded.

`cwd` has exactly one supported policy: `fabric-workspace`. The bridge passes
Core's canonical working directory literally both to spawn and `session/new`;
it cannot select a different user/project directory. The explicit spawn environment
contains only `HOME` (the configured native home) and `LC_ALL=C`; macOS may add
`__CF_USER_TEXT_ENCODING` inside the child runtime even with an empty environment.
No inherited PATH, credentials, `NODE_OPTIONS`, loader environment or arbitrary
environment field is accepted.
Native config/credential contents stay in native stores; neither packaging nor
the bridge reads/copies them. Never put credentials in arguments or configuration.
Native stores/configuration remain mutable and are not fingerprinted as model
or authentication evidence.

Use an explicitly approved absolute Node >=22.18 executable for these commands.
`REGISTRY` must be a private canonical directory outside Git;
`FABRIC` is the absolute installed Fabric binary. Registration is explicit:

```sh
/absolute/node plugins/acp/package.mjs launch-input.json /absolute/new-package /absolute/acp.tgz
# Retain/review the printed package sha256 and new-package/launch.json.
/absolute/node sdk/typescript/cli.mjs install /absolute/acp.tgz \
  --registry "$REGISTRY" --fabric "$FABRIC" --version 0.1.0 --sha256 "$HASH" --trust
export FABRIC_PLUGIN_REGISTRY="$REGISTRY"
export FABRIC_ACP_HOME=/approved/absolute/native-home
export PATH="$REGISTRY/packages/$HASH:/usr/bin:/bin"
"$FABRIC" harnesses doctor acp --json
```

Changing executable, argv, native home, pins, setup timeout or runtime requires a
new reviewed package/hash and explicit SDK upgrade; no launch field comes from
peer metadata. Probe/version describe **this bridge**, not a native agent version.
`agentInfo`, auth methods, modes/models and capability metadata are untrusted,
never consent or readiness. No authenticate, install, login or model-selection RPC.

### Opt-in initialize-only inspection

After reviewing the installed package/hash and `launch.json`, explicitly select
an **existing, canonical, absolute directory owned by the current user**. Use a
dedicated disposable workspace; native executables can have startup side effects.
The command accepts exactly `--inspect --cwd DIRECTORY`, no config override or extra flags:

```sh
/usr/bin/env -i "$REGISTRY/packages/$HASH/fabric-acp-bridge" \
  --inspect --cwd /approved/absolute/owned-inspection-workspace
```

The packaged shebang binds the approved absolute Node used for packaging.
The command reuses `config.mjs` validation and `transport.mjs`, reads only the
bounded installed launch configuration and pinned files, and verifies runtime,
executable and resource hashes before launching. It does not read native HOME
contents. The child receives only the bound `HOME` and `LC_ALL=C`, literal argv
and the explicit cwd. No PATH resolution, shell, loader environment, auth store,
discovery, installation, update, login, retry or detached process.
Probe/doctor/context/plan never invoke this command or launch the ACP peer.

For an approved script-based native installation, select its actual absolute
runtime, **not a shell wrapper**. For example, use
`"executable":{"path":"/approved/native/node","sha256":"<approved runtime hash>"}`,
`"args":["/approved/native/index.js","acp"]` and
`"pins":[{"path":"/approved/native/index.js","sha256":"<approved script hash>"}]`
in the launch input above. Use the installed program's reviewed literal ACP
arguments (Cline's documented flag is `--acp`), and pin additional reviewed
resources via `pins`. Package with the separately approved bridge Node.
The bridge runtime is pinned automatically; do not use `env` shebangs, `-e`,
loader flags, PATH aliases or unreviewed wrapper-derived arguments.
Hashes identify approved local bytes, not a vendor version or transitive dependencies.

Only `initialize` is sent: no `authenticate`, `session/new`, `model/list`,
`session/prompt` or session cancellation RPC. The handshake is capped at the
smaller of the packaged setup timeout and 10 seconds; records are limited to
65,536 bytes, one pending request and 16 notification callbacks. Notifications
are discarded without retaining/displaying payloads. Unsupported peer requests
fail with `acp_unsupported_request` rather than waiting for human answers.
SIGINT/SIGTERM abort negotiation or cleanup. All launched paths close and await
direct-child reaping, escalating SIGTERM to SIGKILL after 250 ms. No success is
emitted before owned teardown completes; exit zero from the peer is not success.

One bounded JSON line reports `stage` (`negotiated` or `failed`), a fixed `code`,
`failed_stage` on failure, `protocolVersion` only after validated negotiation,
and `launch` evidence (bridge identity, config/executable/runtime/resource
SHA-256 values, never paths). `cleanup` is `not_started` or `reaped`.
`auth`/`model` remain `unverified`; `peer_identity` remains `untrusted`.
Exit status is 0 only for negotiation plus owned cleanup; failures exit 1.
No argv, native-home/config paths, credentials, raw stderr, remote error data,
auth methods, capabilities or `agentInfo` are emitted.

This is **initialize-only evidence**, not authentication, model readiness,
installed vendor-version proof or a live Fabric worker turn. The direct-child
ownership/same-user replacement limits above still apply: this command is not
a sandbox and cannot reclaim malicious escaped descendants.

### Remaining native gates

Primary native entrypoint contracts: [Cursor `agent acp`](https://cursor.com/docs/cli/acp)
and [Cline `cline --acp`](https://docs.cline.bot/usage/acp).
Cline does not auto-approve tools by default; do not enable auto-approval.
These sources describe invocation, not proof of an installed executable or version.

Installation, version, negotiation and live-turn evidence are tracked separately
in #89/#93/#95. This inspector never authorizes installation, updates or login.
For Cline 3.0.75, startup auto-update runs before ACP initialization. Before its
first launch, create `.cline/data/settings/global-settings.json` under the
approved private native HOME with
`{"autoUpdateEnabled":false,"telemetryOptOut":true}` and include that file in
the launch pins. Use the pinned native executable directly with `--acp`, not
the npm wrapper. This is version-specific startup preparation, not a generic
ACP requirement or an authentication guarantee. See the
[pinned Cline source](https://github.com/cline/cline/tree/06f0ec711ea417430d552b98e7911fdac6f495a1).
Separately authorized auth/model/live-turn acceptance remains a gate.
Fixture or initialize-only success cannot turn the support matrix green or close #95.

### Native context / current host limit

The selected `FABRIC_ACP_HOME` must exactly match the packaged native home.
Inspection returns a native-default profile and `credential_available:false`:
generic ACP cannot prove authentication or model readiness without a separate
typed policy. `fabric harness connect acp` consequently **fails closed** in the
current host (`crates/fabric-core/src/onboarding.rs` requires credential
availability). The plugin does not forge readiness to make that route green.

The existing explicit-profile API is usable for an operator-reviewed invocation:

```sh
# Run inside the registered, prepared, committed source checkout.
"$FABRIC" --root "$STORAGE" init --harness acp --check --json > /private/inspection.json
jq '.native_context.profile' /private/inspection.json > /private/acp-profile.json
# Review native-home exposure and the installed package/launch configuration.
"$FABRIC" --root "$STORAGE" run PROJECT --harness acp --placement local \
  --profile /private/acp-profile.json --prompt-file /private/prompt.txt --json
```

This explicit profile is execution consent, **not** verified authentication.
No automatic onboarding, model resolution, credential-readiness success or
provider support is claimed. Native-default only: model overrides/exact policies,
resume/followup, cost limits, imported skills/plugins/MCP, inspect/publish access,
and nonlocal placement are rejected. Core owns workspace/patch/cancellation.
Only headless text turns and structured text/tool/failure normalization are native
capabilities. Tool notifications add no execution or approval capability.
All other optional capabilities remain unsupported.

### Turn and normalization contract

The bridge performs `initialize` → `session/new` (empty `mcpServers`) →
`session/prompt` (one text block). IDs are connection-local; no session persistence,
restart, replay or second turn. Core `session/update` notifications must match the
exact session during the pending prompt, including the wire-receipt phase check.
Supported rendered variants are `agent_message_chunk` text, `tool_call` and
`tool_call_update`. Each assistant text chunk is at most 32,768 UTF-8 bytes; output
buffering is at most 1 MiB. Control characters except tab/newline are removed from text.
Malformed, mismatched or out-of-phase core updates, unknown core methods and
non-text assistant messages still fail closed.

**Private reasoning and checklist subset (stable v1):** `agent_thought_chunk`
accepts the existing standard content-block shapes and annotations, optional
string/null `messageId`, and object/null `_meta`. Legacy `plan` requires an
`entries` array; each entry requires string `content`, priority
`high`/`medium`/`low`, and status `pending`/`in_progress`/`completed`, with optional
object/null `_meta` on the entry and update. Empty checklists are valid.
These notifications are validated and discarded, **not rendered or retained**:
reasoning, IDs, plan content, annotations and metadata never become assistant
output, tool labels, diagnostics or extension evidence. They do not imply
execution, tool completion, billing or turn success; only the real prompt result
determines completion. No reasoning history or plan accumulation is kept.
Processing remains bounded by the existing 1 MiB transport frame and 64 in-flight
notification callback limits, with no additional ceiling or capability.
`plan_update`/`plan_removed` still fail closed: they require the client's `plan`
capability, which the bridge does not advertise. Other standard session updates,
auth/readiness/resume/permission semantics and native-provider support remain
outside this subset under #95.

**Tool lifecycle subset (stable v1):** initial calls require string `toolCallId`
and `title`; omitted kind/status default to `other`/`pending`. Updates require
only the ID; omitted/null optional fields leave previous evidence unchanged.
Raw input/output accept any JSON value, not only objects. Standard content blocks
(text, image, audio, resource link, embedded resource), diffs, locations and
terminal references are shape-validated and discarded. Subagent session content
is rejected: no `subagents` capability is advertised. Peer titles, names, IDs,
commands, arguments, output, paths, content and `_meta` are never used as native
names or diagnostics, retained as raw tool evidence, executed, opened or applied.
Filesystem, terminal and permission requests remain unsupported even after a
tool notification; the bridge advertises empty client capabilities.

At most 256 distinct tool IDs are correlated per turn using fixed-size hashes;
the next distinct ID fails with `acp_tool_overflow`. No lifecycle history or raw
metadata is kept. Native tool names are bridge-assigned
`acp.tool.<ordinal>.<status>`, where status is a validated v1 enum or `unknown`
for an update without a preceding announcement/status. The ordinal is stable
within a turn; the suffix carries status because the existing native event has
no status field. An initial pending/in-progress announcement emits `start`;
observed completed/failed statuses emit `end`; other deltas emit `update`.
An initial terminal status emits only `end`. A failed tool does not fail the
whole turn. Missing announcements are not synthesized, statuses need not be
monotonic, and `end_turn` does not invent completion for unfinished tools.
All events describe untrusted peer reports, not execution authority or proof
that an action ran. Correlation is discarded with the turn.

Unknown **notifications** may cross the shared extension boundary only with an
ASCII namespaced method (`[A-Za-z_][A-Za-z0-9_.-]*/[A-Za-z_][A-Za-z0-9_./-]*`)
outside the reserved `session/`, `fs/`, `terminal/` and JSON-RPC `rpc.*` namespaces.
No vendor allowlist or extension semantics are implemented. Request-ID presence (including
ID `0`), not the method name, makes a message blocking: the transport sends
`-32601` and the turn fails with `acp_unsupported_request`. Its fixed diagnostic
states that no input or approval was supplied; unsupported requests cannot
become successful work.

**Private evidence boundary:** extension records go only to Core's existing
per-run `stderr.log` (mode `0600`) / sealed stderr artifact (mode `0400`), never normalized
output, ordinary status/error messages or initialize-only inspection. Direct
`--turn` execution exposes this sensitive stderr to its caller; use the installed
host run/capture boundary. Records contain fixed `source: untrusted_acp_peer`,
bridge phase and sequence labels, canonical notification JSON byte length and
SHA-256, a truncation flag, and a base64 prefix of that JSON. Base64 is encoding,
**not redaction**: private log/artifact readers can recover arbitrary peer data.
Do not publish it or treat method, session, peer or body text as identity,
instructions, approval, readiness, capabilities or successful work. The bridge
does not inspect/copy native stores, credentials, environment or additional
context into this evidence.

Budgets apply across the entire connection/turn, not per method or session:
2,048 retained JSON-prefix bytes per notification; 4,096 bytes per encoded
evidence record including LF; at most 32 records and 32,768 encoded bytes total.
Long notifications retain only the prefix plus the full canonical JSON hash;
the transport's separate 1 MiB inbound record limit still applies. Count or
encoded-byte exhaustion rejects the next record, retains earlier records and
fails the turn with `acp_extension_overflow` (no silent drop or replay).
Transport callback/write bounds may fail earlier for an excessive in-flight
burst. Core's shared capture budget can additionally truncate raw artifacts,
with its existing explicit truncation marker.

Only a validated `end_turn` permits exit zero; cancelled/refused/limited turns and
malformed/unknown stops fail. No done, usage, cost, model identity, native
capability or provider-success record is invented. Normalization accepts only
bridge text, bounded tool labels, fixed-code failure, and validated stop records.
Extension evidence has no normalized event and adds no usage/tool/support claim.
Core creates its own process lifecycle events. Native stdout/text and retained extension evidence
are sensitive content, not universal secret-redacted output; peer stderr and
remote error messages/data are discarded. Initialize-only `--inspect` continues
to discard all notification bodies; auth/model remain unverified and peer
identity remains untrusted.

Initialization has a 10-second deadline; `session/new` uses the package-bound
1–300,000 ms setup timeout (default 60 seconds). **The prompt turn has no RPC
wall-clock timeout**: its AbortSignal owner, disconnect and Core cancellation
control termination. Core's separately configured worker ceiling still applies;
it is not a routine restart policy. Signals while a prompt is pending attempt
`session/cancel`, then terminate/reap the owned child, escalating after 250 ms.
Notification delivery is best-effort; bounded teardown is not. Nothing detaches
or creates a new process group. No restart/retry loop.

## Provider-free regression

```sh
FABRIC_ACP_TEST_HOST=/absolute/built/fabric /absolute/node --test plugins/acp/*.test.mjs
```

The host integration test requires a real built Fabric; it never silently skips.
Without `FABRIC_ACP_TEST_HOST`, it uses this checkout's `target/debug/fabric`.
No SDK native-reference conformance helper is used.

The check packages/installs into an isolated registry, uses real fixture children
through Core probe/context/capabilities/plan/command/normalization and local target
primitives, verifies text/patch/artifact hashes, malformed and mismatched events,
tool lifecycles/deltas/defaults, malformed tool shapes, unsupported modalities and
requests, remote failure, invalid stops and disconnect. Tool checks include
arbitrary JSON input/output, non-monotonic and orphan updates, terminal initial
reports, private sentinel non-disclosure, inert content/diffs/paths/terminals,
correlation overflow and initialize/session-new/post-response phase rejection.
Private-update checks cover thought/plan/text/tool receipt ordering in one write,
nullable metadata, standard reasoning content, empty/replacement checklists,
metadata-only turns without fabricated output, malformed required/optional
fields, unsupported capability-dependent variants, wrong sessions, premature/late
updates, oversized frames and repeated private updates without accumulation.
Extension checks cover interleaved exact-session text/patch success, private bounded
evidence, truncation/hash integrity, count/byte floods and blocking vendor
requests (including the same method as an accepted notification with ID `0`).
Two independently active installed-plugin turns have distinct literal prompts,
children, evidence and owned workspaces: SIGINT cancels one with a pending prompt
after private thought/plan and extension/tool delivery while the other remains alive, then the survivor
produces only its own evidence/text/patch and terminal tool event. Cancelled tools
receive no fabricated end. Both workspaces are reclaimed and both children reaped
without replay; a fixture release file makes overlap deterministic.
It also checks longer-than-setup-deadline turns, process-group inheritance and
changed pins without executing replacements.
It clones the local existing commit into a temporary fixture repo; no commit,
remote, provider, credential store or supervisor storage is touched.
Owned temporary trees/children are cleaned up. The packaged initialize-only
checks cover post-initialize RPC exclusion, secret/metadata non-disclosure,
invalid/changed launch input, deadlines, malformed/oversized frames, notification
overflow, blocking peer requests, exit zero, signals during initialize/teardown
and TERM-ignoring child reaping. No absent native agent is silently skipped.
Transport-only checks remain independent:
`/absolute/node --test plugins/acp/transport.test.mjs`.

Protocol references: [initialization](https://agentclientprotocol.com/protocol/v1/initialization.md),
[stdio](https://agentclientprotocol.com/protocol/v1/transports.md),
[schema](https://agentclientprotocol.com/protocol/v1/schema.md),
[tool calls](https://agentclientprotocol.com/protocol/v1/tool-calls.md),
[prompt turns](https://agentclientprotocol.com/protocol/v1/prompt-turn.md),
[agent plans](https://agentclientprotocol.com/protocol/v1/agent-plan.md),
[security](https://agentclientprotocol.com/protocol/v1/security.md).
