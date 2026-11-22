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

Set `FABRIC_PLUGIN_REGISTRY=/absolute/private/plugins` explicitly in the invoking shell (and in the manager's environment for durable workers) to select installed harnesses with existing `--harness ID` APIs. Native OMP, Codex and fake adapters retain their interfaces. Registry list performs no execution and reports each plugin independently. Check performs startup/health and typed nonbilling planning/evidence. Revocation prevents subsequent resolution; it does not retroactively kill an already-authorized process. Upgrades are explicit replacements with stable ID and kind, not automatic downloads. Executables remain operator-managed; install does not copy them. Compatibility means protocol range and executable presence, not verified behavior.

Example manifest:

```json
{"id":"external-harness","version":"1.0.0","protocol":{"min":1,"max":1},"kind":"harness","executable":"/absolute/plugin","args":[],"capabilities":["harness.run"]}
```

IDs use existing HarnessId syntax and cannot shadow built-ins. Versions are SemVer 2.0.0 strings up to 64 bytes, including prerelease and build identifiers; numeric core/prerelease identifiers reject leading zeroes. Kinds/capabilities are exactly `harness`/`harness.run`, `target`/`target.plan`, `connector`/`connector.evidence`; unknown capabilities and fields fail closed. Manifest input is a bounded regular file, not a symlink, FIFO or device. Unknown protocol ranges can be installed for compatibility reporting but cannot execute. Missing/revoked IDs fail closed. A target can propose local placement only; connector evidence is an availability boolean, never a credential value or authorization.

## Control transport

The host launches literal executable/argv plus `--fabric-plugin-stdio`, with private HOME/XDG, cleared environment, fixed PATH and cwd `/`. Stderr is discarded. JSON-RPC 2.0 over UTF-8 newline-delimited JSON; no batch requests, notifications, plugin-initiated methods or extra response fields. Frames include the LF and are limited to 65,536 bytes. Reads and writes are nonblocking, bounded by one total exchange deadline (default 5 seconds, configurable in the Rust host up to 60 seconds).

Each operation owns one process and sends requests in order:

1. `initialize`, id 1, params `{"protocol":1}`. Result: `{"protocol":1,"id":"external-harness","version":"1.0.0","kind":"harness"}` matching the installed manifest exactly.
2. `health`, id 2, params `{}`. Result: `{"ready":true}`.
3. Operation, id 3: `harness.plan` with `{}` returns `{"ready":true}`; `target.plan` with `{"placement":"local"}` returns `{"placement":"local"}`; `connector.evidence` with `{}` returns `{"available":true}` or false.
4. `shutdown`, id 4, params `{}` returns `{"ready":true}`, then exit successfully.

Responses contain `jsonrpc`, matching numeric `id`, and exactly one of `result` or `error`. Structured errors contain numeric `code` and string `message`; neither remote messages nor OS/path details are reflected in host diagnostics. Host diagnostics use fixed codes (`timeout`, `cancelled`, `remote_error`, `malformed_response`, etc.). Malformed/oversized frames, failed negotiation, hangs, crashes, failed shutdown and cancellation terminate and reap the owned process group. Cancellation is out-of-band process termination, not a cooperative promise. Shutdown shares the operation deadline. There is no global daemon or persistent plugin session.

## Harness migration bridge and authority

After nonbilling startup negotiation and existing capability/profile planning, Fabric launches the same installed executable/argv with `--fabric-plugin-run PROMPT_FILE` in the freshly validated owned workspace. This execution mode emits existing bounded JSON-lines harness events and uses the existing capture, timeout, cancellation, process-group, artifact and durable-worker lifecycle. Execution stdout/stderr are private captured artifacts, **not secret-free output**; do not write secrets there. They are not promoted into plugin error diagnostics or normalized failure messages. Cost limits, model overrides, resume, native-context import and unadvertised features are rejected rather than silently ignored.

Control results cannot specify argv, cwd, environment, paths, reclaim flags or arbitrary methods. Target plans and connector evidence do not provision resources or authorize operations. Only Core performs workspace allocation, launch revalidation, ownership checks, artifact verification and reclamation. No plugin result grants deletion authority. Trusted executables can still make arbitrary system calls as the user: this protocol does not defend against a malicious installed executable directly accessing user files. Revoke and inspect retained evidence if trust is lost.

Provider-free regression command: `cargo test -p fabric-runner --test plugins` plus Core workspace lifecycle tests. Fixtures start external Python processes for all three kinds; no provider service is required.
