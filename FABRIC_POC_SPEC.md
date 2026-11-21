# Fabric PoC Specification — v0.1

**Status:** implementation-ready draft  
**Target:** Rust, Linux, local-first  
**Primary consumer:** OMP/om-pstack  
**Purpose:** Prove that Fabric can run many independent coding-agent tasks against a prepared project **without repeating setup and without leaving uncontrolled workspace garbage**.

## 0. Product contract

A user registers a Git repository, prepares it once, runs several independent tasks in isolated workspaces, inspects their outputs, and releases the temporary workspaces. Fabric preserves evidence and recoverable source edits before deleting anything. The physical execution directory is temporary; the Run ID and resulting artifacts are durable.

**Demo acceptance:** Given a fixture repository with a preparation script that creates an ignored dependency marker, `fabric project prepare` executes the script once. `fabric run-many` starts four independent workers from the same prepared revision; each sees the dependency marker immediately and writes a distinct result. After they finish and cleanup succeeds, no per-run workspace directories remain, the prepared base remains, and results and recoverable changes are inspectable by Run ID. Re-running does not rerun preparation unless the preparation key changes. This demo uses a deterministic fake harness in CI and is repeated manually with a real OMP binary.

## 1. Goals and non-goals

### Must prove in PoC

1. **Prepared project state:** fetch/clone a pinned local Git revision into a private prepared base, execute configured preparation commands once per fingerprint, reuse the resulting files including ignored dependency directories.
2. **Isolated forks:** create a private workspace for each task. No task can change the prepared base or another task's files under ordinary (non-adversarial) operation.
3. **Real harness execution:** run an installed OMP CLI headlessly, capturing structured events, stdout/stderr, exit status, duration, and the exact input provenance. Also support a deterministic fake harness for tests.
4. **Concurrent scheduling:** run N local workers subject to a bounded concurrency limit, cancellation and timeouts.
5. **Run and artifact lifecycle:** persist run metadata and useful output, recover tracked and untracked (non-ignored) source changes, then reclaim disposable workspaces with fail-closed deletion logic.
6. **Disk discipline:** require a configurable free-space reserve, preflight storage estimates, periodic usage checks, and an upper bound on Fabric-managed log/artifact growth. Stop admitting more tasks under pressure; evict eligible caches/workspaces before reaching the reserve.
7. **Inspectability:** CLI commands show project readiness, worker status, storage use, why an item is retained, and a dry-run cleanup plan.
8. **Extensibility:** backend, harness and artifact-store implementations sit behind narrow interfaces so later cloud execution, durable workers and event wakeups do not rewrite the domain model.

### Explicitly out of scope

- A hosted API, billing, web UI, provider OAuth, MCP provisioning and secret syncing.
- Multi-tenant isolation, hostile code sandboxing, container/VM security, network restrictions.
- Always-on background daemon, cross-host scheduling, webhooks, CI wakeups, agent-to-agent messaging.
- Transparent restore of live Linux processes or OMP extension memory.
- Automatic migration of local uncommitted project changes into remote workers.
- Guaranteed physical disk quotas on every filesystem. The PoC offers **best-effort proactive control** plus admission checks; hard enforcement requires a quota-capable execution backend.
- Arbitrary GitHub push/PR operations or unattended merging.

**Security boundary:** PoC executes **trusted local code as the current OS user**. It is not safe for mutually untrusted tenants. Document this prominently. Agents may still run arbitrary shell commands, and disk monitors cannot stop instantaneous writes before a filesystem quota is enforced.

## 2. Personas and principal use cases

**Primary user:** Developer with a working OMP installation and local Git repo, running `om-pstack` skills such as `/swarm` and `/arena`.

- **UC-1**: Prepare a project once (e.g. dependencies installed and build artifacts produced).
- **UC-2**: Delegate four distinct prompts to four independent OMP workers and obtain separate outputs.
- **UC-3**: Kill one worker; preserve what can be recovered, report failure, and leave no orphaned process silently claiming success.
- **UC-4**: Reclaim 20 completed workspaces and show that their useful outputs remain accessible.
- **UC-5**: Encounter low disk space; Fabric declines additional work and explains what can be safely reclaimed.
- **UC-6**: Restart CLI after a crash; mark interrupted runs for reconciliation without deleting unique changes.

## 3. User-facing command contract

Commands are proposed and become stable within the PoC once tested:

```sh
fabric project add --name my-project --path .
fabric project prepare my-project --recipe ./fabric.prepare.toml
fabric project status my-project
fabric doctor

fabric run my-project --harness omp --prompt-file prompts/review.md
fabric run-many my-project --harness omp --tasks tasks.json --concurrency 4

fabric runs list
fabric runs show <run-id>
fabric runs artifacts <run-id>
fabric storage status
fabric storage gc --dry-run
fabric storage gc --apply
```

`tasks.json` contains a list of `{ "id": "unique-task-id", "prompt_file": "..." }`. Each entry receives a separate writable workspace. `run-many` returns a batch ID and the individual Run IDs; with the PoC's foreground supervisor it waits for all tasks and returns nonzero if any fail. No detach or cloud persistence is promised.

**Preparation recipe** (`fabric.prepare.toml`):

```toml
version = 1
[[steps]]
argv = ["sh", "-c", "printf ready > .prepared-marker"]
# Real projects might run npm ci, uv sync, cargo build, etc.
[limits]
max_duration_seconds = 1200
```

Preparation executes commands as `argv` with explicit working directory, not shell-concatenated user data. Recipes are trusted code and may themselves invoke a shell intentionally.

## 4. Domain model and invariants

### Project
`project_id`, `name`, `source_path`, `default_ref`, `created_at`.

### PreparedState
`state_id`, `project_id`, `source_commit_sha`, `recipe_digest`, `harness_environment_digest?`, `prepared_path`, `status` (`PREPARING|READY|FAILED`), `created_at`, `verified_at`, `size_bytes`.

**Preparation key:** SHA-256 of `(source_commit_sha, recipe contents, preparation format version, selected environment inputs)`. No secret values appear in the key. Changing the recipe or commit produces a new state. Changing undeclared external dependencies is outside the automatic invalidation guarantee in v0.1.

### Run
`run_id`, `batch_id?`, `project_id`, `prepared_state_id`, `base_commit_sha`, `harness`, `harness_version?`, `prompt_digest`, `state`, `workspace_id`, `pid_metadata?`, `exit_code?`, `timestamps`, `error_kind?`, `artifact_manifest_id?`.

**Run states:** `QUEUED -> PREPARING -> RUNNING -> FINALIZING -> (SUCCEEDED | FAILED | CANCELLED | NEEDS_ATTENTION)`. State transitions are persisted atomically. Success is determined from process exit and required capture checks, never merely the model's last message.

### Workspace
`workspace_id`, `run_id`, `managed_path`, `state_id`, `state` (`ACTIVE|CLEANUP_PENDING|RECLAIMED|RETAINED|RECOVERY_REQUIRED`), `lease`, `usage_bytes`, `retention_reason?`.

### Artifact
`artifact_id`, `run_id`, `kind` (`stdout|stderr|events|tracked_patch|untracked_archive|manifest|summary`), `relative_path`, `sha256`, `size_bytes`, `created_at`.

### Critical invariants

- A Run ID remains stable after a Worker process or Workspace disappears.
- A prepared state is immutable once `READY` and never executed as the writable workspace.
- Every managed workspace has exactly one owning Run and a DB record **before** being used.
- A workspace cannot be deleted while its lease is valid or when the only copy of recoverable changes has not been safely captured.
- Artifacts are written to staging, fsynced/closed, checksummed, then atomically registered before cleanup becomes eligible.
- Cleanup only targets recorded Fabric-managed descendants of the configured storage root, with fresh path validation and a Fabric marker; never arbitrary user checkout paths.
- State-changing operations are idempotent after restart; partial failures become `NEEDS_ATTENTION` rather than silently dropping data.
- User secrets and OMP credentials are never copied into prepared states by Fabric automatically. For v0.1 local OMP inherits only explicitly selected environment variables (and its normal local config mechanism); the threat model remains trusted-local.

## 5. Architecture and Rust packages

Single Cargo workspace; do not split into networked services yet.

```text
fabric/
  Cargo.toml
  crates/
    fabric-domain/         # typed IDs, states, policies, events, errors; no I/O
    fabric-store/          # SQLite migrations, atomic state and artifact records
    fabric-project/        # source resolution, recipe hashing, preparation
    fabric-workspace/      # fork strategies, usage metering, checkpoints, GC
    fabric-runner/         # process supervisor, OMP adapter, fake adapter
    fabric-core/           # orchestration use cases, provider-neutral traits
    fabric-cli/            # clap binary `fabric`
  tests/fixtures/          # tiny Git fixture and fake agent process
  docs/                    # contracts, ADRs, threat model
```

Suggested crates: `tokio`, `clap`, `serde`, `serde_json`, `toml`, `rusqlite` (bundled SQLite), `uuid`, `sha2`, `tracing`, `thiserror`, `tempfile`, `assert_cmd`. Pin resolved versions via `Cargo.lock`. Avoid premature distributed frameworks or custom async executors.

Interfaces (illustrative):

```rust
#[async_trait::async_trait]
pub trait ExecutionBackend: Send + Sync {
    async fn start(&self, spec: ExecutionSpec) -> Result<ExecutionHandle, FabricError>;
    async fn cancel(&self, handle: &ExecutionHandle) -> Result<(), FabricError>;
}

#[async_trait::async_trait]
pub trait HarnessAdapter: Send + Sync {
    async fn probe(&self) -> Result<HarnessInfo, FabricError>;
    fn command(&self, spec: &AgentSpec) -> Result<CommandSpec, FabricError>;
}

pub trait WorkspaceBackend: Send + Sync {
    fn fork(&self, state: &PreparedState, run: RunId) -> Result<Workspace, FabricError>;
    fn meter(&self, workspace: &Workspace) -> Result<Usage, FabricError>;
    fn reclaim(&self, workspace: &Workspace) -> Result<(), FabricError>;
}
```

These are **ports**, not an instruction to implement unused cloud adapters. In-process implementations only. Data types should carry stable IDs and structured failure reasons.

### Persistence

SQLite in WAL mode; schema migrations; foreign keys on; transactions around run/workspace creation and transition. Logs/artifacts stored under Fabric's root (`$XDG_DATA_HOME/fabric` by default), with DB holding checksums/locations. Use owner-only permissions where supported. One local process can coordinate a batch; a later daemon will reuse the same domain/store API.

### Prepared-state creation

Resolve the exact source commit, create a managed local clone/checkout into a staging directory, run recipe in that directory, verify output and record the final source SHA, then publish a content-addressed READY state. Do not use the developer's actual working tree as writable state. If the source checkout is dirty, print a warning: only committed HEAD enters this PoC, and no local uncommitted changes are uploaded or silently included.

### Workspace fork strategy

`WorkspaceBackend` has a `reflink` implementation where the local filesystem supports copy-on-write and a `copy` fallback **only after explicit size admission**. Never use hardlinks for writable file contents. Clone the **whole prepared state**, including ignored dependency files, into a private path. No duplicate install/prepare scripts may run at worker startup. Initially Linux-only; a later overlay/snapshot backend can replace copying without changing the Run model.

**Pragmatic note:** reflink is unavailable on some common filesystems; correctness tests run against the size-limited copy backend on CI. Performance targets are measured separately only on supported CoW filesystems. This PoC must not claim instant forks on arbitrary disks.

## 6. Agent execution and concurrency

OMP adapter:

- Probe the installed binary with `omp --version`; pin actual version in run metadata.
- For headless execution, invoke OMP with the supported `-p --mode json` interface, supplying the prompt safely via a file/stdin or one argument rather than concatenating a shell string. The CLI reference documents JSONL session/events and nonzero exits on terminal failures.
- Start each process in its own process group on Linux; set `cwd` to its workspace. Stream bounded stdout/stderr to managed files, parse JSONL events when possible, and preserve raw bounded logs for diagnostics.
- Enforce a per-run wall-clock timeout and configured concurrency limit; cancellation sends TERM, waits a grace period, then KILL to the group. Do not kill reused PIDs based on an unverified stale DB record.
- Bound model spending only when OMP supports an appropriate flag; track `max_cost_usd` as a best-effort harness control, not a guaranteed cloud billing cap.
- Use a **fake harness process** for automated CI. Real OMP smoke test requires installed OMP and configured model credentials and is opt-in.
- For v0.1 foreground runs, unexpected supervisor death may require reconciliation. It is **not** a durable remote execution system.

## 7. Storage policy and safe reclamation

Config example:

```toml
[storage]
root = "~/.local/share/fabric"
max_managed_bytes = 21474836480  # 20 GiB
min_free_bytes = 5368709120      # 5 GiB free-space reserve
max_workspace_bytes = 2147483648  # 2 GiB per worker
max_log_bytes_per_run = 10485760  # 10 MiB (truncate with marker)
max_artifact_bytes_per_run = 104857600 # 100 MiB
on_success = "checkpoint_then_discard"
on_failure = "checkpoint_then_retain_if_unsafe"
recovery_retention_hours = 24
```

Algorithm: before launch, reconcile stale records, assess current disk/free-space and predicted fork cost; evict eligible **rebuildable caches** first, then reject if reserve cannot be maintained. While running, meter usage periodically, warn near threshold, stop/terminate offending run if necessary, and capture recovery artifacts if space permits. **This is best effort**: for hard multi-tenant disk isolation later, use filesystem/project quotas inside isolated sandboxes.

On completion: capture `git diff HEAD --binary` (tracked edits including staged changes) and **non-ignored untracked** files as a size-bounded archive with a manifest. Do not follow symlinks out of workspace. Ignored files are treated as rebuildable by default; if a user wants to preserve them, it must be explicit in a later retention contract. If outputs exceed cap or capture fails, keep the workspace as `RECOVERY_REQUIRED` and explain why. Never silently discard recoverable changes. After successful checksum verification, mark cleanup eligible and reclaim only the private fork. Keep the prepared base until cache policy evicts it; never remove an active state's last required copy.

GC procedure: obtain exclusive cleanup lease, re-read DB row, check no valid process/worker lease, canonicalize and validate root/path ownership, verify artifact manifest, atomically record cleanup intent, delete managed path, then mark reclaimed. Repeat safely after interruption. A path outside root, symlink surprise, unknown directory, active lease or missing artifact manifest is a hard **refusal**, not a force-delete.

## 8. Verification and acceptance tests

All CI tests run offline, without model credentials, against a temporary Git repository and fake harness.

- **AT-01 preparation reuse:** two requests for unchanged `(commit, recipe)` produce same state ID; recipe's side-effect counter increments once. Changing recipe or commit creates a distinct state.
- **AT-02 prepared dependencies:** recipe creates ignored `.deps/ready`; every fork contains it without running preparation again.
- **AT-03 isolation:** two parallel workers write same relative file to different content; base and sibling remain unchanged.
- **AT-04 fan-out:** four fake workers run with concurrency two; peak active processes never exceed two; all have individual Run IDs/artifacts.
- **AT-05 provenance:** saved manifest includes source SHA, recipe digest, prompt digest, harness version and observed exit status.
- **AT-06 outputs:** tracked modifications and untracked non-ignored files survive as verified artifacts even after workspace deletion.
- **AT-07 safe failure:** force artifact capture to fail; worker becomes `NEEDS_ATTENTION`/workspace `RECOVERY_REQUIRED`, and its directory remains intact.
- **AT-08 timeout/cancel:** fake worker spawns a child process and waits; supervisor cancels process group, records outcome and no child continues writing.
- **AT-09 no unsafe deletion:** tamper with managed path or replace it with a symlink; GC refuses deletion and never touches an outside sentinel directory.
- **AT-10 low space:** injected storage meter reports insufficient headroom; run is rejected before starting any worker, with actionable error.
- **AT-11 GC idempotence:** interrupt GC after intent and rerun; final state is consistent, no second deletion of unrelated data.
- **AT-12 crash recovery:** mark active run with expired lease and ensure reconciliation preserves workspace until recoverability is determined.
- **AT-13 bounded logs:** fake harness emits 100 MB; retained log obeys cap and includes explicit truncation marker.
- **AT-14 deterministic CLI:** `doctor`, `project status`, `runs show`, `storage status`, GC dry-run produce stable machine-readable `--json` variants.
- **AT-15 manual OMP smoke:** on a developer machine, a real OMP run sees prepped files, returns JSON events and produces a preserved patch. Not a required CI check.

**Acceptance demo:** Run AT-01 through AT-14 plus `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and README CLI smoke against a fixture repo. Run one real OMP smoke before calling the PoC demonstrated. Never mark a story complete solely because its implementation looks plausible.

## 9. What the PoC is intentionally proving — and not proving

Success means Fabric has a viable **agent-aware workspace lifecycle and local orchestration** model with reusable project state, isolated forks, inspectable outputs and safe GC.

It does **not** demonstrate cloud latency, sandbox security across tenants, true hard disk quotas on arbitrary filesystems, durable wakeups, exact compatibility with every OMP extension, or one-to-one pstack parity. Those are subsequent phases. The initial adapter will make replacing its process runner possible.

## 10. Extension roadmap and required seams

### Phase 2 — pstack local integration
Add OMP extension (TypeScript) exposing `fabric_spawn`, `fabric_status`, `fabric_results`, `fabric_cancel`; map `swarm` and `arena` requests to local execution. Do not assume all OMP `task` fields are identical to Cursor's.

### Phase 3 — self-hosted cloud workers
Create a Rust worker daemon and central control-plane API. Replace `LocalExecutionBackend` with remote execution implementation; persist worker heartbeats, use secure sandbox/VM isolation, project-state distribution, tenant-aware storage quotas, and remote artifact store. Preserve Run/Workspace IDs and event schema.

### Phase 4 — durability and signals
Add queued jobs, detach, checkpoint/resume at safe agent-turn boundaries, GitHub/webhook/timer signals, secure token broker, approvals, and PR publication. Run IDs must remain valid as underlying sandbox IDs change.

### Phase 5 — cloud readiness and economics
Build content-addressed preparation cache, CoW-backed snapshots, cache-aware placement, autoscaling, billing/usage reports, and enterprise controls. Measure time-to-first-useful-command and storage reclaimed per completed agent task.

## 11. TDD/Ralph implementation protocol

1. Work from `FABRIC_POC_SPEC.md`, an ordered `PRD.json` and `RALPH_PROMPT.md` checked into the repository. These are the requirements source of truth.
2. Each loop iteration selects **one** uncompleted task whose dependencies are done. Do not implement the next task until its acceptance tests pass.
3. Write a failing test first; run it and observe expected failure; implement minimum production change; refactor; rerun focused and full required checks.
4. The implementer must not edit the specification, acceptance tests intended as fixed external gates, or change acceptance criteria to make implementation pass. Operator-owned gates should be reviewed separately. Tests developed as part of the story can be added but should not weaken the spec.
5. Use a fresh context for each story; persist progress in Git commits, a small progress log and task state. Prefer an **external verifier** to mark a story passed after independent checks. Do not trust a self-reported completion token.
6. Limit each iteration to one task and a finite tool/time budget. If blocked, write blocker/evidence and stop rather than looping forever.
7. No push, merge, deploy, deleting arbitrary directories or spending on model calls as part of verification without explicit operator approval.

See `PRD.json` for ordered stories and `RALPH_PROMPT.md` for the agent instruction.
