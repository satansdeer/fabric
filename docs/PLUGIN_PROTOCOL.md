# Plugin protocol v1

External plugins are explicitly installed trusted-local executables, not sandboxed code. Installing a plugin authorizes code execution as the operator. Do not install code you do not trust. Fabric does not discover or execute project manifests. No marketplace, dynamic library loading, credential inheritance, or remote execution is provided.

## Operator lifecycle

Create a mode-0700 directory outside all Git checkouts. Use its canonical absolute path. The JSON registry is owner-only, symlinks are rejected, writes use atomic replacement. Serialize operator changes; concurrent registry writers are not supported.

```
fabric plugins --registry /absolute/private/plugins install /absolute/manifest.json
fabric plugins --registry /absolute/private/plugins list
fabric plugins --registry /absolute/private/plugins check external-harness
fabric plugins --registry /absolute/private/plugins upgrade /absolute/new-manifest.json
fabric plugins --registry /absolute/private/plugins revoke external-harness
```

Set `FABRIC_PLUGIN_REGISTRY=/absolute/private/plugins` explicitly in the invoking shell (and in the manager's environment for durable workers) to select installed harnesses with existing `--harness ID` APIs. OMP and Codex require explicit content-pinned installation; no native vendor is registered or used as a fallback. Only provider-free fake adapters remain built in. Registry list performs no execution and reports each plugin independently. Check performs startup/health and typed nonbilling probing/evidence. Revocation prevents subsequent resolution; it does not retroactively kill an already-authorized process. Upgrades are explicit replacements with stable ID and kind, not automatic downloads. Executables remain operator-managed; raw manifest install does not copy them. Compatibility means protocol range and executable presence, not verified provider behavior.

Example manifest:

```json
{"id":"external-harness","version":"1.0.0","protocol":{"min":1,"max":1},"kind":"harness","executable":"/absolute/plugin","args":[],"capabilities":["harness.run"]}
```

IDs use existing HarnessId syntax and cannot shadow built-ins. Versions are SemVer 2.0.0 strings up to 64 bytes, including prerelease and build identifiers; numeric core/prerelease identifiers reject leading zeroes. Kinds/capabilities are exactly `harness`/`harness.run`, `target`/`target.execute`, `connector`/`connector.evidence`; unknown capabilities and fields fail closed. Manifest input is a bounded regular file, not a symlink, FIFO or device. Unknown protocol ranges can be installed for compatibility reporting but cannot execute. Missing/revoked IDs fail closed. Connector evidence is an availability boolean, never a credential value or authorization.

## Control transport

The host launches literal executable/argv plus `--fabric-plugin-stdio`, with private HOME/XDG, cleared environment, fixed PATH and cwd `/`. Stderr is discarded. JSON-RPC 2.0 over UTF-8 newline-delimited JSON; no batch requests, notifications, plugin-initiated methods or extra response fields. Legacy and target frames include LF and are limited to 65,536 bytes. Reads and writes are nonblocking. Legacy exchanges share one total deadline (default 5 seconds, configurable in the Rust host up to 60 seconds). Persistent target exchanges use a deadline per request; opt-in native exchanges also use per-request deadlines and 131,072-byte frames.

Legacy harness/connector operations own one process and send requests in order:

1. `initialize`, id 1, params `{"protocol":1}`. Result: `{"protocol":1,"id":"external-harness","version":"1.0.0","kind":"harness"}` matching the installed manifest exactly.
2. `health`, id 2, params `{}`. Result: `{"ready":true}`.
3. Operation, id 3: `harness.plan` with `{}` returns `{"ready":true}`; `connector.evidence` with `{}` returns `{"available":true}` or false.
4. `shutdown`, id 4, params `{}` returns `{"ready":true}`, then exit successfully.

Responses contain `jsonrpc`, matching numeric `id`, and exactly one of `result` or `error`. Structured errors contain numeric `code` and string `message`; neither remote messages nor OS/path details are reflected in host diagnostics. Host diagnostics use fixed codes (`timeout`, `cancelled`, `remote_error`, `malformed_response`, etc.). Malformed/oversized frames, failed negotiation, hangs, crashes, failed shutdown and cancellation terminate and reap the owned process group. Cancellation is out-of-band process termination, not a cooperative promise. Legacy shutdown shares the operation deadline. There is no global daemon; native normalization and target lifecycle each reuse supervised sessions with monotonically increasing request IDs.

## Harness migration bridge and authority

After nonbilling startup negotiation and existing capability/profile planning, Fabric launches the same installed executable/argv with `--fabric-plugin-run PROMPT_FILE` in the freshly validated owned workspace. This execution mode emits existing bounded JSON-lines harness events and uses the existing capture, timeout, cancellation, process-group, artifact and durable-worker lifecycle. Execution stdout/stderr are private captured artifacts, **not secret-free output**; do not write secrets there. They are not promoted into plugin error diagnostics or normalized failure messages. Cost limits, model overrides, resume, native-context import and unadvertised features are rejected rather than silently ignored.

Legacy control results cannot specify argv, cwd, environment, paths, reclaim flags or arbitrary methods. The opt-in native extension below admits bounded argv and scoped environment proposals, never cwd or ownership/deletion authority. Target instructions use the restricted host primitives below; connector evidence does not authorize operations. Only Core performs workspace allocation, launch revalidation, ownership checks, artifact verification and reclamation. No plugin result grants deletion authority. Trusted executables can still make arbitrary system calls as the user: this protocol does not defend against a malicious installed executable directly accessing user files. Revoke and inspect retained evidence if trust is lost.

Provider-free regression command: `cargo test -p fabric-runner --test plugins` plus Core workspace lifecycle tests. Fixtures start external Python processes for all three kinds; no provider service is required.

## Installed execution targets

`target.plan` and operation-token acknowledgments are removed, not compatibility aliases. After initialize/health, `target.capabilities {}` returns exactly `{"placements":["PLUGIN_ID"],"operations":["prepare","start","inspect","cancel","collect","reclaim"],"optional":[]}`. Optional PTY, persistent environments and infrastructure-evidence claims are rejected. Before admission, `target.placement {"placement":"PLUGIN_ID","backends":["host.local"]}` selects `{"backend":"host.local"}` or refuses with `{"backend":"unsupported"}`. Any installed target ID may select the offered backend; this is not a literal-`local` allowlist.

Target initialization, health and capability exchanges retain the five-second
per-request startup bound. After negotiation, lifecycle operations and shutdown
are limited to 250 ms each so a blocked driver cannot stall Core supervision
for the startup timeout. Any overrun fails closed, terminates owned processes
and retains incomplete cleanup evidence; there is no retry or local fallback.

Each `target.OP` receives `{placement,run_id,grant,actions,context}`. The opaque grant is scoped to that request and the offered actions; context contains bounded nonsecret inputs, not authority. The exact reply is `{grant,action}` selecting one offered action, or `{grant,action:{primitive:"unsupported"}}`. Unknown fields, stale grants, unoffered actions, paths, commands, PIDs and plugin-supplied outcomes fail closed. Core executes the selected primitive against captured validated objects inside this boundary, not after an acknowledgment. A separate target can, for example, select `copy` rather than `clone_or_copy` for preparation without rebuilding Core.

| Operation | Offered concrete primitive |
| --- | --- |
| prepare | `workspace.fork`, strategy `clone_or_copy` or `copy` |
| start | `process.spawn` |
| inspect | `process.inspect` (live owned group or recovery ownership evidence) |
| cancel | `process.terminate` |
| collect | `artifacts.capture` or recovery `artifacts.recover` |
| reclaim | `workspace.reclaim` |

After executing a primitive Core sends `target.completed {grant,evidence}` with observed facts; the exact receipt is `{received:grant}`. Receipts do not create successful outcomes. Actual Rust workspace/process/capture results, independently committed pins, status and artifact hashes remain authoritative. Start revalidates workspace, consent, command, storage and cancellation before dispatch and again immediately before spawn. Cancellation drives process-group termination; mandatory host stop/reap remains in force even on driver failure. Reclaim is offered only after apply, sealed hashes, owner and lease checks and durable cleanup intent; deletion remains descriptor-anchored. Dry-run never dispatches reclaim. Shutdown must exit successfully before successful finalization.

A separately installed fake remote refuses placement before allocating runs/workspaces; no local fallback occurs. Only the trusted-local host primitive backend is implemented. Actual remote execution requires a separately specified verifiable backend, not acceptance of remote status assertions.

`local` resolves through the registry like any other target, or to the distributed sibling `fabric-local-target` when no local registry entry exists. A revoked/broken installed local entry is an error, not permission to use the bundled executable. Admission pins manifest, executable SHA-256, placement and selected registry into the durable accepted plan and immutable schema-14 `run_targets` row before preparation. Execution/recovery never silently repin after upgrades; missing/revoked/changed plugins refuse finalization/reclaim and retain unknown cleanup evidence. Existing pre-target workspace records retain their original evidence-based recovery rules.

The local executable and its installable SDK reference package ship with release tooling. Replacing the installed driver does not rebuild Core. This is a trusted local host-primitive interface, not remote execution, a sandbox, secrets relocation, PTY or persistent-environment support.


## TypeScript SDK and pinned package installation

[`@fabric/plugin-sdk` 0.1.0](../sdk/typescript/README.md) supplies `defineHarness`, `defineTarget`, `defineConnector`, runnable samples and provider-free `fabric-plugin create/dev/test` commands. Generated projects use Node 22.18+ erasable TypeScript directly; no Rust toolchain, npm install, provider service or Core rebuild is needed. SDK/protocol metadata is checked before local conformance or installation executes any plugin code. The JSON-RPC v1 fields and ordered exchange above are unchanged.

The SDK installer accepts only an explicit local package directory or bounded gzip/ustar npm tarball, exact SemVer, content-tree SHA-256, `--trust`, absolute Fabric binary and owner-managed registry path. It warns that plugin code has the user's privileges, never invokes npm hooks or discovers project plugins, copies reviewed content to `REGISTRY/packages/SHA256`, and invokes the existing `fabric plugins install/upgrade` command. Only Core writes `plugins.json`; existing ownership, no-project-registry, permission, built-in-id, kind, revocation and lifecycle checks remain authoritative.

Manifests optionally carry `"integrity":{"root":"/canonical/private/package","sha256":"64-lowercase-hex"}`. This additive manifest extension requires a Core version supporting package integrity; older hosts fail closed on the unknown field. Legacy manifests without it keep their existing trusted-local behavior. For pinned manifests, Core checks a canonical owner-only root, bounded regular-file tree, and hash at installation, every control launch, and harness execution revalidation. Fixed `integrity_mismatch` diagnostics do not reflect paths, contents or remote errors.

The hash is SHA-256 of concatenated `relative_ASCII_path + NUL + lowercase_hex(SHA256(file_bytes)) + LF` records sorted by relative path. Limits: 256 regular files, 512 entries, 16 directory levels, 32 MiB aggregate file bytes; symlinks and special files are rejected. The SDK copies only explicitly shipped regular files, not tar metadata or excluded development files. Directory and npm tarball pins therefore match. A pin detects changed content; it does not sandbox trusted code, pin the Node/system runtime, or prevent a trusted local user racing filesystem changes. Registry writes remain serialized, as in the existing owner-managed interface.

Provider-free SDK checks: `cd sdk/typescript && npm ci --ignore-scripts && npm test`. Set `FABRIC_PLUGIN_TEST_HOST=/absolute/fabric` to additionally exercise all three kinds through packaged install, native check, tamper rejection, upgrade and revoke in an outside-checkout temporary registry. Set `FABRIC_PLUGIN_TEST_NODE=/absolute/node` for the Rust `typescript_scaffolds_use_existing_native_host` test when an on-disk registry cannot be created outside the checkout. Neither check authorizes model-backed execution or public npm publication.

## Full native harness extension

Content-pinned harness manifests may add `native` metadata: `executable` (the CLI basename), `root_environment` (bounded native directory variable names), and `fingerprint: {domain, files, roots}`. `roots` is an ordered list of `[profile_reference_kind, relative_suffix]`; Core selects the first matching authorized root and fingerprints the declared bounded regular config files, including absence. This declaration is part of the installed pin, not a runtime reply. OMP declares its seven non-auth configuration files; Codex declares `config.toml`. Authentication stores are never fingerprinted.

After initialize/health, native operations return `{result: VALUE, failure: null}` or `{result: null, failure: TYPED_ERROR_KIND}`. Only documented domain rejection kinds are accepted; messages are fixed by Core. The TypeScript SDK exports matching `Native*` types and `defineNativeHarness`:

| Method | Params | Result |
| --- | --- | --- |
| `harness.probe` | `{context}` | `{version}`: exact native CLI version |
| `harness.native_context` | `{context}` | `{profile, credential_paths, credential_available, exposure}` |
| `harness.capabilities` | `{version}` | complete typed capability manifest |
| `harness.plan` | `{request, version, context}` | `{plan, supports_cost_limit}` |
| `harness.command` | `{request, profile, prompt_file, context}` | `{program, args, environment}` |
| `harness.normalize` | `{event: JSON}` or `{text: STRING}` | `{event: typed_event_or_null}` |

`context` carries the discovered CLI path, bounded runtime PATH, explicitly requested native root paths for discovery, and selected credential **availability booleans only**. No environment secret values are sent to control children. Command credential proposals use the literal `fabric-credential-available`; only Core resolves selected profile connections into the final worker environment. Native HOME/directory proposals must remain within selected profile roots; unsolicited environment keys, loader/XDG overrides, duplicate keys, another executable and invalid argv are rejected. Exactly one `{prompt:true}` argument is replaced with Core's literal UTF-8 prompt, avoiding large prompt control frames and shell interpretation.

Core validates capability identity/version/completeness, retains Fabric-managed cancellation/workspace/patch capture, negotiates requests, computes fingerprints and evidence digests, compares effective requests and revalidates at launch. Native-context paths must be within discovery grants, and credential paths within the proposed native roots. Existing onboarding compares metadata and persists revocable explicit local consent; plugins do not grant consent.

Normalization is streamed through a persistent bounded control process, not one process per event. Typed output/tool/usage/failure events are validated; unknown fields and plugin-supplied started/completed lifecycle events fail closed. The host alone emits lifecycle events. A normalization failure produces a fixed typed failure, marks the capture unsuccessful and continues draining raw output for evidence. The execution deadline also bounds normalizer initialization and every event RPC; buffered frames cannot extend the run indefinitely. Deadline expiry stops normalization, preserves raw draining and reports a timeout even if the native command has already exited. Cancellation interrupts the active normalizer and kills/reaps its process group without recording a synthetic plugin failure. Deadline expiry never cancels sibling workers. Worker launch, limits, storage, artifact hashes/sealing and reclaim remain Core-owned.

OMP/Codex implementations and their original provider-free fixtures live in `sdk/rust/reference-harnesses`. Build/package/install instructions are in the [SDK guide](../sdk/typescript/README.md#native-reference-harness-plugins). Controller and worker harness selections remain independent; installation never selects a harness automatically. Legacy harness readiness/run and connector contracts remain supported; targets use `target.execute`. Provider-free fixtures do not establish provider authentication, model availability or model-backed execution.
