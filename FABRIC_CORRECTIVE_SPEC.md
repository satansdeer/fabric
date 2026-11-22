# Fabric Corrective Specification — v0.3

**Status:** Developer implementation handoff  
**Baseline:** [`satansdeer/fabric` commit `8e926f934f6508d114579e87d53e60a6d9ebdaf6`](https://github.com/satansdeer/fabric/commit/8e926f934f6508d114579e87d53e60a6d9ebdaf6)  
**Intent:** Correct product boundaries, remove unnecessary complexity, and ship a genuinely usable **local, harness-neutral worker layer**. **This is an incremental change, not a rewrite.**  
**Primary acceptance consumer:** `satansdeer/om-pstack` `/swarm`, using OMP as the controller.  
**Additional worker harness:** Codex. **Target platform:** macOS, trusted local execution.  
**Process:** TDD, one bounded story at a time, with explicit evidence.  

## 0. Source-grounded findings (at baseline)

These observations were verified from the pinned GitHub source, not inferred from earlier plans:

| Observation | Source |
|---|---|
| `fabric_core::run::spawn_many` executes workers with scoped threads and joins before returning; it is a **foreground batch**, not a durable async worker service. | [`crates/fabric-core/src/run.rs#L470-L613`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-core/src/run.rs#L470-L613) |
| Current OMP extension presents a raw `{request: JSON}` tool and `/fabric <JSON>` slash command. | [`integrations/omp/fabric.ts`](https://github.com/satansdeer/fabric/blob/8e926f9/integrations/omp/fabric.ts) |
| `select_prepared()` resolves the registered checkout's current HEAD before selecting a READY state; it does not select a stored immutable revision independently. | [`crates/fabric-core/src/run.rs#L133-L155`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-core/src/run.rs#L133-L155) |
| Local workspace creation is hardwired to `LocalWorkspaceBackend` inside run orchestration. | [`crates/fabric-core/src/run.rs#L192-L204`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-core/src/run.rs#L192-L204) |
| Live OMP/Codex require explicit profiles and credential-connection references. | [`crates/fabric-runner/src/omp_profile.rs`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-runner/src/omp_profile.rs), [`codex_profile.rs`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-runner/src/codex_profile.rs) |
| `AgentProfile` exposes `skills`, `plugins`, `mcp`, but current runner rejects those as imports. | [`crates/fabric-domain/src/profile.rs`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-domain/src/profile.rs), [`crates/fabric-runner/src/profile.rs`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-runner/src/profile.rs) |
| Some execution failures enter `NEEDS_ATTENTION` / `RECOVERY_REQUIRED`; ordinary GC refuses anything not eligible for verified cleanup. This is safe but can strand disk space. | [`crates/fabric-store/src/lib.rs#L1242-L1280`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-store/src/lib.rs#L1242-L1280), [`crates/fabric-core/src/workspace.rs#L521-L625`](https://github.com/satansdeer/fabric/blob/8e926f9/crates/fabric-core/src/workspace.rs#L521-L625) |
| Documentation itself says native `/swarm` routing, real cloud execution, and live-provider tests are not yet verified. | [`docs/CONTROL_SURFACES.md`](https://github.com/satansdeer/fabric/blob/8e926f9/docs/CONTROL_SURFACES.md) |

**Reported, not independently re-run:** 181 Rust tests passed and installed OMP completed a fake-worker control-surface test. Live model-backed OMP/Codex worker execution remains unverified.

## 1. Product contract

**Fabric is a thin, harness-neutral layer for launching, observing, coordinating and cleaning up coding-agent workers locally, with execution backends replaceable later.** It does *not* replace the selected harness's model authentication, skills, MCP, plugins, prompts or native configuration system.

A local user should be able to:

```sh
fabric init                     # One-time project detection and harness connection
fabric workers spawn --harness omp --prompt 'Investigate the caching bug'
fabric workers list
fabric workers show <run-id>
fabric workers cancel <run-id>
```

And from a supported controller harness (first: OMP), an agent-facing delegation tool should submit several independently isolated workers **without requiring the model to author a versioned raw JSON envelope or the user to prepare prompt files by hand**.

**Success is behavioral:** the OMP port of pstack's `/swarm` can run N real **local** Fabric workers, return quickly with durable handles, collect normalized outputs/evidence, and reclaim disposable filesystem state. **Do not call that cloud parity.**

### Core invariants — must not regress

1. A run is not a process; a run identity and its result outlive a process or controller session.
2. A workspace is owned and disposable; a preserved patch/report is not. No deletion until capture and ownership checks pass.
3. Prepared bases stay immutable; workers have independent writable copies.
4. Unsupported placement, model override, session resume, security guarantees or hard cost caps fail explicitly, before allocating work.
5. No executable name, Git ref, user prompt, environment value, or filesystem path is interpolated into an implicit shell command.
6. Local trusted execution is **not** a multi-tenant security sandbox. Process HOME/env scoping is not OS-level credential isolation.
7. The Rust scheduler/core must not import OMP, Codex or pstack-specific types or branches. Controller and worker harness choices are independent.
8. Model spending is opt-in for acceptance tests; tests never quietly use real credentials.
9. Preserve compatibility with the prior CLI wherever feasible; if a wire schema changes, version it and provide actionable migration errors.

## 2. KEEP — do not rewrite or delete

- Existing seven-crate Rust workspace and SQLite-backed domain/run/workspace/artifact lifecycle, unless a targeted change is justified by tests.
- `HarnessAdapter` (probe / negotiate / construct command / normalize native events) and the generic registry. Keep OMP and Codex as real adapters, and deterministic fake adapters for tests.
- Immutable prepared project snapshots, private workspace forks, source SHA/provenance, safe artifact manifests, bounded logs, run/worker attempt history.
- Safe deletion: descriptor-rooted path checks, markers, leases, persisted cleanup intent, verification before reclaim, refusal on uncertainty. Do **not** replace with `remove_dir_all` on guessed paths or time-based cleanup.
- Explicit capability rejection and truthful status reporting (including best-effort-only cost support and macOS-only support).
- Existing CLI `run`, `run-many`, and `api --request ...` as low-level compatibility/diagnostic entry points. New UX should sit above them, not fork the execution engine.
- Thin controller integration that calls the same Rust API. Never put scheduling/worker lifecycle inside `integrations/omp`.

## 3. REMOVE / SIMPLIFY — explicit decisions

| Existing element | Change required | Deletion/migration rule |
|---|---|---|
| Universal `AgentProfile.skills`, `.plugins`, `.mcp` arrays that cannot currently be imported | **Remove from the new minimal user-facing request/profile contract.** Native harness already discovers its own skills/MCP. | Preserve parsing of existing v1 inputs if needed; reject nonempty legacy imports with a clear migration message. Do not silently ignore them. Avoid storing duplicate conceptual profile data. |
| Per-run handwritten credential-connection manifests for trusted-local default | **Replace as the standard UX** with a one-time explicit `fabric harness connect <id>` / `fabric init` consent to run the installed harness using a selected native config context. | Keep low-level explicit env/config scoping as an advanced option. Do not blanket-inherit all environment variables or silently read/copy credential stores. Do not build a secrets vault. |
| Core-level assumptions about particular providers, credential files, MCP formats or model IDs | **Keep in harness adapters or remove if not exercised.** Core needs an opaque, bounded harness execution-context reference and generic optional model request. | Introduce a narrow compatibility adapter for old profile JSON; add tests before schema migration. No automatic migration of credentials. |
| Speculative capabilities with no executable path (portable profile import, session follow-up, cloud placement, etc.) | Retain only as honest **unsupported** capability reports, not as eagerly built plumbing in current runtime. | Do not delete guardrails that currently prevent false claims. No fake implementations. |
| `fake`, `fake-a`, `fake-b` in normal user discovery | Move toward fixture/test-only visibility behind explicit test mode where practical. | Keep test functionality and CI conformance. Do not delete fixture adapters/tests. |
| `/fabric <literal JSON>` as the expected user interaction | Keep as advanced/debug fallback, but **stop documenting it as normal usage**. Add typed agent tools and a brief human CLI. | Do not remove the v1 JSON API until v2 callers and tests work. |
| Repeated manual project registration, recipe path and prompt file steps | Add a guided `fabric init` and simple prompt submission. | Never auto-execute an inferred setup recipe without user confirmation. Preserve explicit prepare override. |
| Future cloud/BYOC placement fields accepted in schemas | Report unsupported in local mode. | Do not map `remote` or `byoc` to local as a fallback. |

**Not a deletion request:** `AgentProfile`, `CredentialConnectionRef`, capability negotiation and version pinning contain real enforcement today. Refactor them incrementally; do not rip them out in one change or weaken execution safety. Prefer removing unsupported *user-facing concepts* while keeping internal evidence and consent enforcement.

## 4. IMPLEMENT — ordered milestone contracts

### C-00 — Baseline verification and behavior map (gate)

- Pin the working tree to the reviewed commit (or explicitly record the newer SHA and diff).
- Run fmt, Clippy, full offline test suite, existing Bun/OMP no-model conformance; record exact command/output.
- Inventory APIs and migration-sensitive fields, including existing v1 requests and persisted SQLite records.
- Add a short `docs/PRODUCT_BOUNDARY.md` stating trusted local versus cloud, harness-native auth ownership, and no hard isolation claim.

**Done when:** baseline can be reproduced or blocked dependencies are documented; no production behavior changed. Do not claim tests passed if not executed.

### C-01 — Trusted-local harness onboarding

- Implement `fabric init` from a Git checkout, with optional `--harness omp|codex` and a noninteractive `--check` mode.
- Discover installed harness/version, repo, whether a READY state exists, and likely setup recipe. Present the *exact* config directory/credential exposure requested. Ask once for consent in interactive mode, persist a local-only selection with owner-only permissions, and allow revocation.
- No Fabric login or provider login: the harness uses **its own** native authentication. No opaque credential file is copied, dumped, parsed for secrets, or uploaded.
- If the selected native context is absent or unauthorized, fail with a one-line actionable diagnostic. Explicit advanced env-key allowlists remain supported.
- For projects without a READY state, offer to create or review a recipe and run preparation after explicit approval. Do not assume `npm install` for every repo.

**TDD acceptance:** authless discovery causes no model call; connecting selected native context enables a fixture-run; unrelated ambient keys are not forwarded; refusal/revocation prevents launch; old explicit profiles still behave as before or return a versioned migration error; macOS-specific directories are handled; project source is untouched.

### C-02 — Persisted asynchronous worker API (highest-impact feature)

Provide semantics independent of the controlling CLI/OMP call:

```text
submit(tasks, project, source, concurrency) -> {batch_id, run_ids, accepted}
status(run_id | batch_id) -> queued/running/terminal/recovery + summary
results(run_id | batch_id) -> normalized outputs, artifact references, errors
cancel(run_id | batch_id) -> acknowledged / already terminal / not owned
wait(run_id | batch_id, timeout) -> bounded wait or current status
```

- Existing foreground `spawn_many` remains usable as `submit + wait` convenience; **do not duplicate scheduling logic**.
- A long-lived local supervisor is permitted **only as needed** for worker survival after the controller process exits. Prefer one small single-user manager and SQLite job queue with a local Unix socket, auto-started on demand. Do not design a distributed control plane.
- Atomically persist accepted work before returning IDs; queued work may be replayed idempotently after manager restart. Running work after a crash must be reconciled against ownership/lease evidence and marked `NEEDS_ATTENTION` if its fate is uncertain. Do not silently rerun potentially side-effecting work or claim success.
- Cancellation must target authorized owned process groups; never signal a PID reconstructed solely from a stale DB field. Parent interruption must **not** cancel independently submitted background workers unless cancellation was explicitly requested. Preserve foreground Ctrl-C behavior for foreground mode.
- Normalized events should have an ordered cursor and bounded retention; `status` and `results` are always available from persisted data, even if the original controller exits.
- Return a partial batch result when one worker fails. Do not suppress healthy siblings or lose error evidence.

**TDD acceptance:** submit 4 fake workers returns before slow workers finish; parent exits; manager finishes work; new client reads completed runs; cancel a running worker with verified ownership; restart manager mid-queue; kill manager mid-run and assert no fabricated completion, no unowned process signaling, and safe recovery classification; duplicate submit with idempotency key creates no duplicate batch; two concurrent client submissions do not corrupt queue state.

**Scope boundary:** no distributed consensus, remote scheduler, Kubernetes, gRPC microservices, autoscaling, native harness session resume or always-hot agent processes.

### C-03 — Generic worker-harness request, not pstack-shaped data

- Keep a small generic `WorkerTask` contract with `task_id`, `prompt`, `harness_id`, `execution_context?`, `source_ref?`, `model?`, `limits?`.
- Model is optional. Inherit the worker harness's native default if omitted. Explicit model override must only be accepted when the adapter can enforce/verify it; otherwise return a typed unsupported response.
- Keep worker harness independent of controlling harness. Same request can mix OMP and Codex with different tasks.
- Keep backend selection (`local` now, future `remote`/`byoc`) independent of the harness. Reject unsupported selection before side effects.
- Do not require consumers to provide JSON schema version, `default_harness`, `profile` internals and artifact limits for the common case. Version those on the internal wire protocol only.

**TDD acceptance:** two fake harnesses with different event protocols pass the same core conformance suite; OMP and Codex adapters both use generic API; no `if harness == "omp"` branches outside adapter/registration modules; explicit unsupported model/placement/capabilities fail before workspace allocation.

### C-04 — Source-state selection and project ergonomics

- Add immutable READY state selection by `prepared_state_id` (preferred) or exact commit SHA. The selected READY state must remain runnable if the original checkout has moved or become temporarily unavailable, *provided the state and its ownership can still be verified*.
- Keep a convenience source selector (`current-head`) for local workflows; fail explicitly if requested commit has not been prepared. Never silently execute an older revision when the user requested HEAD.
- Continue excluding dirty/uncommitted changes unless an explicit, reviewed transfer feature is implemented later.
- Add a simple `fabric run <prompt>` or `fabric workers spawn --prompt ...` route that uses current project linkage and native harness; retain advanced CLI options.

**TDD acceptance:** prepare commit A, move checkout to B, execute explicit A; prepare B, execute B; remove/move original checkout and execute explicit A; immutable base untouched; missing/not-ready SHA rejected; dirty checkout is reported and no uncommitted data leaks into the worker.

### C-05 — Agent-friendly OMP control surface and real `/swarm` bridge

- Preserve `integrations/omp/fabric.ts` as a **thin TypeScript adapter** over the versioned Rust API.
- Add typed agent-facing tools (names provisional): `fabric_spawn`, `fabric_status`, `fabric_results`, `fabric_cancel`. The common `spawn` call accepts concise tasks with inline prompts and harness IDs. Its result returns immediately with run handles. Keep a debug JSON escape hatch.
- Integrate via the **om-pstack OMP adapter**, not by embedding pstack behavior inside Fabric Core. Map pstack's cloud-worker *intent* onto Fabric only when placement requirements are truly satisfied. For local-mode testing use an explicitly requested local worker. If a skill explicitly requires remote cloud placement, report unsupported until a remote backend exists.
- Provide progress/completion delivery to an active OMP controller when supported, and durable `results` retrieval after reconnect. A controller does not need to poll in a hot loop; the runtime should expose a bounded wait/event subscription mechanism.
- Do not replace or monkeypatch OMP's native `task` tool. Keep ordinary local tasks working.
- Preserve exact source revision, requested harness, model (if supported), and per-worker evidence in results.

**TDD acceptance:** one `/swarm`-style OMP controller request starts 4 independent Fabric local workers using the same prepared base and different prompts, returns handles promptly, can read progress/results, sees failures as failures, preserves artifacts, and reclaims workspaces. Test the extension in actual installed OMP without model calls using fake workers, then run opt-in live harness tests. No manual prompt files, raw API JSON or `fabric project prepare` commands in the normal **already-initialized** workflow.

### C-06 — Safe failure recovery and bounded disk responsibility

- Audit all transitions that create `NEEDS_ATTENTION` / `RECOVERY_REQUIRED`; add a recoverable state inspection action and reasoned `fabric storage status` summaries.
- On failure/crash, attempt safe recovery capture **only after confirming the worker is no longer active** and verifying file ownership and path identity. If capture is complete, seal artifacts, transition to cleanup eligibility, reclaim via the existing safe path.
- If safe recovery capture cannot be established, retain the workspace and surface a clear, actionable warning showing run ID, bytes, and reason. Do **not** delete based on TTL alone. Document if expiration is merely an alert/escalation policy.
- Proactively enforce storage headroom/admission for Fabric-managed work. Distinguish logical bytes from APFS exclusive physical storage; do not promise host disk can never fill.
- `fabric storage explain` or equivalent should report the largest retained run-owned workspaces, why they exist, and what action can recover space.

**TDD acceptance:** 20 workers (successful, cancelled, exit failures, crash during capture, crash during cleanup, corrupted marker, symlink substitution); captureable failures reclaim safely; unrecoverable ones remain protected and visible; repeated GC is idempotent; no deletion beyond managed root; concurrency/leases never permit cleanup of a live worker; recovered artifacts retain valid digests.

### C-07 — Real harness smoke and end-to-end release gates

- Run *opt-in* actual OMP execution with the user's native config and a minimal bounded task; validate output, captured artifacts and cleanup. Repeat with Codex if available. Report exact version and whether authentication/model invocation actually succeeded. No silent spending in default CI.
- Run a mixed OMP + Codex batch under the same API with separate workspaces. If unavailable, report **NOT_VERIFIED**, not PASS.
- Run `/swarm` integration once provider-free, once opt-in live with a small budget and independent workers. Log version, exact invocation, source SHA, artifact digests and cleanup outcome.
- Keep CI `cargo fmt --check`, Clippy `-D warnings`, `cargo test --offline --workspace`, adapter conformance and native OMP control-surface checks.

**Release labels:** `LOCAL_WORKER_API_READY`, `PSTACK_LOCAL_SWARM_READY`, and `LIVE_HARNESS_VERIFIED` are independent evidence gates. **Do not issue `PSTACK_CLOUD_READY`** until a genuinely remote, isolated backend exists and survives local client exit.

## 5. Explicit NON-GOALS — do not implement in this corrective milestone

- No cloud provider backend, Fabric Cloud account system, BYOC, EC2/Fly/Daytona integration, Kubernetes or multi-tenant runtime.
- No distributed scheduler, replication, global control plane, event bus, autoscaling, multi-region persistence, billing system or cloud dashboard.
- No credential vault, OAuth brokerage, model gateway, provider-account manager or universal harness configuration compiler.
- No copying local auth directories into remote execution; no secret scanner that purports to prove safety. For trusted-local mode, use explicit native-config consent and existing harness auth.
- No custom MCP transport or skill/plugin installer. Do not implement skills/MCP import in Fabric core.
- No generic DAG/workflow editor, GitHub label/webhook automation service, CI watcher, durable native session resumption, PR publisher, or `/loop` reimplementation.
- No implementing all harnesses (Droid/Factory, Hermes, Qwen, Claude Code, etc.) now; maintain a generic adapter contract and prove OMP + Codex.
- No pstack-specific dispatcher logic or Cursor `Task` field emulation in the Rust core. This milestone proves **local `/swarm` behavior**, not full one-to-one Cursor/pstack equivalence.
- No unsafe claims: isolated worktrees do not imply process or credential security isolation; a `max_cost_usd` adapter option is not a universal hard spending cap; stored state does not imply hot CPU.
- No forced POSIX/Linux port while the codebase has explicitly tested macOS assumptions. Future Linux should be planned only after local behavior is proven.
- No wholesale crate restructuring, new databases, gratuitous generalized trait systems or cosmetic UI work unless a story's acceptance tests require them.

## 6. Developer-facing API sketches (design targets, not existing commands)

### Minimal interactive happy path

```sh
cd ~/projects/my-project
fabric init
# detects repo, detects OMP, asks permission to use selected native config,
# checks a ready project state or offers to prepare one after approval

fabric workers spawn --harness omp --prompt 'Investigate test flakiness' --background
# → accepted: run r-..., source sha ..., status queued

fabric workers show r-...
fabric workers results r-...
fabric storage status
```

### Generic agent-to-Fabric tool call

```json
{
  "project": "my-project",
  "source": { "kind": "prepared", "state_id": "ps-..." },
  "tasks": [
    { "id": "investigate", "harness": "omp", "prompt": "Find the race condition" },
    { "id": "review", "harness": "codex", "prompt": "Independently inspect the same revision" }
  ],
  "max_concurrency": 2
}
```

Return `{batch_id, tasks: [{id, run_id, status: "queued"}]}` immediately; show and results are separate. For explicit `placement: "cloud"`, fail `unsupported_placement` in local-only mode. If one task requests an unverified model override, fail preflight rather than quietly ignoring it.

## 7. Ralph/TDD execution contract

Each iteration must:

1. Read this spec, `docs/PRODUCT_BOUNDARY.md`, the current plan/progress file and relevant source.
2. Pick exactly one smallest unfinished acceptance criterion from C-00 through C-07; do not start dependent work before prerequisites pass.
3. Write a failing test, including a negative/safety test where applicable. Show why it fails.
4. Implement the minimum code change; do not add speculative scaffolding.
5. Run focused tests, then fmt/Clippy/full offline suite; note any environment blocks honestly.
6. Record changed files, commands, results, behavioral evidence, remaining blockers and next smallest criterion. Commit only after verification.

**Stop conditions:** never mark an unverified real harness as live-tested; never accept a fake backend as cloud; never weaken cleanup/credential checks to pass a test; never modify acceptance criteria without documenting a deliberate product decision.

**Required code review before a story is considered done:** check for OMP-specific logic in Rust core, accidental ambient environment inheritance, secrets in diagnostics, orphan processes, untracked unrecoverable writes, deleting paths after ownership uncertainty, and non-idempotent retry of side-effecting work.

## 8. End-to-end definition of done

A new contributor can install/build Fabric, run `fabric init`, explicitly connect a supported installed harness, and start a worker against a prepared repository **without writing a profile JSON or prompt file**.

Inside OMP, an `om-pstack` `/swarm`-equivalent local task invokes Fabric's typed tool, starts four workers and **returns control while they run**. The user can close/reopen the controlling client and inspect persisted results. After completion or recoverable failure, patches, reports, stdout/stderr, provenance and run records remain accessible; disposable workspaces are reclaimed without touching the original checkout. Unsupported cloud placement explicitly rejects, and no default test makes a paid model call.

A second real harness adapter (Codex) uses the same API; actual provider execution is verified separately, not inferred from fake-adapter tests. Code and documentation state precisely what is supported, unverified, and out of scope.

## 9. Deliverables

- Updated Rust code + OMP extension + minimal om-pstack adapter integration.
- A `docs/PRODUCT_BOUNDARY.md` that stays explicit about supported modes and non-goals.
- CLI help and migration note for old v1 raw JSON/profile users.
- Automated TDD/integration tests and opt-in live smoke scripts.
- `docs/VERIFICATION.md` with exact commands, observed results, evidence-gate statuses and remaining limitations.
- A concise `docs/REMOVALS.md` listing which speculative fields/paths were deleted, deferred or preserved for compatibility, with justification.

**Final rule:** optimize for *less user setup and fewer persistent moving parts*, not a larger number of abstractions. Do not introduce a new subsystem unless it is necessary to pass a concrete acceptance test above.
