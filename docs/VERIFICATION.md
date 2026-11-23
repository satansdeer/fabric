# Corrective milestone verification

## Evidence labels and release gates

`IMPLEMENTED` means code exists; `TESTED_WITH_FAKE` means deterministic provider-free lifecycle proof; `LIVE_VERIFIED` means an actual provider-backed harness run with recorded evidence; `NOT_VERIFIED` and `BLOCKED` are not passes.

| Gate | Status | Evidence |
|---|---|---|
| LOCAL_WORKER_API_READY | TESTED_WITH_FAKE | Deterministic lifecycle and recovery checks; current issue-5 candidate proof below. |
| LIVE_HARNESS_VERIFIED | LIVE_VERIFIED, scoped | Published v0.1.1 ran the native OMP issue-5 correction worker to SUCCEEDED with sealed artifacts and independently verified reclamation. This does not prove full skill parity. |
| PSTACK_LOCAL_SWARM_READY | TESTED_WITH_FAKE, scoped | [Accepted consumer overlap evidence](https://github.com/satansdeer/om-pstack/issues/2#issuecomment-7090364128): native controller plus four deterministic fake workers, overlapping RUNNING intervals and cancellation. Not four reasoning workers or full swarm parity. |

No cloud readiness or full model-driven skill-routing claim is made. Later sections preserve historical verification records; their counts and blockers are not current release gates.

## Issue 5: installed trusted-local target correction

Immutable source baseline: `52a086a1ed59958629f17b4d6e53484504e15932`
(unaccepted review checkpoint, not released code). Reviewed issue-body SHA-256:
`c637767c4dd99297177eb515357e6da088c7b91922d41c75d2b4667181d2f7e9`.
Manager plan: <https://github.com/satansdeer/fabric/issues/5#issuecomment-7089448737>.
The executing worker runtime is independently managed public Fabric v0.1.1;
it is not evidence that this source candidate has shipped.

Installed targets select concrete, request-scoped host primitives, not lifecycle
acknowledgments. Placement negotiates `host.local` for any admitted installed ID.
Each operation receives an opaque grant and bounded offered actions; only an exact
granted action executes against captured validated Rust objects. Preparation can
select clone-or-copy or copy; start spawns the supervised process; inspection
observes it; cancellation signals it; collection captures/reconstructs artifacts;
reclaim deletes through the validated open workspace descriptor.
`target.completed` carries independently observed evidence, never plugin-authored
exit status, ownership, paths, argv, seals or cleanup proof. No local fallback
follows a refused, crashed, unavailable or malformed target.

Core retains pins, admission, leases, launch checks, ownership, artifact
sealing/hash checks and durable cleanup state. Start revalidates cancellation and
authority before dispatch and again before spawn. GC dispatches reclaim only
after apply, validated ownership/lease and durable cleanup intent; dry-run
dispatches none. Mandatory process-tree stop/reap remains host-owned even if
target communication fails.

Consumer-visible regressions exercise a separately installed non-`local` target
selecting copy, executing a worker, restoring verified artifacts and reclaiming;
fake-remote refusal without allocation/launch; crash/forged/hung target retention;
recovery with an unavailable target; pre-dispatch and pre-spawn authority loss;
acknowledgment-only/wrong-grant refusal; and dry-run/owner-mismatch non-dispatch.
SDK conformance accepts concrete third-party plans but is only transport proof;
the actual installed CLI worker tests supply execution evidence.

The implementation worker reported focused Rust, formatting, strict lint and SDK
checks. Manager-independent verification on macOS arm64 with Rust 1.91.0:

```sh
cargo build --locked -p fabric-cli -p fabric-reference-harnesses --bins
export FABRIC_REFERENCE_PLUGIN_BINARY="$CARGO_TARGET_DIR/debug/fabric-reference-harnesses"
export FABRIC_LOCAL_TARGET_BINARY="$CARGO_TARGET_DIR/debug/fabric-local-target"
export FABRIC_PLUGIN_TEST_HOST="$CARGO_TARGET_DIR/debug/fabric"
export FABRIC_PLUGIN_TEST_NODE=/opt/homebrew/bin/node RUST_TEST_THREADS=4
cargo test --locked -p fabric-cli --test run installed_
cargo test --locked -p fabric-cli --test run \
  crashed_or_forging_target_retains_unknown_cleanup_and_cannot_succeed -- --exact
cargo test --locked -p fabric-runner --test plugins target_
cargo test --locked -p fabric-core --lib \
  dry_run_and_owner_mismatch_never_dispatch_target_reclaim
cd sdk/typescript
npm ci --ignore-scripts
npm test
```

Build passed; all six selected Rust regressions and all 17 SDK tests passed.
The SDK command includes TypeScript contract checking and actual Core integration.
The manager used an explicit shared `CARGO_TARGET_DIR`; binaries were built from
this candidate, not represented as a published release.

Release tooling bundles the concrete `fabric-local-target` executable and pinned
installable package; CI supplies its path to SDK runtime checks. Candidate release
archive/download proof and exact-head CI remain required before publication.
No cloud, PTY, persistent-environment, infrastructure-security or isolation claim
is made.

## Issue 42: native capture drain accounting

Source baseline: `ebaad083626f8d9b6fcef35d9ffaaabe29d357f7`.
Approved issue-body SHA-256:
`cdfbcaf43ae399bbd34dfa4ccdb3d305590804e25de4eb2bb3424f2bc21fd620`.
The reported reproduction is [merged-main macOS acceptance run 45097474252](https://github.com/satansdeer/fabric/actions/runs/45097474252):
`native_capture_respects_the_execution_deadline_and_preserves_cancellation`
returned `Capture pipe remained open after group termination; capture incomplete.`
That failure was not rerun merely to reproduce it.

Source trace: runs, version probes and Git artifact capture share `execute` and
`PipeReader`. Execution terminates/reaps its owned group, sets `stopped`, then
joins both drains. Stdout decodes buffered frames and synchronously normalizes
them through a separately owned persistent `PluginSession`. Normalization RPCs
observe the execution-local deadline/cancellation token; a failed RPC drops and
reaps that session, while adapter drop finishes any retained session.

The old `PipeReader::stopped_at` starts on the first read observing `stopped`.
Time spent normalizing **between** subsequent reads then consumes its 250 ms
budget. A read after that pause fails before attempting to read buffered pipe
bytes or EOF, even with no writer left. This source-proven false-timeout path
depends on whether the first read observes group termination; the CI log does
not record the exact thread interleaving. It is not evidence of a leaked writer.

The shared reader now accounts cumulative time **inside** post-stop reads,
excluding consumer processing. The same 250 ms drain budget still bounds waiting
on an open writer and continuously readable output. Execution/RPC deadlines,
TERM/KILL, owned PID anchoring, reaping and incomplete-capture errors are unchanged.
The deterministic pipe regression closes its writer, reads one byte after stop,
pauses beyond the old budget, then requires the remaining byte and real EOF.
The existing native regression retains both slow/deadline and hanging/cancellation
branches, and additionally checks real OS status, all six decoded frames and
normalizer reaping before adapter destruction.

Worker verification on macOS arm64, each command attempted once:

```sh
PATH="/Users/maksimivanov/.rustup/toolchains/1.91.0-aarch64-apple-darwin/bin:$PATH" FABRIC_REFERENCE_PLUGIN_BINARY=/Users/maksimivanov/.local/share/fabric/releases/v0.1.0/install/fabric-v0.1.0-macos-arm64/plugins/omp/plugin cargo test --offline --locked -p fabric-runner --lib --test plugins capture -- --nocapture
PATH="/Users/maksimivanov/.rustup/toolchains/1.91.0-aarch64-apple-darwin/bin:$PATH" cargo fmt --all -- --check
```

- Regression command: exit 101 **before compilation**, `no matching package named
  serde_json found` in the offline crates.io index, required by `fabric-domain`.
  No tests executed; no retry or dependency/configuration change was made.
- Format check: exit 0, no output.
- Rust LSP references were attempted but unavailable: the installed rustup shim
  could not launch rust-analyzer, and the supplied toolchain has no rust-analyzer
  binary. Callers were traced through source searches/reads instead; no exported
  API changed.

Worker status was **IMPLEMENTED, behavior NOT_VERIFIED** because its private
offline dependency cache prevented compilation.

Manager verification on macOS arm64 used the recovered candidate, the existing
host dependency cache and shared build output. The cancellation status assertion
accepts either owned TERM or grace-period KILL, rather than assuming scheduling
always permits graceful termination. LSP resolved both production stdout/stderr
`PipeReader` callers; no exported API changed.

```sh
export PATH="/Users/maksimivanov/.rustup/toolchains/1.91.0-aarch64-apple-darwin/bin:$PATH" CARGO_TARGET_DIR=/Users/maksimivanov/.local/share/fabric/reviews/issue-38-source/target FABRIC_REFERENCE_PLUGIN_BINARY=/Users/maksimivanov/.local/share/fabric/releases/v0.1.0/install/fabric-v0.1.0-macos-arm64/plugins/omp/plugin
cargo test --locked -p fabric-runner --lib --test plugins capture -- --nocapture
cargo fmt --all -- --check
```

Both commands passed once. Regression result: **8 passed**, two suites, 16
filtered out; tests completed in 2.43 seconds. This covers buffered bytes/EOF
after delayed consumption, held-open writer failure, and native
deadline/cancellation/frame/reaping paths. Status: **TESTED_WITH_FAKE** for this
capture path. No full local suite, lint, release package, live-provider capture
proof or actual-route timing proof was run.
This candidate does not diagnose the unrelated issue #38 storage-meter failure.
Published runtime remains v0.1.0; review, exact-head and merged-main acceptance,
v0.1.1 publication and independent downloaded-release smoke remain manager-owned.

## Issue 39: content-free storage meter diagnostics

Source baseline verified before editing: `a8448f5a04dee05ffda801157655685e6eb01cf9`.
Reviewed issue-body SHA-256 supplied with the task:
`96f260f6bf75f69851f2aad3410d53781525aef1fedc239f28359484a11fdae0`.

`TESTED_WITH_FAKE`: the candidate carries typed measurement stage/cause codes
through storage supervision, durable observations and terminal reports, including
report reconstruction from observations. No database schema migration, cap
increase, retry, storage-error suppression or stop-category change was made.
Legacy observations/results without the optional field remain readable.

Exact focused command on macOS arm64 (source-built candidate CLI; existing
released v0.1.0 reference plugin used only for fixture discovery/planning):

```sh
PATH="/Users/maksimivanov/.rustup/toolchains/1.91.0-aarch64-apple-darwin/bin:$PATH" FABRIC_REFERENCE_PLUGIN_BINARY=/Users/maksimivanov/.local/share/fabric/releases/v0.1.0/install/fabric-v0.1.0-macos-arm64/plugins/omp/plugin cargo test --locked -p fabric-cli --test run storage_meter_failure_survives_terminal_reconnect_without_raw_content -- --exact --nocapture
```

First attempt: `0 passed; 1 failed; 47 filtered out`; the read-only viewer rejected
the fixture's noncanonical macOS temporary-path alias (`Log root must be canonical.`).
Only the test viewer was corrected to pass the canonical root. Same focused
command rerun:

```text
test storage_meter_failure_survives_terminal_reconnect_without_raw_content ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 47 filtered out; finished in 6.77s
```

The single regression injects a free-space `statvfs`/`EIO` failure after a fake
shell worker starts. It checks that zero exit remains `FAILED` /
`storage_meter_failed`, artifacts are captured, the fork is reclaimed, admission
capacity is released, and a new read-only CLI process reconnects to terminal
observations containing `{"stage":"free_space","code":"io","operation":"statvfs","os_code":5}`.
Credential-path, file-content and native-log sentinels are absent; raw output is
not requested. No manager, native/model worker or failed-worker replay was started.

Limitations: only this regression was run; no full suite, lint/format pass,
manager-restart scenario or scan-bound/race injection was run. This is candidate
source proof, **not released-runtime verification**. The historical issue #38
meter failure's actual cause remains **UNKNOWN**, not proven storage pressure,
a scan bound or a race. Verification with the next released runtime and inspection
of its new evidence remain manager-owned prerequisites to any root-cause fix.

## Issue 38 native release bootstrap

Scope: unsigned/unnotarized macOS arm64 native archive, not full issue #7. Publication and downloaded-install verification remain pending until actual successful evidence is recorded in issues #38 and #7.

`scripts/package-release.mjs FULL_SHA NEW_ABSOLUTE_OUTPUT` requires clean source, matching HEAD/version tag and main ancestry. It builds locked native binaries in an external temporary target directory, then packages an explicit public allowlist. `--local` permits unmerged worktree verification with unmistakable `local-unreviewed` manifest/CLI identity; never publish that local archive.

```sh
SOURCE_SHA=b0bedbb6654c6f65ba9270b5c52e20a56d4df79d
BASE=$(cd "$(mktemp -d)" && pwd -P)
node scripts/package-release.mjs --local "$SOURCE_SHA" "$BASE/package"
ARCHIVE="$BASE/package/fabric-v0.1.0-local.$(printf %.12s "$SOURCE_SHA")-macos-arm64.tar.gz"
HASH=$(cut -d ' ' -f 1 "$ARCHIVE.sha256")
node scripts/smoke-release.mjs --local "$ARCHIVE" "$HASH" "$SOURCE_SHA" "$BASE/install"
```

Manager local package verification passed on macOS arm64: archive SHA-256 `3268b92a01341b585e66edb5adc5dbac7c3aac75a9c33b4389d838789d9f5dfb`, base source `b0bedbb6654c6f65ba9270b5c52e20a56d4df79d`. The isolated installed archive reported `LOCAL_PACKAGE_ONLY`; executable/version, payload hashes, both plugin pins, pinned OMP discovery, disposable project and preparation passed with Cargo/Rust absent from child PATH. Providers and manager were not started; Codex discovery and host swarm were not tested. This verifies the unmerged local package, not a published release.

After manager review/merge and successful main CI, create a lightweight `v0.1.0` tag at that exact merged implementation commit, then dispatch:

```sh
: "${SOURCE_SHA:?accepted merged implementation commit}"
gh api repos/satansdeer/fabric/git/refs -f ref=refs/tags/v0.1.0 -f sha="$SOURCE_SHA"
gh workflow run release-macos.yml --repo satansdeer/fabric --ref main \
  -f source_sha="$SOURCE_SHA" -f tag=v0.1.0
```

Never move an existing tag or overwrite a release/asset. Inspect partial drafts before deciding how to proceed. Workflow package/publish/download jobs record the archive hash, public inventory and downloaded-install proof separately. Its tag/source/merged-PR/main-CI checks precede publication; only publication has write permission. Tag immutability is an operational no-overwrite rule, not Apple signing or an assertion about GitHub immutable-release settings.

The smoke uses the packaged executable/plugins and pinned official OMP 19.1.0 host only for provider-free discovery. Cargo/Rust are absent from child PATH. It checks a disposable project and prepared state, not auth/model access, installed OMP host integration or skill routing. User config/credentials and existing managers remain untouched.

The public manifest includes plugin/version/content pins, source SHA, file hashes, matching public OMP extension provenance and the bundled fake fixture. Consumer om-pstack #2 must use the downloaded version/hash, separately pinned OMP/pstack and Bun; `integrations/omp/host-smoke.ts` is bundled but not executed by this release smoke. No source-built fallback or full runtime-parity claim.

## C-00 baseline reproduction

Baseline SHA: `8e926f934f6508d114579e87d53e60a6d9ebdaf6`, macOS arm64. Documentation-only changes begin this milestone; no production behavior was changed for baseline reproduction.

Commands, run from the Fabric repository in this order:

```sh
git rev-parse HEAD
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --offline --workspace --all-targets -- -D warnings
cargo test --offline --workspace
cargo build --offline --workspace --bins
/Users/maksimivanov/.bun/bin/bun integrations/omp/conformance.ts "$PWD/target/debug/fabric"
```

Observed: all commands exit 0; 181 Rust tests pass across 29 suites. Bun conformance passes real CLI/tool/slash mixed batches, profiles, sealed artifacts/reclaim, typed no-allocation rejection, private request cleanup and safe transport failures. The conformance host is a fake OMP host, not the installed OMP runtime. Raw captured baseline output: session artifact `local://281.log`.

### Actual installed OMP, provider-free

Observed separately against installed OMP 19.1.0 using a disposable local clone, one prepared base and two inline fake-worker prompts. The clone was registered with `project add --name demo --path <clone> --json` and prepared with `project prepare demo --recipe <recipe> --json`; recipe step was literal argv `['/usr/bin/true']`.

OMP process invocation (absolute binary/extension paths; disposable variables substituted):

```sh
omp --mode rpc --no-ui --no-session --no-title --no-extensions \
  --no-skills --no-rules --no-context-files --no-lsp \
  --model openai/gpt-5.2 \
  --api-key fabric-control-plane-test-not-a-real-key \
  --max-cost 0.000000001 \
  -e "$FABRIC_REPO/integrations/omp/fabric.ts"
```

Environment was an explicit allowlist: disposable `HOME`, disposable `PI_CODING_AGENT_DIR`, system/Homebrew `PATH`, `LANG=en_US.UTF-8`, `FABRIC_BINARY` and `FABRIC_ROOT`. The dummy API key is not usable authentication. The command was first confirmed in `get_available_commands`; only then was a registered `/fabric <v1 JSON>` RPC prompt sent. Request used `version:1`, `op:spawn_many`, project `demo`, concurrency 2, harnesses `fake-a`/`fake-b`, distinct JSON write prompts, a literal fake model and explicit fake profiles. No model prompt was submitted.

Observed:

- Actual host exited 0; both durable runs `SUCCEEDED`.
- Separate `prompt_result` reported `agentInvoked:false`, `status:completed`, `sessionSettled:true`; no `agent_start` or model/tool execution event occurred.
- All 28 registered artifacts matched stored SHA-256 and byte lengths.
- Both workspaces were `RECLAIMED` and absent.
- Private fixture removed after inspection. This is `TESTED_WITH_FAKE` on an actual OMP host, not `LIVE_HARNESS_VERIFIED`.

## Controller compatibility inventory

Reference repository: `satansdeer/om-pstack`, local `/Users/maksimivanov/Workspace/pstack-omp`, branch `omp`.

- `/skill:swarm` is a manual skill, not a registered custom `/swarm` command. Native `task` returns asynchronous agent IDs, without Fabric's durable cross-controller lifecycle.
- Inherited swarm instructions default to cloud and mention Cursor-only placement/background/branch arguments. The OMP adapter documents native `task`'s actual shared `context` and `tasks` schema; it must not translate required remote execution into local work.
- C05 must explicitly distinguish ordinary native delegation, explicitly requested Fabric-local workers, and unsupported required remote/cloud. No `task` replacement or monkeypatch.
- Adapter source and drift-checked generator own generated plugin/skill outputs. Change `adapters/omp.md`, `scripts/adapt.py` and `tests/test_adapt.py`, then regenerate; never hand-edit generated output.
- `scripts/check_omp.py` makes provider calls; it is not a provider-free baseline check and has not been run.
- Installed-host deterministic command/tool tests cannot prove model-driven skill routing. Keep that gate separate and explicitly unverified without opt-in live execution.

## C-00 compatibility review

### Public and persisted compatibility

- Keep v1 `spawn_many` synchronous with final `{version,op,batch:{batch_id,succeeded,tasks}}`; strict bounded requests and inline prompts stay supported. Async acceptance gets a distinct API.
- Preserve `run`, `run-many`, `api --request`, `runs` and `storage` CLI contracts. Existing profile/env allowlist enforcement stays authoritative.
- Schema v9: `projects`, immutable `prepared_states`, `batches`, ordered `batch_tasks`, reciprocal `runs`/`workspaces`, immutable-provenance `worker_attempts`, immutable `artifacts`, `cleanup_intents`, `storage_reservations`, `worker_leases`, `cache_evictions`. Read-only inspection accepts v5–v9. New records require an additive, migration-tested version.
- Queued batch tasks can exist before a `runs` row. Accepted run IDs must remain stable. Persist prompt bytes/private owned inputs, not paths into v1's temporary prompt directory.
- Reuse preflight and the existing prepared execution/capture/cleanup path. Persist one exact prepared-state identity for a batch; explicit selection must not resolve current checkout HEAD.
- Persist native-context references/consent only, never credential values or opaque auth-store contents. Plan and immediate command construction must continue adapter authorization/profile revalidation.
- Separate submitter interruption from manager-owned cancellation. Only a live owned, unreaped process group may receive a signal; persisted PIDs never confer signalling authority.
- Crash/unknown ownership remains NEEDS_ATTENTION/RECOVERY_REQUIRED, never invented success, exit, capture or implicit rerun. Expired leases are not death/deletion proof.
- Preserve capture-before-cleanup: complete registered evidence and verified ownership/path/marker/lease/intent gates, then existing reclaim/GC only.

Read-only audit artifact: `local://fabric-c00-compatibility.md` (session-local detailed evidence map). No Fabric/om-pstack graph index exists, so source fallback was used.

The initially configured Rust LSP could not start: the rust-analyzer toolchain component was missing. Installed it with `rustup component add rust-analyzer`; `rust-analyzer --version` now reports `1.100.0 (0ef1bc386 2026-11-09)`. Initial reference lookup reported indexing/incomplete results; exported changes require completed lookup before edits.

## C-01 onboarding

`cargo test --offline -p fabric-cli --test onboarding` exited 101 before production edits: `init_check_reports_authless_context_without_mutation` and `connect_requires_consent_and_revoke_is_idempotent` failed on unrecognized `init`/`harness` commands. These use the actual CLI, isolated Git/native directories, environment clearing and a version-only stub; no provider calls. Raw session evidence: `local://285.log`.

GREEN commands:

```sh
cargo test --offline -p fabric-runner --test native_context
cargo test --offline -p fabric-core onboarding::tests
cargo test --offline -p fabric-cli --test onboarding
cargo fmt --all -- --check
cargo clippy --offline --workspace --all-targets -- -D warnings
cargo test --offline --workspace
cargo build --offline --workspace --bins
/Users/maksimivanov/.bun/bin/bun integrations/omp/conformance.ts "$PWD/target/debug/fabric"
```

Full suite: **190 passed, 31 suites**, 135.13 seconds; full gate 165.99 seconds. Bun passed the unchanged v1 compatibility proof. Raw final gate: `local://289.log`. Integration corrections before GREEN were fixture-only: supported Codex version, macOS canonical temp path, private temp-root permissions, prompt creation and reuse of public discovery rather than a nonexistent constructor.

Coverage includes default/overridden native roots, unreadable credential contents with metadata-only discovery, auth symlink/hardlink aliases excluded from fingerprints, private bounded consent storage, malformed/oversized/nonregular/linked selection refusal, missing-auth no-mutation, revocation after command construction, foreground/batch launch through the shared path, and independently authorized explicit profiles.

Actual installed OMP 19.1.0 human CLI smoke: environment-cleared `fabric --root <absent-private-fixture> init --harness omp --check` exited 0, displayed the canonical native directory, source commit/dirty/READY state, metadata-only authentication availability, exposure and next actions. The supplied storage root remained absent. **This proves discovery, not successful authentication or native worker execution.** No provider/model call was attempted.

## C-02 durable async lifecycle

Eight actual-CLI acceptance scenarios passed: four detached workers with artifact verification and reclaim; partial failure isolation; owned run/batch cancellation; one bounded pool across concurrent submissions; bounded wait and ordered reconnect cursors; concurrent idempotent admission; queued-only restart replay; and continued worker heartbeat after manager loss, without persisted-PID signalling.

Focused storage/core checks passed for atomic admission and immutable prepared references, queued cancellation/restart, v9-to-v10 evidence preservation, oversized frame rejection before allocation, and preservation of the original queued native-consent stamp after reconnect.

```sh
cargo test --offline -p fabric-cli --test async_workers
cargo test --offline -p fabric-store --test store async_
cargo test --offline -p fabric-store --test store v9_migration_adds_queue_without_rewriting_completed_run_evidence -- --exact
cargo test --offline -p fabric-core manager::tests
cargo test --offline -p fabric-core serialized_queued_plan_keeps_original_grant_after_reconnect
```

macOS-specific regression: accepted Unix sockets inherited listener nonblocking mode. A foreground-manager probe showed small status replies succeeded while larger terminal-result replies truncated. Accepted streams now explicitly become blocking before bounded read/write timeouts; the four-worker terminal-result/artifact scenario passed afterward. The owned probe and its completed workspaces were removed.

C-02 final compatibility gate passed: formatting; offline workspace Clippy with warnings denied; all 203 tests across 32 suites (128.95s); offline workspace binary build; and Bun real-CLI/OMP-extension conformance. Both corrected v1/v2 migration fixtures also passed their focused rerun. Combined command completed in 149.71s; raw evidence `local://303.log`. No provider/model calls. Generic worker requests, source selection and native pstack routing remain separate unverified gates.

## C-03 generic workers and v1 compatibility

`TESTED_WITH_FAKE`: version-free `workers spawn` accepts per-task native/explicit execution contexts, optional verified model overrides, and independent persisted limits; returns early task/run handles; shares the durable scheduler with v1. Native models are not guessed or passed as overrides. Remote placement, unverifiable models, hard spending caps and legacy portable skill/plugin/MCP lists reject explicitly before worker admission. Prepared-state/commit source refs remain unsupported until C04.

Eight CLI scenarios cover mixed fake protocols, early acknowledgement, native defaults, literal prompts, typed atomic preflight rejection, serial default concurrency, pinned OMP/Codex executable fixtures without provider calls, independent timeouts/capture limits, and legacy v1 responses/migration errors. Artifacts are size/digest verified after workspace reclaim. Fixtures are not live harness verification.

The first test failed on the missing `workers` command (`local://306.log`). Subsequent gates exposed and fixed manager startup before capability rejection, invalid option error typing, and lost v1 private prompt-file provenance. The existing v1 SIGTERM test now proves real 0600 prompt files under a 0700 parent while running and cleanup after completion.

Final command:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --offline -p fabric-cli --test run api_sigterm_cancels_owned_worker_seals_evidence_and_reclaims -- --exact
cargo fmt --all -- --check
cargo clippy --offline --workspace --all-targets -- -D warnings
cargo test --offline --workspace
cargo build --offline --workspace --bins
/Users/maksimivanov/.bun/bin/bun integrations/omp/conformance.ts "$PWD/target/debug/fabric"
```

All passed: focused regression 5.07s; 211 workspace tests/33 suites, 166.57s; combined gate 203.77s. Raw evidence: `local://317.log`. No model/provider calls. `copy_limit_bytes` bounds byte-copy fallback, not successful APFS clones; existing forced-copy backend tests pass, but native runtime fallback end-to-end remains `NOT_VERIFIED` on this APFS host.

## C-04 immutable sources and durable reconnect

`TESTED_WITH_FAKE`: explicit READY prepared-state IDs and full exact commit SHAs remain usable after the source checkout disappears; current-head never selects a different prepared commit. Admission validates ownership/readiness before allocation. Dirty changes are excluded and reported; each task freezes source provenance, and all referenced prepared states stay pinned across queued execution/restart. Inline prompt/source flags preserve literal bytes and reject conflicting inputs.

Additional REDs caught identical idempotent reconnect unnecessarily depending on a moved checkout, and acknowledged manager shutdown leaving an unserviced listener backlog. The latter failed on stop/reconnect cycle 4 (`local://328.log`); Stop now acknowledges after endpoint teardown, client drain, scheduler join and owner-lock release. No automatic admission retry or persisted-PID signalling was added.

Commands:

```sh
cargo test --offline -p fabric-cli --test workers manager_stop_acknowledges_release_before_immediate_reconnect -- --exact --nocapture
cargo fmt --all --check
cargo clippy --offline --workspace --all-targets -- -D warnings
cargo test --offline --workspace
cargo build --offline --bins
/Users/maksimivanov/.bun/bin/bun integrations/omp/conformance.ts "$PWD/target/debug/fabric"
```

All passed: focused regression 14.54 s; 220 workspace tests/33 suites, 156.77 s; combined gate 201.35 s. Raw evidence: `local://330.log`. Separate store-focused migration/contract proof: 27 tests passed, `local://329.log`. No provider calls.

## C-05 typed native OMP and explicit local pstack

- Pstack commit `437dd8e4b2ec579202fce2e81ca0900765b16472`, generated plugin
  `omp-0.15.46-7a3b678628a7`: five generator tests pass after actual RED (nine
  missing fail-closed drift subcases). No source/model/provider selection guessed;
  ordinary native dispatch remains the default. Explicit local routing does not
  fall back to cloud.
- Typed acceptance actual RED: missing `fabric_spawn`. Legacy and typed
  conformance then passed against real CLI/manager/fake worker processes.
- Actual pinned OMP `omp/19.1.0` initially caught unsupported
  `pi.zod.discriminatedUnion`, which the in-process proxy did not detect.
  Supported literal-tagged `union` schemas now pass actual-host positive and
  negative validation cases. Exposed schema API source:
  <https://github.com/can1357/oh-my-pi/blob/a7135b8f31826c878734388b648f52bed8da436e/packages/omptype/src/zod.ts>.
- Final combined provider-free gate passed in 54.70 s:
  `host-smoke.ts`, `conformance.ts`, `typed-conformance.ts`.
  Actual installed-host proof uses private HOME and the generated pstack plugin:
  early four-worker IDs; visible ordinary-context partial progress; controller
  close/reconnect without cancellation; independent real nonzero fake-worker
  failure; exact artifact digests and reclaimed workspaces; explicit cancellation;
  actual `/skill:swarm` discovery. No agent/provider turn.
- Retained native-host evidence:
  `/var/folders/1f/387q3rgd0_l041rkswmt1t_r0000gn/T/fabric-omp-host-8mOJ30`.
  This is native control-plane proof with fake workers, not live authentication,
  model-selected pstack routing, Codex/OMP live workers or cloud readiness.

## C-06 recovery and retained storage

Provider-free checks on macOS:

```sh
cargo test --offline --locked -p fabric-cli --test recovery -- --nocapture
cargo test --offline --locked -p fabric-core gc_recovers_promoted_sealed_capture_before_database_association -- --nocapture
cargo test --offline --locked -p fabric-store recovery_tests:: --lib -- --nocapture
```

- Two CLI tests passed in 46.05s. The mixed case starts twenty real fake-worker
  processes at concurrency four: successes, nonzero exits, cancellation, capture
  and cleanup I/O failures, partial staging, corrupt marker, substituted symlink
  and two genuinely killed supervisor processes.
- Fourteen runs are safely reclaimed; six remain retained with actionable
  ownership/evidence reasons. Repeated inspection/recovery/GC preserves artifacts,
  recorded outcomes, attempts, unknown completion and external/source sentinels.
  Cached async results expose newly registered artifacts and cleanup state.
- One core regression passed in 10.82s across three interrupted-capture boundaries:
  promoted/sealed, promoted/unsealed and complete/unsealed staging. A SQLite
  transaction abort creates the orphan; permission/rename changes simulate the
  other durable cutpoints. These are not claimed as process-crash injections.
- The schema-12 unit check passed in 0.02s: v11 evidence survives migration,
  pre-execution capture context is immutable, stale/invalid associations roll
  back, and recovery cannot manufacture an observed outcome or alter attempts.
- The first combined run failed both CLI tests because a shared store validator
  rejected current fourteen-file captures as not exactly seven. The correction
  preserves required legacy kinds and restores extended current manifests.
- Logical retained-byte accounting is best effort, not exclusive APFS allocation.
  Partial/corrupt evidence and live or unverifiable ownership remain retained.
  No provider/model calls were made; live OMP/Codex and model-selected routing
  remain `BLOCKED` / `NOT_VERIFIED`.
- `cargo fmt --all -- --check` passed. The store integration run passed 26 checks
  and exposed an outdated cleanup contract: verified recovered
  `NEEDS_ATTENTION` / `CLEANUP_PENDING` is now eligible. The corrected test still
  refuses `RECOVERY_REQUIRED` and passed its focused rerun (0.02s).

## C-07 release gates

- Native OMP 19.1.0, legacy adapter and typed-tool checks all passed against the
  current Fabric binary (85.52s combined), with pinned pstack plugin `437dd8e`.
  Actual schemas/tools, early four-worker handles, ordinary visible context,
  close/reconnect, independent failure, artifact digests/reclaim, cancellation
  and `/skill:swarm` discovery were observed. No agent/provider turn was sent.
- Native-host evidence:
  `/var/folders/1f/387q3rgd0_l041rkswmt1t_r0000gn/T/fabric-omp-host-uGdDOe`.
- `bash scripts/smoke-codex.sh` returned `NOT_VERIFIED` with explicit opt-in and
  provider-charge warning; no profile or provider was invoked.
- Human CLI surfaces were launched against that private fake-worker fixture:
  `storage status` displayed logical bytes, Run IDs and reclaim actions;
  `recover RUN_ID` displayed inspection-only `already_reclaimed`.
- Updated macOS workflow YAML parsed successfully. It retains locked offline
  Rust gates and adds both Bun protocols plus checksum-verified native OMP and
  pinned real pstack plugin discovery. Remote execution passed; see below.
- The initial workspace run passed the C06 twenty-worker recovery scenario,
  then reported 42 passing CLI run checks and four failures: a legacy-profile
  fixture still had schema12 capture context, two total-CLI timing assertions
  expired, and a cooperative TERM fixture was killed before exit zero.
  The legacy fixture now models v11 before migration; cooperative fixtures use
  an explicit two-second TERM grace. Outcome and timing assertions remain.
- CI/release acceptance uses `RUST_TEST_THREADS=4` to bound process-heavy
  test-case contention, not omit tests. A later strict lint rejected the
  descending storage comparator; `sort_by_key(Reverse(...))` preserves its
  ordering without allocation. The corrected gate passed on local
  Rust/Clippy 1.100.0: formatting, strict offline workspace/all-target lint,
  all 224 tests across 34 suites (289.49s), and locked offline binary builds
  (302.22s combined). No test was filtered out.
- Ten live-guide shell blocks and embedded Python parsed successfully without
  execution. The guide's `sh scripts/smoke-codex.sh` default returned
  `NOT_VERIFIED`; no live provider recipe was executed.
- Remote [macOS CI 45029533204](https://github.com/satansdeer/fabric/actions/runs/45029533204)
  passed in 5m0s on implementation commit `f86ac3c330`. Rust 1.91 formatting,
  strict lint, complete workspace acceptance and binary builds all passed;
  both Bun protocols passed; native OMP 19.1.0 download checksum verification
  and actual native-host/pinned-pstack checks passed. No provider/model turn
  was invoked. Live OMP, Codex, mixed-harness and model-selected routing
  remain explicitly BLOCKED/NOT_VERIFIED.

## TypeScript plugin SDK — issue #3

- One Fabric-managed native OMP 19.1.0 worker ran from source
  `568cc0296b72683506aa6379d93a7933473dc628` as
  `r-1795384877421260000-42614-0`: `SUCCEEDED`, exit 0, workspace
  `RECLAIMED`, capture not truncated. The controller verified all fourteen
  retained artifact lengths and SHA-256 digests before recovering source into
  a separate review checkout. No second worker or automatic retry was launched.
- Controller checks on macOS with Node 22.23.2:
  - `cargo build --offline --locked --bin fabric` passed.
  - `cd sdk/typescript && npm ci --ignore-scripts` passed.
  - `FABRIC_PLUGIN_TEST_HOST=/absolute/review/target/debug/fabric npm test`
    passed typed contracts and all **12 tests, zero skips**. Actual subprocesses
    exercised all three generated plugin kinds, offline npm-packed scaffolding,
    watch-mode reruns, bounded malformed input, pin/version rejection and
    secret-free failures. Real Core installation/check/tamper rejection/upgrade/
    revoke used an owner-only temporary registry outside every Git checkout.
  - `cargo fmt --all -- --check` passed.
  - `FABRIC_PLUGIN_TEST_NODE=/absolute/node cargo test --offline --locked
    -p fabric-runner --test plugins` passed **7 tests**, including generated
    TypeScript plugins through the native host and pin checks before execution.
  - `cargo test --offline --locked -p fabric-core --test workspace
    external_plugin_reclaim_claim_cannot_bypass_artifacts_or_ownership`
    passed **1 test** after updating its manifest constructor for optional pins.
- SDK fixtures are provider-free; the implementation worker was a separately
  owner-approved model-backed run. These checks do not claim public npm
  publication, sandbox isolation, real provider-plugin execution or completion
  of harness/target/connector extraction (#4–#6).
- Pre-merge review found the push CI run failed in typed OMP conformance with
  `SQLITE_BUSY` while reading workspace state from the live manager database;
  the pull-request CI run on the same commit passed. The shared test query
  connection now sets SQLite's bounded `busy_timeout = 5000` instead of failing
  immediately during a concurrent manager write. The unchanged typed conformance
  scenario passed locally after that fix; no provider call or automatic retry
  was used.

## Issue #4 — extracted native plugins, provider-free worker evidence

Implementation evidence only; human review and external-plugin provider acceptance remain separate.

- Rust 1.91.0, Node 24.7.0, offline dependency caches, `RUST_TEST_THREADS=4`.
- `cargo build --offline --release -p fabric-reference-harnesses --bin fabric-reference-harnesses`: PASS.
- `cargo build --offline -p fabric-cli --bins`: PASS.
- Rust fixtures used `FABRIC_REFERENCE_PLUGIN_BINARY` pointing to that release executable and `FABRIC_PLUGIN_TEST_NODE` pointing to Node. Each native fixture explicitly installed a private, content-pinned reference package.

| Local check | Observed result |
| --- | --- |
| `cargo test --offline -p fabric-runner -p fabric-reference-harnesses` | 47 passed across 11 suites |
| `cargo test --offline -p fabric-core onboarding` | 6 passed |
| `cargo test --offline -p fabric-core --test workspace` | 11 passed |
| `cargo test --offline -p fabric-cli --test doctor --test onboarding --test run --test workers` | doctor 8 passed; onboarding 3 passed; run 47 passed; workers 15 passed, 1 failed |
| `cargo test --offline -p fabric-cli --test workers unverifiable_native_model_rejects_before_manager_admission` after the diagnostic fix | 1 passed, 15 filtered; 3.91 seconds |
| `npm test` in `sdk/typescript`, with canonical `FABRIC_PLUGIN_TEST_HOST` and `FABRIC_REFERENCE_PLUGIN_BINARY` paths | typed contracts passed; 15 tests passed, zero skipped |

The worker failure was loss of the existing `MODEL_UNVERIFIED` diagnostic across sanitized plugin errors. Core now reuses required-capability validation on an unsupported-capability planning response; the original consumer-visible assertion passes without forwarding plugin exception text. The combined CLI command was not rerun after this focused fix.

Earlier check failures also identified and corrected external `--fake-binary` rejection classification, missing installed-ID classification, the SDK's vendor-event conformance fixture, and SDK temporary-directory cleanup after artifact/source sealing. Initial SDK setup needed the existing offline TypeScript cache and canonical executable paths. No contract assertions were weakened.

Passing checks exercise explicit OMP/Codex installs, native-context consent and fingerprints, literal prompts and native argv, typed event normalization, malformed/lifecycle-spoofed event rejection, persistent normalization and cancellation/reaping, artifact hashes and storage/reclaim, existing v1 behavior, mixed fake harness batches, and third-party SDK harness execution without Core registration changes. Control-message tests check credential availability rather than credential values and reject ambient-secret leakage.

Actual CLI checks: `fabric harnesses list` with no plugin registry lists only `fake`, `fake-a`, `fake-b`; `fabric init --harness omp --check --json` fails clearly with `unknown_harness` and explicit-install guidance. No silent native fallback.

No live OMP/Codex model-provider turn, service connection, credential change, deployment, source commit, push or GitHub mutation was performed. Full-workspace lint/test gates were not run in this worker. Provider-backed plugin execution remains **NOT_VERIFIED**; these results do not declare issue acceptance.

### Controller review and acceptance

- One approved Fabric/native OMP 19.1.0 worker completed from merged source `858c844ae4356c8ea673de61df52694dee52c4da`: run `r-1795387683252776000-51206-0`, `SUCCEEDED`, exit 0, workspace `RECLAIMED`. All 14 retained artifacts were size/SHA-256 verified before importing source changes. The 64 MiB raw capture is **TRUNCATED**; private native logs were not published. This is implementation-worker evidence, not a provider-backed extracted-plugin acceptance run.
- Distribution review added real Core `plugins check` calls against both installed reference packages with local native CLI doubles and ambient-secret rejection; CI now builds the separate release executable before tests and exports its exact path. The SDK guide documents the private explicit install, reviewed content pin, native CLI prerequisite and manager upgrade boundary.
- Runtime review fixed execution-deadline propagation into persistent normalization, including initialization and each RPC. Deadline expiry disables further normalization and reports `Timeout` even after a successful native process exit, without cancelling the caller's token or sibling workers. Cancellation no longer creates a synthetic plugin failure. Raw capture continues in both cases.
- Strict lint exposed an eight-argument control helper; grouped its duplex streams rather than suppressing the lint.

| Controller check | Observed result |
| --- | --- |
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | PASS |
| `cargo build --offline --locked --bins` | PASS |
| `cargo build --offline --locked --release -p fabric-reference-harnesses --bin fabric-reference-harnesses` | PASS |
| `cargo test --offline --locked -p fabric-runner --test plugins native_capture_respects_the_execution_deadline_and_preserves_cancellation` | 1 passed, 9 filtered; 1.99 seconds |
| `python3 scripts/test-fabric-issue.py --fabric ABS_BINARY` | PASS; actual provider-free issue/worker/artifact workflow |
| `bun integrations/omp/conformance.ts ABS_BINARY` | PASS; fake OMP host, no provider call |
| `bun integrations/omp/typed-conformance.ts ABS_BINARY` | PASS; in-process host, no provider call |
| `cargo test --offline --locked --workspace` | PASS; full workspace, including migrated native fixtures, worker admission and the new deadline/cancellation regression |
| `npm test` in `sdk/typescript`, with actual Core host and release reference executable | typed contracts PASS; 15 passed, zero failed, zero skipped |

The combined full-workspace and SDK command completed in 392.52 seconds. The SDK run exercised both installed native reference executables through real Core `plugins check` and executed a third-party fake harness through Core without vendor registration changes. These provider-free results satisfy the issue's local fixture gates, not live model-provider or remote-target acceptance.

Controller: macOS arm64, Node 22.23.2, `RUST_TEST_THREADS=4`, explicit release reference executable and actual debug Core host. No separate live-provider smoke, credential change, service deployment or npm publication.
