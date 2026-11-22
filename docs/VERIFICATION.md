# Corrective milestone verification

## Evidence labels and release gates

`IMPLEMENTED` means code exists; `TESTED_WITH_FAKE` means deterministic provider-free lifecycle proof; `LIVE_VERIFIED` means an actual provider-backed harness run with recorded evidence; `NOT_VERIFIED` and `BLOCKED` are not passes.

| Gate | Current status | Evidence |
|---|---|---|
| LOCAL_WORKER_API_READY | TESTED_WITH_FAKE | Async/generic API and immutable-source acceptance passed: 220 workspace tests/33 suites and adapter conformance. Recovery/storage follow-up remains C06. |
| PSTACK_LOCAL_SWARM_READY | NOT_VERIFIED | Existing pstack skill routes native tasks; explicit Fabric bridge pending. |
| LIVE_HARNESS_VERIFIED | BLOCKED | No authorization for provider spending; no live worker calls made. |

No cloud readiness claim is made. Native installed-host loading and fake workers do not establish authentication/model access or model-driven skill routing.

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
