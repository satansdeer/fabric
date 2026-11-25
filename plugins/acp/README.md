# ACP v1 local transport foundation

Stage one of issue #95, implemented by #97. **Not a supported Fabric worker or
installable Fabric plugin.** No runtime dependencies; Node >=22.18, ESM. This
module provides reusable local stdio transport and nonbilling initialization,
not another scheduler or agent loop.

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
- `notify(method, params = {})` sends a notification after initialization, with
  no fabricated response. A caller may explicitly send `session/cancel`; the
  module does not infer ACP sessions or prompt-turn cancellation semantics.
- `onNotification(message, signal)` receives the full notification envelope,
  including unknown methods. Delivery starts in wire order without awaiting
  earlier callbacks; async completions may be unordered. Replies continue while
  callbacks wait. Callback throws/rejections fail the connection. Consumers must
  sanitize strings/URIs before display and never act on unknown methods as grants.
- All incoming agent **requests** receive JSON-RPC `-32601`, `Method not found`.
  There is no request handler/permission auto-approval. In particular
  `cursor/ask_question`, `cursor/create_plan`, `session/request_permission`, file
  and terminal requests cannot block on or receive fake human answers.
- `cancel()`, `close()`, request/initialization abort, or launch-signal abort
  terminate the connection. Await these methods or `closed` to wait for child
  reaping. `closed` always resolves `{ code }`, retaining the first terminal reason;
  `pid` identifies only this owned child for lifecycle observation.

## Exact resource and failure bounds

All numeric options are integer bounds, never infinite/disabled deadlines:

| Option | Default | Allowed range |
| --- | ---: | ---: |
| `maxRecordBytes` | 1,048,576 | 256–16,777,216 |
| `maxPendingRequests` | 64 | 1–1,024 |
| `maxNotificationCallbacks` | 64 | 1–1,024 |
| `requestTimeoutMs` | 10,000 | 1–300,000 |
| `initializeTimeoutMs` | 10,000 | 1–300,000 |
| `terminationGraceMs` | 250 | 0–5,000 |

Per-request `timeoutMs` has the same allowed range as `requestTimeoutMs`.
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
`acp_write_overflow`, `acp_deadline`, `acp_aborted`, `acp_closed`, and
`acp_disconnected`. There is no stderr/payload logging or raw capture API.
Stderr is drained and discarded. No retries, version fallback, replay or reconnect.

Terminal cleanup destroys owned pipes, sends SIGTERM to the launched child,
escalates to SIGKILL after the grace period if still alive, and waits for Node's
child `close` event. No killing by name, process discovery or detached process
group. **Trusted-local, same-user execution, not a sandbox.** This standalone
module owns/reaps the direct child only; it cannot contain a malicious agent or
promise cleanup of escaped descendants. A later Fabric integration must retain
Core's process-group ownership and leave the bridge and agent in that group.

## Next native SDK integration slice: actual host boundary

Source evidence in this prepared tree:

- [`sdk/typescript/index.mjs:30–75`](../../sdk/typescript/index.mjs):
  `defineNativeHarness` requires probe/nativeContext/capabilities/plan/command/
  normalize callbacks; `serveNative` is the Fabric control protocol, **not ACP**.
- [`sdk/typescript/index.d.mts:122–188`](../../sdk/typescript/index.d.mts):
  native metadata selects the executable; `NativeCommand` takes literal args
  and `{ prompt: true }`, with selected credential references rather than values.
- [`crates/fabric-runner/src/native_plugin.rs:114–174,384–493`](../../crates/fabric-runner/src/native_plugin.rs):
  Core discovers `manifest.native.executable`; `command.program` must equal that
  discovered path. Exactly one `{ prompt: true }` is required; Core substitutes
  literal prompt text. Returning Node/a bridge instead of a discovered ACP agent,
  or omitting the prompt argument, is rejected. Environment proposals are also
  restricted to selected credentials/roots/controller state.
- [`crates/fabric-runner/src/lib.rs:245–272`](../../crates/fabric-runner/src/lib.rs):
  Core clears environment, supplies private roots as needed, launches with null
  stdin and piped output in its owned process group. A direct stdio ACP agent
  cannot receive initialization or session traffic on that null stdin.

Compatible approach for review: package an actual discoverable, content-pinned
**native ACP bridge executable** as the selected harness executable (not an alias
claiming an existing agent binary). Its future native metadata must discover the
bridge itself; `harness.command` returns exactly `context.binary` with exactly one
prompt marker. The bridge accepts that literal prompt argument, owns the ACP
child's pipes using this module, and emits bounded native records for the existing
normalizer. Core continues owning the bridge/child process group; neither detaches.
Its runtime and underlying agent launch configuration need explicit reviewed
packaging/profile/consent rather than environment or executable substitution.
This meets the current executable/prompt/stdin contract without changing Core;
no such executable, manifest, selection alias or session implementation is added
here.

That next slice still needs native SDK packaging, exact-version probing,
capability/profile evidence, consent/auth readiness policy, `session/new` and
`session/prompt` event normalization, permission routing, cancellation, usage/cost
semantics and artifact/session lifecycle integration. Stage one implements none
of those higher-level worker requirements and does not advertise ACP support.

## Provider-free check

```sh
node --test plugins/acp/*.test.mjs
```

Real fixture children run in fresh temporary working directories with explicit
minimal environments. Coverage includes fragmented/multibyte/coalesced frames,
out-of-order replies, independent connections, negotiation validation, unsupported
requests, bounded no-LF records, strict IDs/envelopes, EOF/disconnection, pending
RPC and callback cancellation, deadlines, sanitized failures and owned reaping
(including a TERM-ignoring child). No provider, model prompt, native-agent
acceptance, installation or authentication is exercised.

Protocol references: [initialization](https://agentclientprotocol.com/protocol/v1/initialization.md),
[stdio](https://agentclientprotocol.com/protocol/v1/transports.md),
[schema](https://agentclientprotocol.com/protocol/v1/schema.md),
[security](https://agentclientprotocol.com/protocol/v1/security.md).
