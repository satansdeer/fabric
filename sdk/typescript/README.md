# Fabric TypeScript plugin SDK 0.1.0

Node **22.18+** (or 24+) runs erasable TypeScript directly. No Rust toolchain, transpilation, runtime dependencies, provider accounts, or npm install are needed for generated projects. Use standard erasable types, not enums, namespaces, parameter properties, or tsconfig path aliases. The SDK itself ships JavaScript and `.d.mts` contracts, so importing it from `node_modules` does not rely on Node stripping dependency TypeScript.

This is a protocol v1 adapter and provider-free development kit, not a worker runtime. [Core's plugin protocol](../../docs/PLUGIN_PROTOCOL.md) remains authoritative. Target drivers negotiate placement and issue restricted lifecycle instructions; connectors verify and normalize incoming events against explicit operator policy. Core owns routing, durable/idempotent worker admission, source/target pins, workspace ownership, budgets, process evidence, capture, artifacts, cancellation and reclamation. The current instruction set supports trusted-local execution, not an acknowledgement-only remote target.

## Scaffold, test, develop

From this checkout, with an explicit project path:

```sh
node sdk/typescript/cli.mjs create harness /absolute/path/my-harness
cd /absolute/path/my-harness
npm test
npm run dev
# One conformance pass instead of watching:
node sdk/cli.mjs dev . --once
```

Use `target` or `connector` instead of `harness` for the other samples. `--id external-name` overrides the destination basename; IDs are lowercase letters, digits and hyphens, excluding Core built-ins. Creation refuses to overwrite an existing path. `dev` uses Node's native `--watch-path` (macOS-first) and re-runs conformance on edits. `test` and `dev` execute trusted local plugin code, **not a sandbox**. The supplied samples make no network/model calls; third-party code can do anything the current user can do.

The scaffold vendors the versioned SDK into `sdk/`, so it works offline without package-manager hooks. Edit `plugin.ts`; keep its declared id/version/kind and `package.json`'s `version`/`fabricPlugin` metadata synchronized. `files` explicitly lists packaged content; dependencies, optional dependencies, bundled dependencies, symlinks and implicit project discovery are unsupported. Vendor reviewed JavaScript dependencies explicitly if needed; imports do not grant any additional authority. No lifecycle script is executed by the SDK CLI.

The runnable templates in `samples/{harness,target,connector}.ts` also accept the protocol's `--fabric-plugin-stdio` flag directly. The scaffolder adjusts their import path for the generated project.

## Typed adapters

```ts
import { defineHarness, defineTarget, defineConnector, serve, TARGET_OPERATIONS, verifyWebhook, normalizeWebhook } from './sdk/index.mjs';

const harness = defineHarness({
  id: 'my-harness', version: '1.0.0',
  plan: () => ({ ready: true }),
  async run({ prompt, emit }) {
    await emit({ type: 'text', text: prompt });
    await emit({ type: 'done' });
  },
});
const target = defineTarget({
  id: 'my-target', version: '1.0.0',
  capabilities: () => ({ placements: ['my-target'], operations: [...TARGET_OPERATIONS], optional: [] }),
  placement: () => ({ backend: 'host.local' }),
  completed: ({ grant }) => ({ received: grant }),
  prepare: ({ grant }) => ({ grant, action: { primitive: 'workspace.fork', strategy: 'copy' } }),
  start: ({ grant }) => ({ grant, action: { primitive: 'process.spawn' } }),
  inspect: ({ grant }) => ({ grant, action: { primitive: 'process.inspect' } }),
  cancel: ({ grant }) => ({ grant, action: { primitive: 'process.terminate' } }),
  collect: ({ grant, actions }) => ({ grant, action: actions[0] }),
  reclaim: ({ grant }) => ({ grant, action: { primitive: 'workspace.reclaim' } }),
});
const connector = defineConnector({
  id: 'my-connector', version: '1.0.0',
  capabilities: () => ({ version: 1, optional: [] }),
  verify: verifyWebhook,
  normalize: normalizeWebhook,
});
// Each executable serves one kind, not all three:
await serve(harness);
```

For a conventional npm dependency, import from `@fabric/plugin-sdk`. Do not run TS entrypoints inside `node_modules`: Node does not strip dependency TypeScript. Installation below copies the reviewed package into the explicit owner-managed registry's content store, outside `node_modules`.

`serve` maps to the existing ordered initialize/health/typed-operation/shutdown exchange and the harness execution bridge. Do not write logs to stdout during control exchanges. Callback failures return fixed errors; raw exception text is never reflected. Harness events are captured execution artifacts and must not contain secrets.

## Provider-free conformance fixtures

`fabric-plugin test DIR` starts actual child processes with cleared environment, a private temporary HOME/XDG directory, cwd `/`, bounded UTF-8 JSON-lines frames and a five-second deadline per process group. It performs:

| Kind | Mocked request/payload | Expected response |
| --- | --- | --- |
| harness | `harness.plan {}` | `{ "ready": true }` |
| target | Capabilities, placement, then six `target.OP {placement,run_id,grant,actions,context}` exchanges and concrete-plan completion receipts | Exact capabilities, offered backend/action and matching grant; fake remote refuses explicitly. Completion evidence here is fixture data, not execution proof. |
| connector | Capabilities, HMAC-authenticated `connector.verify`, then `connector.normalize` with the identical versioned input | Exact capabilities, `verified: true`, and the complete expected normalized trigger |
| harness run | `--fabric-plugin-run PROMPT_FILE`, fixed fixture prompt | sample text event containing the prompt, then `done` |

All kinds negotiate protocol/id/version/kind and complete health/shutdown. Harness execution runs in a second actual process. The kit bounds frames to 65,536 bytes including LF, and its harness fixture capture to 1,024 events / 1 MiB. Invalid/malformed/oversized/truncated frames, remote errors and timeouts fail with fixed secret-free diagnostics and terminate the process group. Connector fixtures exercise authentication and normalization, not worker scheduling. Third-party callback correctness, provider-backed behavior and Core artifact/lifecycle acceptance remain outside fixture proof.

Target conformance accepts the negotiated `environment.v1` extension advertised by the native local reference package; its six-operation fixture is not environment lifecycle proof. Core's environment integration checks provide that proof. Generated TypeScript targets and `serve(defineTarget(...))` still support only `optional: []`; advertising an unimplemented environment adapter remains rejected. Unknown, mixed or duplicate optional target capabilities fail closed.

SDK metadata must be exactly `sdk: "0.1.0", protocol: 1`; mismatch diagnostics explain what to regenerate/update **before plugin code runs**. Runtime identity/protocol mismatches fail negotiation before Core admits execution. Update package pins only after reviewing the new content.

### Incoming connector fixtures and webhook

The generated connector accepts `{event_id,idempotency_key,data}` JSON bodies with HMAC-SHA256 or a scoped bearer-file reference. It advertises no subscribe/reply operations. Another format implements the same typed `verify`/`normalize` callbacks; Core does not gain provider branches.

`fabric-plugin test DIR --connector-fixture FILE` tests an alternate format without rebuilding Core. The JSON fixture is `{body: STRING, expected: NormalizedTrigger, authorization?: STRING}`: a raw body string, the complete version-1 normalized contract and optional format-specific signature (at most 512 characters). The kit supplies a private deterministic HMAC key reference (32 bytes of value 7), trusted policy identities from `expected`, fixture clock/timestamp 1000 and expiry 1060. Without `authorization` it generates the unchanged generic signature; either path checks the exact normalized result. Fixtures are local test data, not automation approval or provider authentication.

See [incoming connector contract](../../docs/PLUGIN_PROTOCOL.md#incoming-connectors-contract-version-1) for explicit private automation files, source/tenant/scope/route checks, signing bytes, durable acknowledgements/replay/rate limits and the stdlib-only loopback HTTP reference. Raw event fields are data, never unrestricted worker instructions. The existing content-pinned external installation below applies unchanged.

The [GitHub issues connector](../../docs/GITHUB_CONNECTOR.md) is an externally packaged example using these callbacks and optional outbound hooks, with provider-free end-to-end and opt-in real-installation checks. It preserves the generic connector and requires separately approved local Automation execution.


## Reproducible local packaging (not public publication)

Prepare the SDK tarball:

```sh
cd sdk/typescript
npm ci --ignore-scripts
npm test
npm pack --ignore-scripts --pack-destination /absolute/output/directory
```

The resulting `fabric-plugin-sdk-0.1.0.tgz` can be unpacked into a reviewed local tooling directory (`tar -xzf ...`); run `node /that/directory/package/cli.mjs create target /absolute/new-project`. It can also be installed using `npm install --ignore-scripts /absolute/path/fabric-plugin-sdk-0.1.0.tgz`. The package supplies the `fabric-plugin` executable: these are the adjusted names for `fabric plugin create/dev/test`, not new Core subcommands. No public npm publication is claimed.

Package a generated plugin without invoking npm at all:

```sh
node sdk/cli.mjs hash .
node sdk/cli.mjs pack . --out /absolute/output/my-plugin-1.0.0.tgz
# npm tarballs are also supported, with hooks explicitly disabled:
npm pack --ignore-scripts
```

`hash` outputs `{id,version,sha256}`. The pin is a **content-tree SHA-256**, not the gzip file checksum: for each regular file sorted by ASCII relative path, hash `path + NUL + lowercase_hex(SHA256(file_bytes)) + LF`; SHA-256 those records. It pins `package.json`, entrypoint, vendored SDK and every shipped file. Therefore directory, deterministic SDK tarball and npm tarball share a pin despite tar metadata differences. `pack` normalizes tar metadata for reproducible bytes.

Input limits: 32 MiB total regular contents, 256 files, 512 entries, 16 directory levels; relative ASCII path components only. Archives are bounded gzip/ustar `package/` trees with checksum validation, no duplicate entries, links, devices, absolute/traversal paths, `node_modules` or `.git` content, or extended/PAX metadata. The deterministic packer's path ceiling is 92 bytes (100 including `package/`); shorten unusually long paths instead of relying on tar extensions. Archives exceeding the expanded limit are rejected without extraction.

## Explicit pinned trusted-local installation

Installation requires an already-installed compatible Fabric binary. It is the only flow here that needs Core; authoring, packaging and conformance do not require Rust. Prepare a canonical absolute registry directory outside **every** Git checkout, owned by you with permissions 0700. Core's existing `fabric plugins` command remains the sole registry writer.

After reviewing the code, version and `hash` output:

```sh
node sdk/cli.mjs install /absolute/path/my-plugin-1.0.0.tgz \
  --registry /canonical/owner/private/fabric-plugins \
  --fabric /canonical/path/to/fabric \
  --version 1.0.0 --sha256 REVIEWED_CONTENT_SHA256 --trust
/canonical/path/to/fabric plugins --registry /canonical/owner/private/fabric-plugins check my-harness
```

A local package directory is accepted instead of the tarball. No remote package names, registry lookup, dependency resolution, install hooks, automatic trust, project scanning or project-local registry are supported. The CLI always warns that installed code has the current user's privileges and requires explicit `--trust`; both exact SemVer and content hash must match before any install operation. Installation itself never executes plugin code. Use explicit `--upgrade` for an existing id; Core's kind/built-in/revoke rules remain unchanged.

Reviewed content is copied atomically to `REGISTRY/packages/SHA256`, then Core installs a manifest with the absolute Node executable, explicit entrypoint argv and an integrity pin. Core rechecks that pin during install, before every control child launch and before harness execution. Modified/added/deleted content or a symlink fails `integrity_mismatch` without reflecting file paths or bytes. Legacy protocol manifests without `integrity` retain their existing trusted-local behavior. Keep registry mutations serialized; the existing registry does not implement multi-writer transactions. Copied content may remain after a failed registry operation (Core might already have committed); it is never unsafely rolled back. Revocation leaves pinned content for owner inspection, not automatic deletion.

This is **not a sandbox or code-signing system**. The Node binary and system imports are trusted; pins do not defend against a malicious trusted user racing filesystem changes between validation and execution, or code deliberately reading outside its package. Changing the Core protocol/integrity extension requires a compatible Core release once; installing subsequent TypeScript plugins does not rebuild it.

## Maintainer checks

`npm ci --ignore-scripts && npm test` checks typed contracts, compiles generated projects, runs all three real stdio kinds, malformed/version/pin/archive boundaries, npm-packaged scaffolding and macOS watch mode without providers. `FABRIC_PLUGIN_TEST_HOST=/absolute/fabric npm test` additionally exercises actual Core registry installation, check, tamper rejection, upgrade and revoke for all kinds. That test explicitly skips when no host is supplied or TMPDIR is inside a Git checkout; CI supplies a built host and an external temporary directory. In restricted workers, `FABRIC_PLUGIN_TEST_NODE=/absolute/node cargo test -p fabric-runner --test plugins typescript_scaffolds_use_existing_native_host` proves generated SDK processes through the actual host/admission boundary without writing an on-disk registry inside the checkout.

## Reference local target

`cargo build --locked -p fabric-cli --bins` builds `fabric-local-target` beside `fabric`. Downloaded releases include both siblings and the installable local reference package; no source build is required by an operator. Without a registry override, the sibling is the reference driver. Explicit installation/upgrade/revocation uses the same registry as other plugins:

```sh
node sdk/typescript/cli.mjs reference local /absolute/new/local-package \
  --binary /absolute/path/to/fabric-local-target
node sdk/typescript/cli.mjs test /absolute/new/local-package
node sdk/typescript/cli.mjs hash /absolute/new/local-package
node sdk/typescript/cli.mjs install /absolute/new/local-package \
  --registry /canonical/owner/private/fabric-plugins --fabric /absolute/path/to/fabric \
  --version 0.1.0 --sha256 REVIEWED_CONTENT_SHA256 --trust
```

Targets select concrete operation-keyed host primitives, never supply success, paths, commands or deletion authority. Core executes the selection against validated objects and sends independently observed `target.completed` evidence; the target returns `{received:grant}`. See [the full operation contract](../../docs/PLUGIN_PROTOCOL.md#installed-execution-targets). Generated and sample targets offer usable `host.local` plans under any installed ID; fake-remote refusal is a separate negative fixture. Conformance requires concrete offered plans and receipts for all six operations, but transport conformance alone is not execution proof. The generated-target Core integration executes a worker, verifies artifact hashes and reclaims. Session-lifetime grants cannot replay completed or unsupported callbacks. Set `FABRIC_LOCAL_TARGET_BINARY=/absolute/path/to/fabric-local-target` for reference-package checks. No cloud claim, fallback or legacy acknowledgment shim.


## Native reference harness plugins

OMP and Codex are separate SDK executables, not Core built-ins. Build the release executable explicitly; the default Cargo members build only Fabric. Release binaries also fit the SDK's 32 MiB package limit.

```sh
cargo build --locked -p fabric-cli --bins
cargo build --locked --release -p fabric-reference-harnesses --bin fabric-reference-harnesses
```

Choose existing canonical, owner-private package and registry directories outside every Git checkout. The package destination must not already exist. Package OMP and run provider-free conformance, then explicitly install the reviewed package. Before the final Core check, install the supported native CLI version listed below; that check executes its version probe without a model request.

```sh
PACKAGES=/canonical/owner/private/plugin-packages
REGISTRY=/canonical/owner/private/fabric-plugins
FABRIC=/canonical/path/to/fabric
node sdk/typescript/cli.mjs reference omp "$PACKAGES/omp" \
  --binary "$PWD/target/release/fabric-reference-harnesses"
node sdk/typescript/cli.mjs test "$PACKAGES/omp"
node sdk/typescript/cli.mjs hash "$PACKAGES/omp"
# Review the content and preceding hash before granting trust:
node sdk/typescript/cli.mjs install "$PACKAGES/omp" \
  --registry "$REGISTRY" --fabric "$FABRIC" \
  --version 0.1.0 --sha256 REVIEWED_CONTENT_SHA256 --trust
export FABRIC_PLUGIN_REGISTRY="$REGISTRY"
"$FABRIC" plugins --registry "$REGISTRY" check omp
```

Repeat with `codex` and its separately reviewed package hash when needed. The plugins retain native CLI IDs and pinned versions: `omp/19.1.0` and `codex-cli 0.171.0`. Install/configure the actual CLI separately before native discovery or consent; packaging and conformance use local doubles and do not authenticate or spend provider funds.

Set `FABRIC_PLUGIN_REGISTRY` in both controller and manager environments. Drain and stop an old manager before changing its binary or environment; follow the main README's upgrade procedure. Then use `fabric init --harness omp --check` and explicit `harness connect` as documented there. Missing, revoked or tampered installs fail closed without a built-in vendor fallback.

Native adapter authors use `defineNativeHarness` and the [native control contract](../../docs/PLUGIN_PROTOCOL.md); ordinary `defineHarness` samples remain provider-free executable harnesses.

For the complete local maintainer checks, build both executables above, then supply their canonical paths:

```sh
export FABRIC_REFERENCE_PLUGIN_BINARY="$PWD/target/release/fabric-reference-harnesses"
export FABRIC_PLUGIN_TEST_HOST="$PWD/target/debug/fabric"
export FABRIC_PLUGIN_TEST_NODE="$(command -v node)"
RUST_TEST_THREADS=4 cargo test --offline --locked --workspace
(cd sdk/typescript && npm ci --ignore-scripts && npm test)
```

Without `FABRIC_REFERENCE_PLUGIN_BINARY`, native SDK conformance is explicitly skipped. CI supplies it and builds reference plugins before workspace tests; neither these checks nor plugin installation imply live provider acceptance.
