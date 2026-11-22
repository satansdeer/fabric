# Corrective milestone verification

## Evidence labels and release gates

`IMPLEMENTED` means code exists; `TESTED_WITH_FAKE` means deterministic provider-free lifecycle proof; `LIVE_VERIFIED` means an actual provider-backed harness run with recorded evidence; `NOT_VERIFIED` and `BLOCKED` are not passes.

| Gate | Current status | Evidence |
|---|---|---|
| LOCAL_WORKER_API_READY | NOT_VERIFIED | Baseline v1 API is synchronous. C02–04 pending. |
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
