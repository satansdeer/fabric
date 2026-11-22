# Fabric

A local-first Rust PoC for reusable agent workspaces. **Trusted local code only:** Fabric is not a sandbox or multi-tenant security boundary. macOS is the primary supported platform; Linux and other platforms are not currently claimed as supported.

## Implemented: FAB-01 through FAB-20 (automated scope)

The seven-package Cargo workspace implements local diagnostics, an I/O-free domain model, SQLite persistence, pinned local Git project registration, validated preparation recipes/fingerprints, reusable private prepared snapshots, full-tree forks, durable run-owned workspaces and foreground fake/OMP harness workers. Successful preparation publishes a durable read-only READY tree, including ignored dependencies. Runs publish durable tracked and nonignored untracked artifacts before lease-aware, path-safe reclaim; `storage gc` reports and retries eligible cleanup.

Requires Rust 1.85 or newer. Resolved dependencies are pinned in `Cargo.lock`.

Native prerequisites: macOS, Xcode Command Line Tools (`xcode-select --install`) for Git/linking, and a Rust toolchain with rustfmt/clippy. CI uses Rust 1.91 on `macos-latest`. APFS supports native `clonefile` full-tree forks; other local filesystems require an explicitly admitted ordinary copy. SQLite is bundled. OMP is optional for deterministic tests and must be installed/configured separately for an approved model run. No Linux support or sandbox isolation is claimed.

**AT-15 is NOT EXECUTED.** Automated acceptance is not model-demonstrated acceptance; the project is not declared `POC_READY_FOR_REVIEW`.

```sh
cargo build --locked -p fabric-cli
./target/debug/fabric doctor
./target/debug/fabric doctor --json
# Or run directly from the workspace:
cargo run --locked --bin fabric -- doctor --json
```

`doctor` reads the current platform and searches `PATH` for executable `git` and `omp` files (including executable symlinks). It does not execute either tool, probe versions, invoke a model, access the network, read credentials, or create/change Fabric storage or user data. Missing tools are diagnostics, not a command failure. A missing `PATH` discovers no tools; relative or empty entries in an explicitly set `PATH` refer to the current directory.

The JSON report has stable field names and tool ordering:

```json
{"platform":{"os":"macos","arch":"aarch64","supported":true,"primary_supported_os":"macos"},"tools":[{"name":"git","path":"/usr/bin/git"},{"name":"omp","path":null}]}
```

`platform.supported` describes the current OS against the macOS target, not readiness of unimplemented lifecycle features. Tool paths are executable candidates, not a guarantee that launching the tool will succeed; `null` means no executable candidate was found. Paths and architecture vary with the host. Diagnostics produce identical output for identical platform and filesystem/PATH inputs. Unsupported platforms still receive a diagnostic report with `supported: false`.

Successful reports go to stdout with exit status 0. Invalid arguments with `--json` produce a structured `{"error":{"kind":"invalid_arguments","message":"..."}}` on stderr and exit status 2. Output failures use error kind `output` and a nonzero status. Help and version retain standard CLI text output.

### Register a local Git project

```sh
./target/debug/fabric --root /private/path/to/fabric project add --name my-project --path /path/to/repo
./target/debug/fabric --root /private/path/to/fabric project status my-project
./target/debug/fabric --root /private/path/to/fabric project status my-project --json
```

Without `--root`, storage uses an absolute `$XDG_DATA_HOME/fabric`, otherwise `$HOME/.local/share/fabric`. A relative/empty XDG value is ignored; absent absolute HOME requires `--root`. New storage directories are owner-only (0700 on Unix); an existing root with group/other permissions or a symlink root is refused, not silently changed. Choose a dedicated private directory. `doctor` never opens storage, even when a root is supplied.

Registration accepts a working tree or its subdirectory, including detached HEAD, and stores the canonical repository root and full committed HEAD SHA. The existing schema's `default_ref` holds this exact SHA, not a moving branch. Status reads persisted metadata without consulting the checkout; JSON contains `project_id`, `name`, `source_path`, `default_ref`, `source_commit_sha`, `created_at` (Unix seconds), and `prepared_states` with READY/failure status and provenance. The registration SHA stays pinned after HEAD moves or the source directory becomes unavailable; each prepared state records its own source SHA. Project names are nonblank strings and unique within one root.

Tracked, staged or non-ignored untracked edits produce a stderr warning: only committed HEAD is pinned; uncommitted changes are not imported. Ignored files are not imported either. Registration copies no source files, changes no checkout/index, runs no recipe and makes no network request. Git uses argument vectors with inherited Git/config/credential environment removed, fsmonitor disabled, optional index writes disabled and lazy fetching disabled. Source paths must be valid UTF-8 for SQLite storage. Missing/non-Git/bare paths and unborn HEAD fail with actionable errors. `--json` works for add/status and structured failures (`source`, `storage`, `not_found`); successful dirty add still emits its warning on stderr.

Keep the source repository and its current committed HEAD available for preparation: registration records historical identity only, not a durable source snapshot. Each prepare request resolves current HEAD once; it does not rewrite or permanently reuse the registration SHA. External removal of that resolved commit makes preparation fail, never fall back to a different revision.

### Preparation recipe contract

`fabric_project::recipe::Recipe::parse` accepts trusted version-1 TOML. Parsing performs no I/O and starts no command. `project prepare` reads and validates the recipe before starting any Git or recipe process.

```toml
version = 1
environment = ["TOOLCHAIN", "BUILD_TOKEN"] # optional fingerprint inputs; names only
[[steps]]
argv = ["sh", "-c", "printf ready > .prepared-marker"]
[limits]
max_duration_seconds = 1200
```

At least one step and a nonblank executable are required. Arguments remain separate and literal (including empty arguments); NUL bytes are rejected. Shell syntax works only when a recipe explicitly invokes a shell. Unknown fields at every level, unsupported versions, wrong shapes/types, duplicate/invalid environment names and nonpositive/out-of-range duration values fail with `invalid_recipe` before any execution. Environment names use `[A-Za-z_][A-Za-z0-9_]*`. Omitted environment defaults to empty; omitted limits or duration defaults to 1200 seconds.

`Recipe::fingerprint(full_commit_sha, &environment_snapshot)` uses only explicitly declared names from the caller-supplied UTF-8 string map, never the ambient environment. Unset differs from an empty value. Both 40- and 64-digit Git SHAs are accepted and normalized to lowercase. Undeclared environment changes do not invalidate the key; changing commit, argv (including boundaries/order), limits, declared names or their values does. Comments, TOML key order, declaration order and explicitly supplied defaults do not change semantic identity.

Fingerprint format 1 hashes compact UTF-8 JSON directly into SHA-256, prefixed by a domain string and NUL: recipe domain `fabric.recipe.v1` serializes fields in order `version`, `steps` (each `argv`), `limits` (`max_duration_seconds`), `environment` (sorted names); environment domain `fabric.environment.v1` serializes the sorted selected name-to-value/null map; preparation domain `fabric.preparation.v1` serializes `[format_version, lowercase_commit_sha, recipe_digest, environment_digest]`. Digests are lowercase hexadecimal. A layout/encoding change requires a format-version bump.

Only commit, format version and digests are returned as persistable/displayable fingerprint metadata; raw environment values are streamed to the hasher without a serialized buffer. Parser errors omit TOML source excerpts. Do not place secrets in recipe argv or literal recipe content; recipes remain trusted code, not a secret store. External dependencies outside declared inputs are not automatically invalidated.

### Prepare a private snapshot

```sh
./target/debug/fabric --root /private/path/to/fabric project prepare my-project --recipe ./fabric.prepare.toml
./target/debug/fabric --root /private/path/to/fabric project prepare my-project --recipe ./fabric.prepare.toml --json
```

Preparation resolves exact current committed HEAD once per request and warns about dirty tracked/staged/non-ignored files before executing. It imports committed objects only, never working edits, untracked files, or existing ignored dependencies. The registered SHA reported by `project status` remains unchanged. A private local clone uses Git transport (`--no-local --no-hardlinks`), an empty template, disabled hooks/global/system configuration, no object alternates or shared Git metadata, and detached checkout of the resolved SHA. Fabric does not modify the developer's checkout/index.

The tree is staged under the owner-only `prepared/<project-id>-<preparation-key>/tree`, with a private `.fabric-owner` marker in its parent and a committed PREPARING DB record before use. Commands receive literal argv and that explicit cwd, with only the recipe's declared environment snapshot, a fixed `/usr/bin:/bin` default PATH, and Git global/system-config suppression. Declared PATH may select installed project tools; HOME and credentials are not inherited unless explicitly declared. Fabric's Git operations use macOS `/usr/bin/git`. Recipe stdout/stderr and Git stderr are intentionally discarded until bounded diagnostic logging is implemented; captured Git metadata is capped at 64 KiB.

One total wall-clock deadline covers source inspection, clone, all recipe steps and verification. Each command has a private macOS process group; timeout/error kills the group and reaps its leader. Background descendants are also killed when their leader exits, so steps must finish their own background work. Trusted recipes are not sandboxed and must not intentionally escape the group or write outside cwd.

Before READY, Fabric rechecks ownership, private Git metadata and final HEAD, measures every file/link including ignored `.deps/ready`, fsyncs files/directories/marker, and removes write/group/other permissions from regular files and directories. Relative symlinks are preserved only when their complete target chain resolves inside the prepared tree; Fabric never opens/chmods through them and syncs their containing directories. Absolute/escaping/dangling/cyclic links, special files, hardlinked files, shared Git metadata or altered ownership markers fail closed. READY is published only by the final durable SQLite transition; a directory alone is never proof of readiness. The base must never be used as a writable worker directory.

Unchanged preparation keys reuse the same READY state ID without rerunning any recipe step. Before reuse, Fabric checks the exact expected managed path, private real directories, ownership marker, read-only tree, safe internal symlinks, recorded size, private Git metadata and recorded source HEAD; invalid caches fail closed without mutation or deletion. Changing committed HEAD, recipe semantics or declared environment values creates a separate state; dirty source edits and undeclared environment changes do not.

Requests for the same project/key serialize across processes using a private kernel file lock held through validation and the final DB transition. Waiting consumes the request's total deadline. Lock files under `preparation-locks/` are permanent coordination inodes, not stale-PID leases; do not remove them while Fabric may be running. Process death releases the lock automatically. Different keys do not share a preparation lock.

Failures retain staging and mark the state FAILED, never READY. After acquiring the key lock, a repeat request reconciles an interrupted PREPARING record to FAILED, even if staging looks complete or was never created. No staging is deleted, resumed or promoted. **FAILED keys are terminal:** recipe side effects may already have happened, so repeating the identical request refuses safely without rerunning or overwriting evidence. For a deliberate new attempt, change the recipe, committed source commit or declared environment input; changing a recipe comment alone does not change its semantic fingerprint. READY metadata is also never rewritten. JSON reports state/project IDs, key, exact source SHA, recipe/environment digests, prepared path, status, timestamps and measured size; environment values are not persisted.

### Fork a prepared tree (library prerequisite)

`fabric_workspace::WorkspaceBackend` is a narrow synchronous fork port with one concrete `LocalWorkspaceBackend`. Construct it with a private existing Fabric root and `ForkStrategy::CloneOrCopy` (macOS `fclonefileat`, the file-descriptor clonefile CoW API) or `ForkStrategy::Copy` to force ordinary copying. `fork(&ready_state, &workspace_id, copy_limit_bytes)` returns a `PreparedFork` with its staging tree path, measured logical bytes and actual cloned/copied regular-file counts. No Git checkout, recipe or setup command runs during a fork.

Every request freshly validates the persisted READY record, its exact managed source path/owner marker, read-only ownership/permissions, private Git, recorded HEAD/size and internal relative symlink chains. It copies the **entire** tree, including `.git`, ignored dependencies and empty directories, into a newly created owner-only `fork-staging/<workspace-id>/tree`. Existing destinations, symlink/permissive directory surprises, foreign ownership, hardlinked/special source files and absolute/escaping/dangling/cyclic links are refused. Regular files receive independent inodes and owner read/write permissions; owner executability is preserved, directories become 0700, and links are reproduced without following or chmodding their targets. READY permissions/content are never changed.

Ordinary copy requires explicit admission of the **whole measured logical tree size**, before creating a forced-copy destination or writing a clone fallback. Clone fallback is allowed only for unsupported/cross-filesystem clone operations, not arbitrary I/O errors. A zero allowance disables copying; CoW may still succeed. This low-level copy allowance is not a free-space check or hard quota; managed CLI/core execution additionally applies the storage admission and monitoring below. Copy is the deterministic correctness-test path; no instant-fork performance claim is made on arbitrary filesystems.

Calling this low-level backend directly produces **unregistered staging forks**, not runnable managed workspaces. Failed or partial destinations remain as evidence and are never overwritten or deleted; their IDs cannot be retried. Use the ownership orchestration below before any future worker launch. Directory existence, a returned `PreparedFork`, and a caller-supplied workspace ID never authorize execution or deletion.

### Register an owned workspace (library use case)

`fabric_core::workspace::create_workspace(root, backend, &request, copy_limit_bytes)` reuses the existing local backend and SQLite store. `WorkspaceRequest` supplies stable Run/Workspace IDs, a persisted prepared-state ID, harness name, prompt digest, creation time and startup reservation expiry (Unix seconds). The Run/Workspace pair commits atomically before the backend can touch its destination; reciprocal foreign keys, unique Run/Workspace IDs and a unique managed path enforce exactly one owner. The Run enters PREPARING, not RUNNING: no agent process is launched.

The same `fork-staging/<workspace-id>/tree` becomes managed only after successful forking, an owner-only fsynced container `.fabric-owner` binding the canonical root and Run/Workspace/prepared-state IDs, and a durable fork-completion update. `RunnerWorkspace` has private fields and is returned only after fresh ownership/path/marker validation. `runner_workspace(root, &run_id)` reopens the store and validates again; use it immediately before future process launch. Unknown roots never get a new database here. Missing/incomplete/failed ownership, outside or non-exact recorded paths, root/database/directory/marker symlinks, hardlinked markers/databases, permissive managed metadata and incorrect markers are refused without deleting anything. Safe relative links inside project contents remain supported; neither this validator nor the backend is a same-user adversarial sandbox.

Fork failures atomically retain the stable Run as NEEDS_ATTENTION with `workspace_fork` diagnostics and its Workspace as RECOVERY_REQUIRED. Existing/partial files and reservation leases remain intact; no retry overwrites or adopts them. An interrupted pending record is not runner-ready, even if a complete tree happens to exist. No process states or exit codes are fabricated.

`lease_expires_at` starts as a persisted **startup reservation** before forking. Missing/expired startup reservations cannot yield a runner-ready workspace. Durable worker ownership, renewal and crash reconciliation are implemented separately: neither this timestamp nor PID metadata proves liveness or death. Successful complete capture clears the reservation; a future lease still refuses reclaim. Expiry alone never authorizes cleanup.

`fabric-domain` uses opaque caller-supplied IDs (1–128 ASCII letters, digits, `_` or `-`); it does not generate IDs or perform filesystem operations. State wire names follow the specification. Run transitions follow the queued/preparing/running/finalizing graph, with PREPARING → NEEDS_ATTENTION for fork failures before execution; terminal states cannot be rewritten. Repeating a state is a no-op. Workspace transitions alone never authorize deletion: core additionally verifies ownership, leases, exact managed paths and complete durable recovery artifacts.

`StoragePolicy::default()` uses the specification's 20 GiB managed limit, 5 GiB free reserve, 2 GiB workspace limit, 10 MiB logs, 100 MiB artifacts and 24-hour recovery retention. Limits must be positive, combined per-run caps must fit the managed budget without overflow, and retention hours must fit in seconds. The reserve is independent of the managed budget. JSON deserialization applies defaults and rejects invalid limits, unknown fields and unsafe/unknown retention mode names; direct field edits require `validate()`. These are best-effort policies, not disk quotas or permission to discard uncaptured changes. Storage-root validation remains an I/O-layer responsibility.

`fabric-store::Store::open` opens a file-backed SQLite database with WAL, full synchronous commits, foreign keys, a five-second busy timeout, and transactional versioned migrations. Its parent directory must exist; new database files have owner-only permissions on Unix. Unsupported schema versions fail closed. Typed project, prepared-state, run and workspace records survive reopening. Prepared-state keys are unique per project, READY metadata cannot be updated, and runs require READY state with matching project/commit provenance. Run/workspace rows have reciprocal one-to-one foreign keys and unique managed paths; both commit before the caller may use a workspace. Atomic state compare-and-set validates the domain transition graphs, rejects stale competing intent, and accepts exact replay without rewriting timestamps.

The store itself does not resolve Git, prepare files, launch processes, validate canonical filesystem ownership, authorize deletion, or reconcile interrupted workers. Core validates private roots, managed ownership and artifact files before registration and deletion. Stored managed-path uniqueness is textual; core requires the exact canonical managed descendant and matching private marker. Lease expiry and lifecycle state alone never authorize cleanup. Byte counts must fit SQLite's signed 64-bit integer range. No cloud adapters are implemented.

### Run one deterministic external worker

```sh
cargo build --locked -p fabric-cli --bins
./target/debug/fabric --root /private/path/to/fabric run my-project \
  --harness fake --prompt-file ./task.json --copy-limit-bytes 104857600 --json
```

The fake harness is an actual separate `fabric-fake-harness` executable beside `fabric`, not a mocked return value. `--fake-binary /absolute/path` selects another compatible fixture executable. Fabric probes its actual `--version`, then passes the private prompt snapshot as literal `--prompt-file PATH` argv without a shell. The process gets only fixed `PATH=/usr/bin:/bin` and `LC_ALL=C`, no inherited HOME/credentials. Its cwd is freshly revalidated owned fork, never the registered source or prepared base. Current committed source HEAD selects the most recently verified READY state for that commit (state ID breaks timestamp ties); historical registration pins and dirty working edits are not execution inputs. There is no recipe selector; cached preparation reuse does not change verification order. A missing READY state refuses with a prepare-first diagnostic.

`task.json` is a UTF-8 JSON object with optional `required_files` (relative paths to readable prepared files), `writes` (`{"path":"tracked","content":"new text"}` entries), `events` (JSON values emitted as JSONL), `absent_environment` (environment names that must be absent), and `exit_code` (0–255, default 0). Unknown fields and unsafe/absolute/parent/symlink fixture paths fail. For example:

```json
{"required_files":[".deps/ready"],"writes":[{"path":"tracked","content":"changed"},{"path":"new.txt","content":"untracked"}],"events":[{"type":"fixture","value":42}],"exit_code":7}
```

The core persists PREPARING before fork work, RUNNING with the actual child PID/version/start time after spawn, FINALIZING with the observed exit, and only after verified artifact publication commits SUCCEEDED, FAILED or CANCELLED with the manifest association and completion timestamp. Prompt identity is SHA-256 of the exact copied input bytes. JSON stdout reports IDs, source SHA, prompt digest, harness version, terminal state, observed exit code/signal, stop reason (`error_kind`), times and `artifact_manifest_id`. A timeout is FAILED with `timeout`; cancellation is CANCELLED with `cancelled`, even if a cooperative harness exits zero. Non-success makes the CLI fail after emitting its durable report. Launch/supervision/capture uncertainty instead records NEEDS_ATTENTION and RECOVERY_REQUIRED, retaining evidence without inventing exits. SQLite schema v5 adds immutable artifact registrations and terminal manifest associations; older schemas upgrade without rewriting prior evidence.

**Source artifacts are durable before reclaim.** A streamed binary Git diff against the pinned launch HEAD captures the combined staged and unstaged tracked result (including staged additions). A restorable GNU tar archive captures Git's NUL-delimited nonignored untracked paths, exact file bytes, ordinary permission bits, directories and safe relative symlinks. Ignored dependencies stay out of this archive. The version-2 manifest covers the archive, exact `prompt.input`, bounded stdout/stderr/events, Git capture diagnostics, IDs, source SHA, recipe/prompt digests, harness/version, observed exit/stop reason and output truncation metadata. Files are privately staged, synced, SHA-256 verified, sealed read-only, and atomically promoted with no overwrite before a single SQLite artifact-registration/terminal-completion transaction. `--max-artifact-bytes N` defaults to 100 MiB per run, counting all promoted bytes including tar headers, path listings and the manifest; exceeding it fails capture rather than publishing partial source evidence. Git capture has its own 60-second bound and is not cancelled by the worker's cancellation token.

**Verified capture marks the workspace CLEANUP_PENDING, then invokes the same safe reclaim path as GC.** Successful, failed-exit, timeout, cancellation and batch runs share this pipeline. Deletion and parent-directory sync precede RECLAIMED; artifacts remain independently recoverable. JSON includes a `cleanup` result; refusal leaves the completed Run outcome unchanged and the workspace available for inspection/retry. Failed publication/registration, oversize data/metadata, unsafe links, hardlinks, special files or observed path/type changes preserve the workspace and staging/promoted evidence as NEEDS_ATTENTION / RECOVERY_REQUIRED, without a registered incomplete manifest. Source reads are descriptor-relative and refuse symlink ancestors; external or chained links fail closed without reading their targets. Reservations and expiry never authorize deletion. Output/artifact/wall-time bounds do not impose workspace, memory or spending quotas. Atomic artifact publication currently supports macOS; other platforms fail closed and retain recovery evidence.

### Inspect or reclaim captured workspaces

```sh
fabric --root /private/path/to/fabric storage gc --dry-run --json
fabric --root /private/path/to/fabric storage gc --apply
```

Choose exactly one mode. Reports are sorted by workspace ID, with `action`, stable `reason`, and diagnostic `detail`; unknown fork-staging entries are reported separately and never adopted. Dry-run opens validated private regular DB/WAL/SHM paths in SQLite read-only mode, seeing committed WAL updates from live writers rather than a stale immutable snapshot. It never migrates the schema, logically writes database data, records an intent, changes workspace/artifact data or permissions, or deletes managed files. Native SQLite lock/shared-memory coordination may create or update its normal private WAL/SHM sidecars, even when the database was initially closed; this is not a zero-I/O inspection contract. Competing cleaners refuse `cleanup_lease_busy`.

Under an exclusive kernel cleanup lease, GC rereads SQLite and requires a terminal Run, CLEANUP_PENDING without retention or a live lease, exact canonical managed ownership, private real containers/tree, and a complete checksummed registered manifest matching provenance. Outside/nonexact recorded paths, missing/mismatched markers, symlink containers/tree, missing/corrupt artifacts and active/recovery/retained workspaces are explicit refusals. A durable immutable SQLite intent binds directory and marker identities before deletion. Descriptor-relative no-follow traversal unlinks internal symlinks without following targets; it refuses foreign ownership and mounted filesystems. Interrupted cleanup can resume with partial trees, a missing marker or a missing container only against that saved authority; substitutions fail closed. Ownership records and intents remain after deletion. No PID is deletion authority. READY-cache eviction and expired-lease worker reconciliation remain FAB-18/FAB-19.

Cleanup leases explicitly unlock on scope exit, so unrelated forked helpers cannot prolong a completed cleanup by retaining an inherited descriptor.

### Interrupted workers and recovery

Schema v8 persists each worker's owner, expiry and private lock-file identity **before** reserving storage, including the pre-Run crash window. Preparation startup, worker admission and GC `--apply` reconcile under the root cleanup lock; read-only status and GC dry-run do not reconcile. Expiry is diagnostic, never death or deletion authority. An inherited kernel lock protects orphaned workers; macOS also compares native PID/start-time identity and inspects the original process group, protecting live workers even after expiry or closing the inherited descriptor. Persisted PIDs are never signalled: only the existing live foreground supervisor may signal its own process group.
Normal foreground completion durably releases ownership and explicitly unlocks it, so unrelated concurrent forks cannot prolong a finished worker's reservation. Dropping an unreleased owner never explicitly unlocks an orphan worker's inherited descriptor.

When that authority proves the owner and worker inactive, stale reservations are released before admission. Interrupted nonterminal Runs become NEEDS_ATTENTION and their workspaces RECOVERY_REQUIRED, retaining original files, observed exit/capture metadata and registered artifacts without inventing an exit or success. Inspect the retained workspace and Run before deciding how to recover; GC will not delete it. Fully captured terminal Runs keep their recorded outcome and may resume the existing verified cleanup intent. Repeating reconciliation changes neither evidence nor outcomes.

Missing, replaced or unverifiable authority, a crash between launch intent and durable worker identity, unsupported native inspection, and pre-v8 reservations without reliable ownership remain conservatively retained. These require operator recovery rather than hidden PID-based killing or automatic deletion; live/unverifiable workers continue to consume reserved headroom.

### Storage budgets and usage

```sh
fabric --root /private/path/to/fabric storage status --json
fabric --root /private/path/to/fabric project prepare my-project --recipe ./fabric.prepare.toml \
  --max-managed-bytes 21474836480 --min-free-bytes 5368709120 --max-workspace-bytes 2147483648
fabric --root /private/path/to/fabric run my-project --harness fake --prompt-file ./task.json \
  --max-managed-bytes 21474836480 --min-free-bytes 5368709120 --max-workspace-bytes 2147483648 --json
```

`project prepare`, `run`, `run-many` and `storage status` accept these three byte flags with the defaults shown. Worker log/artifact caps retain their existing flags. Overrides are per invocation, not persisted configuration. Status is a read-only inspection with the same native SQLite coordination-sidecar caveat as GC dry-run; its JSON includes policy, free space, managed logical usage, outstanding admission reservations and per-cache/workspace usage, retention reasons and measurement diagnostics. A missing or unsafe retained path reports unknown usage (`null`), not invented zero; safely reclaimed workspaces report zero with artifacts preserved.

Before fresh preparation or worker admission, Fabric takes the existing root cleanup lock, meters available filesystem space and logical managed bytes, resumes durable cache eviction intents, and attempts verified eligible workspace cleanup and safe cache eviction before rejecting with `storage_admission`. Repeated preparation resolves the same recipe/environment/commit identity and validates an existing READY cache under its key lock before any fresh-tree admission: reuse keeps its state/tree identity without rerunning setup or demanding another workspace's headroom. Worker admission reserves the whole workspace cap plus log and artifact caps atomically across concurrent workers/processes; fresh preparation reserves a workspace-sized estimate while holding the root lock. Existing reservations are conservatively deducted from free headroom and added to current root usage even as workers materialize files. This may reject earlier than physical space requires. A second check immediately before process launch rejects newly observed pressure without starting the worker.

Cache eviction preserves every historical Run/Batch/Workspace reference, admitted worker and latest READY selection for each project/commit. Only freshly validated unreferenced older READY caches can be removed. Under the root cleanup lock and the existing per-key preparation lock, an immutable descriptor-identity intent and transactional removal of READY metadata precede deletion. Interruption resumes against that authority; an evicted identical key can subsequently be prepared again. Unknown paths, substitutions, invalid caches and recoverable/uncaptured workspaces are never adopted or deleted. Lease expiration alone grants no deletion permission.

Each worker has a bounded no-follow workspace/root scan, polled at 100 ms between scans, with a warning near its workspace limit. Scans stop at 200,000 entries, depth 64 or two seconds per tree; an unreadable/unbounded measurement fails closed as `storage_meter_failed`. Concurrently vanished entries are skipped rather than mistaken for unsafe reads. Workspace overflow stops only that worker's owned process group as `workspace_limit`; root/free-space pressure uses `storage_pressure`. Existing TERM/grace/KILL supervision and capture are reused. These reasons mean FAILED even for cooperative zero exits, never user cancellation; capture failure retains the reason and original workspace as NEEDS_ATTENTION/RECOVERY_REQUIRED. Final measurement also catches fast zero-exit oversize workers.

Accounting uses logical file lengths including ignored dependencies and link text without following links, not APFS-exclusive allocated bytes. Free space uses native `statvfs` available blocks. These are **best-effort admission/monitoring, not hard quotas**: trusted code can allocate between samples or outside the root, and preparation output may exceed its estimate. Crash reconciliation releases only reservations whose persisted ownership is provably inactive; expiry alone releases no capacity and authorizes no deletion.

Library callers set `RunOptions.storage` or use `fabric_core::workspace::storage::prepare` for managed preparation; `StorageOptions.meter: Arc<dyn DiskMeter>` permits deterministic injected free-space measurements. Calling the lower-level project preparation/fork APIs directly does not provide budget admission.

### Run a bounded task batch

```sh
./target/debug/fabric --root /private/path/to/fabric run-many my-project \
  --harness fake --tasks ./batch/tasks.json --concurrency 2 --json
```

The manifest is a nonempty JSON array with exactly `id` and `prompt_file` fields per task:

```json
[{"id":"first","prompt_file":"first.json"},{"id":"second","prompt_file":"second.json"}]
```

IDs must be nonempty and unique within the batch. Prompt paths are resolved against the **manifest's parent directory**, not the project, worker cwd or CLI cwd; absolute prompt paths stay absolute. For the example above, `first.json` means `./batch/first.json`. Malformed/empty manifests, duplicate/blank IDs, empty prompt paths, unknown fields and nonpositive concurrency are rejected before any worker or version probe launches. `--concurrency N` is required and positive.

A pool of at most `min(N, task_count)` threads admits external workers through the same foreground execution path as `run`, with no per-task thread creation. One READY full prepared tree and one probed harness version are pinned for the entire batch; every task gets its own stable Run ID and isolated full-tree workspace, including ignored dependencies. Preparation is reused, not rerun per task. Harness, copy-admission and capture options are the same as `run`; capture limits are **per run**, not a total batch disk quota. Real OMP execution still requires explicit spending approval.

Before admission, SQLite schema v3 records the Batch ID, pinned prepared-state ID, ordered task IDs, resolved prompt paths and assigned Run IDs. Each task durably records its ordinary Run report or a prelaunch error; launched Runs also carry their Batch ID and separate Workspace ID. A missing/unreadable prompt remains a reported task error with its assigned Run ID even though no workspace was created. One task's failure does not stop independent tasks. JSON stdout contains ordered `tasks` with `id`, `run_id`, `result` or `error`, plus `batch_id` and `succeeded`; it remains available when the command exits nonzero. Completed aggregate success/failure is also persisted. A zero CLI exit requires every task to succeed. Infrastructure errors can leave durable pending task records; this story does not resume or reconcile interrupted batches.

Batches share the same verified tracked/untracked capture-before-finalization pipeline, lease-aware automatic reclaim and cancellation/timeout controls as single runs, including artifacts for failed exits and stopped workers. Crash reconciliation does not invent missing task reports or aggregate batch success.

### Timeout and cancellation (macOS)

Both `run` and `run-many` accept `--timeout-seconds N` (positive integer, default 3600). This is a **per-worker wall-clock deadline from launch**, not an aggregate batch deadline and not a model-spending quota. Version probes separately have a five-second deadline. Ctrl-C cancels the foreground invocation: active owned groups stop, completed batch results stay durable, and queued tasks become durable `Cancelled before launch` errors without allocating a Run/workspace. Repeating cancellation is harmless. There is deliberately no command to kill a stored PID or adopt a process from SQLite.

The existing runner owns each child's private macOS process group. Timeout, cancellation and natural leader exit send group TERM, wait a bounded grace (250 ms by default), then send group KILL and reap the owned leader. The leader remains unreaped through the final signal (`waitid` with `WNOWAIT`), preventing PID reuse from redirecting a later group signal. Remaining same-credential group descendants are killed, with orphan reaping handled by macOS. Darwin's zombie-only group EPERM is accepted only after observing the owned leader's exit. Nonblocking capture drains have a bounded post-termination deadline; an inherited pipe that stays open yields an explicit incomplete-capture error/NEEDS_ATTENTION rather than a hang or fabricated complete capture. This is trusted local supervision, not a sandbox: intentionally escaped groups or processes that change credentials beyond the runner's signal permissions are not contained. SIGKILL/crash reconciliation remains unimplemented.

Library callers pass `fabric_core::run::RunOptions` to `foreground`/`run_many`, setting `limits: fabric_runner::ExecutionLimits { timeout, term_grace }` and a cloneable `fabric_runner::CancellationToken`. Calling `cancel()` on that live token is idempotent and contains no PID. API deadlines may use subsecond `Duration` values; timeout must be positive/representable and TERM grace at most 30 seconds. `ForegroundInterrupt` is an opt-in scoped SIGINT handler used by the CLI, not automatically installed by library execution.

For OMP only, `--max-cost-usd USD` (API `RunOptions::max_cost_usd`) passes its supported `--max-cost=USD` request-budget flag. Values must be finite and positive. OMP stops admitting new model requests once its reported spend reaches this budget; in-flight calls can overshoot, so it is **best effort, not an exact billing cap**. Unsupported installed OMP versions can reject the flag as an ordinary observed harness failure. Fabric enforces its own wall timeout; it does not depend on OMP `--max-time`, promise exact provider accounting, or make unapproved paid calls.

### Installed OMP adapter (manual execution requires spending approval)

After explicit approval for model spending, the corresponding command is:

```sh
./target/debug/fabric --root /private/path/to/fabric run my-project \
  --harness omp --prompt-file ./prompt.md --copy-limit-bytes 104857600 --json
```

OMP discovery reuses doctor's executable PATH lookup, resolving relative entries before changing cwd. Missing OMP fails with installation/PATH guidance before creating a run or workspace. Each single run or batch probes the selected executable once using `--version`; the exact returned version line (minus CR/LF) is persisted, not a configured version. The installed macOS binary was observed on 2026-11-22 as `omp/19.1.0`; its actual `--help` documents print mode, JSON mode, positional messages, `--no-session` and `--no-title`. Only version/help probes were executed; **AT-15 actual model smoke is not complete**.

The existing foreground runner launches `omp -p --mode json --no-session --no-title -- PROMPT` in the freshly validated private fork. UTF-8 prompt snapshot contents are one argv value, verbatim including leading flags, newlines, quotes, shell characters and empty input. NUL/non-UTF-8 inputs fail clearly; operating-system argv size limits still apply. There is no shell interpolation or Fabric prompt-file expansion. OMP may interpret its own message syntax (for example leading `@` file references); argv fidelity is not a promise to disable OMP's message semantics.

The process environment is cleared first. OMP receives fixed `LC_ALL=C`, default `PATH=/usr/bin:/bin`, and only explicitly allowlisted ambient `PATH`, `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `TMPDIR` when set. HOME/XDG permit OMP's normal installed config/auth mechanism without reading/copying credentials into Fabric-managed data; arbitrary API keys, secrets, injection variables and `PI_*` overrides are not inherited. Config files and the trusted OMP process itself remain outside Fabric's sandbox guarantees. The fake harness keeps its fixed minimal environment.

#### AT-15: explicit opt-in operator procedure (pending approval)

Do not execute this section without separate approval for real provider/model spending. First run `omp --version` and `omp --help` yourself: confirm the installed interface above, configured model and normal OMP authentication. Do not put credentials in prompts, recipes, task manifests or Fabric flags. `doctor --json` only discovers the executable; it does not prove authentication/model availability.

After approval, choose a dedicated private root and a disposable **committed local Git fixture** containing `tracked.txt` with `original`, and `.gitignore` containing `.deps/`. Prepare it using the recipe contract above with this step: `argv = ["/bin/sh", "-c", "mkdir .deps; printf ready > .deps/ready"]`. Use absolute paths below; the recipe is outside the source checkout.

```sh
FABRIC=./target/debug/fabric
ROOT=/private/path/to/approved-fabric
REPO=/private/path/to/disposable-git-fixture
$FABRIC --root "$ROOT" project add --name omp-smoke --path "$REPO"
$FABRIC --root "$ROOT" project prepare omp-smoke --recipe /private/path/to/recipe.toml --json
cat > /private/path/to/approved-prompt.txt <<'PROMPT'
Read .deps/ready and require its exact contents to be ready. Change tracked.txt to "model edited" plus a newline. Create model-note.txt containing "model created" plus a newline. Do not change anything else. Report what you actually did.
PROMPT
$FABRIC --root "$ROOT" run omp-smoke --harness omp \
  --prompt-file /private/path/to/approved-prompt.txt \
  --timeout-seconds 120 --max-cost-usd 0.25 --copy-limit-bytes 104857600 --json
$FABRIC --root "$ROOT" runs list --json
# Set RUN_ID to the observed run_id, not a fabricated expected value:
$FABRIC --root "$ROOT" runs show "$RUN_ID" --json
$FABRIC --root "$ROOT" runs artifacts "$RUN_ID" --json
$FABRIC --root "$ROOT" storage status --json
$FABRIC --root "$ROOT" storage gc --dry-run --json
```

Require the observed OMP version, zero exit, SUCCEEDED Run, source/recipe/prompt digests, actual parsed JSON records in `events.jsonl`, a nonempty preserved `tracked.patch`, complete checksummed artifacts and reclaimed workspace; inspect any actual failure instead of relabeling it success. Follow recovery below in a separate checkout and verify the two requested file contents and unchanged original/base `.deps/ready`. Save actual command outputs, Run ID, version, provenance, checksums and recovered bytes outside the managed root for review. If approved, repeat with four distinct prompt files in the existing `run-many` manifest format and `--concurrency 2`, verifying independent Run IDs/artifacts. Budget flags are best effort and in-flight requests may overshoot. No command in this procedure has been executed against a model as part of automated acceptance.

### Read-only inspection and recovery

```sh
./target/debug/fabric --root /private/path/to/fabric runs list --json
./target/debug/fabric --root /private/path/to/fabric runs show RUN_ID --json
./target/debug/fabric --root /private/path/to/fabric runs artifacts RUN_ID --json
./target/debug/fabric --root /private/path/to/fabric storage status --json
./target/debug/fabric --root /private/path/to/fabric storage gc --dry-run --json
```

Omit `--json` for pretty metadata. `runs list` is an array ordered by Run ID. `runs show` has `run`, `workspace`, and `prepared_state`: persisted state/times, source/recipe/environment/prompt digests, batch/harness/version, observed process/exit/error/capture metadata, lease expiry and retention reason. A recorded PID is historical metadata, never kill authority. `runs artifacts` has `run_id`, nullable `artifact_manifest_id` and artifacts ordered by kind, with root-relative paths, byte lengths and SHA-256. Empty/unavailable artifacts are not fabricated. Unknown Run IDs return `not_found` on stderr with nonzero exit and empty stdout; unsafe IDs are invalid arguments. Inspection never launches a harness, reads prompt/log contents or prints environment/credentials. Trusted harness output and prompts themselves are not generically redacted.

Project status and run inspection use the existing native read-only SQLite path, including committed live WAL, without migrations, reconciliation or logical writes. SQLite may coordinate locks/shared memory or create validated private WAL/SHM sidecars; read-only means no logical state/data mutation, not zero filesystem coordination. Stable persisted metadata produces stable JSON; live processes and storage/free-space measurements can legitimately change.

Artifacts remain discoverable after workspace reclaim; inspect their registered inventory rather than assuming the old workspace path still exists. Verify each artifact's length and SHA-256 (`shasum -a 256 "$ROOT/relative_path"`) and the manifest's source SHA before recovery. Create a separate trusted checkout of that exact source commit, then apply `tracked.patch` with `git -C /private/path/to/recovery apply --binary /absolute/path/to/tracked.patch` (skip an empty patch), and extract `untracked.tar` using `tar -xpf /absolute/path/to/untracked.tar -C /private/path/to/recovery`. Never restore into the managed prepared base or original repository. The archive restores only nonignored untracked paths; ignored dependencies require the original preparation recipe. Inspect trusted-local artifact inputs before extraction; Fabric is not an untrusted archive sandbox.

Storage figures count best-effort **logical bytes**, not APFS-exclusive allocation or a hard quota. Unknown/unsafe measurements are reported rather than treated as zero. Admission reserves conservatively and monitors workers, but scans race allocations and cannot enforce an exact disk/billing ceiling. Failed/incomplete capture and uncertain ownership retain recovery workspaces; expiration of the policy's 24-hour recovery period alone does **not** delete them. Current GC does not expire recovery data or purge artifact history. `storage gc --apply` reconciles proven inactive ownership and reclaims only validated eligible workspaces with complete durable recovery evidence; it may safely refuse or report unknown paths. It does not repair artifacts or kill recorded PIDs. Back up retained evidence, inspect reasons and resolve actual failures; do not bypass ownership protections or manually edit the database.

### Deterministic acceptance map

All names below refer to permanent tests run by `cargo test --offline --workspace`; no installed OMP, credentials or paid calls are needed. The macOS workflow fetches locked dependencies first, then runs fmt, strict clippy and the complete offline workspace suite.

| Acceptance | Runnable evidence |
| --- | --- |
| AT-01 preparation reuse | CLI `prepare::ready_cache_reuses_once_and_commit_recipe_environment_separate_states`, `concurrent_processes_wait_and_reuse_one_ready_publication` |
| AT-02 prepared dependencies | workspace `fork::admitted_copy_preserves_complete_prepared_tree_without_setup_and_isolates_siblings`; CLI end-to-end below |
| AT-03 isolation | the same fork test, core `durable_pair_precedes_backend_and_owned_siblings_isolate_same_relative_writes`, CLI end-to-end |
| AT-04 fan-out | CLI `run::run_many_caps_actual_external_workers_and_durably_associates_results` (four external fake workers, barrier-observed peak two) |
| AT-05 provenance | CLI `foreground_fake_process_records_provenance_exit_and_recovers_changes_after_reclaim`, end-to-end checksummed manifest verification |
| AT-06 outputs | CLI `tracked_binary_patch_round_trips_staged_and_unstaged_changes`, `untracked_archive_restores_bytes_modes_links_and_excludes_ignored_dependencies`, end-to-end recovery |
| AT-07 safe failure | CLI `capture_failures_durably_retain_workspace_and_never_claim_terminal_success` |
| AT-08 timeout/cancel | CLI `wall_timeout_stops_fake_parent_and_term_ignoring_child_and_persists_reason`, `ctrl_c_cancels_live_worker_and_preserves_observed_exit` |
| AT-09 no unsafe deletion | CLI `gc_refuses_unsafe_ownership_paths_manifests_and_noneligible_states`, `gc_reports_unknown_paths_and_unlinks_internal_links_without_touching_targets` |
| AT-10 low space | CLI `storage_admission_cli_denies_before_worker_and_reports_usage`, `storage_injected_low_free_and_managed_cap_never_launch` |
| AT-11 GC interruption | core `interrupted_cleanup_resumes_after_intent_partial_unlink_marker_and_container_removal`, `storage_cache_intent_resumes_and_same_key_rebuilds_after_interruption` |
| AT-12 expired leases | CLI `reconciliation_preserves_expired_live_workers_then_recovers_crashed_owner`, `reconciliation_protects_orphan_worker_that_closed_inherited_lease_descriptor` |
| AT-13 output cap | CLI `hundred_mib_concurrent_fake_output_has_one_durable_capture_budget`: actual 50 MiB per pipe, 100 MiB total drained against shared 10 MiB durable cap, truncation marker and checksums |
| AT-14 JSON inspection | CLI doctor/project suites plus `acceptance_cli_inspects_four_isolated_runs_and_recovery_after_reclaim`, `inspection_reads_live_wal_without_migration_or_logical_writes` |
| AT-15 real OMP | **NOT EXECUTED**, separate opt-in procedure above; not covered by fake acceptance |

The end-to-end test drives actual CLI processes: unchanged recipe reuse with a once-only ignored setup counter, four fake tasks at concurrency two, dependency-preserving forks, independent tracked/nonignored-untracked edits, durable SHA-256/length-verified artifacts, safe automatic reclaim, read-only inspection afterwards, and restoring each distinct result into a fresh checkout. Source/prepared bytes stay unchanged and staging is empty. The live-WAL test keeps a writer open, verifies WAL-only retention metadata through all inspection surfaces, and proves DB/WAL bytes and schema version unchanged.

```sh
cargo test --offline -p fabric-cli --test run acceptance_cli_inspects_four_isolated_runs_and_recovery_after_reclaim
cargo test --offline -p fabric-cli --test run inspection_reads_live_wal_without_migration_or_logical_writes
```

### Bounded run capture

Both harness adapters and version probes use the same concurrent stdout/stderr drain. Ordinary runs default to **10 MiB total**, configurable with `--max-log-bytes N` (also an explicit core/runner API parameter). The sum of actual `stdout.log`, `stderr.log` and `events.jsonl` lengths never exceeds this shared budget; it is not a separate allowance per file. All raw bytes, JSON record wrappers/newlines and the truncation marker count. Bounded SQLite capture metadata, the input `prompt.input` snapshot and workspace/source files are outside this output budget.

The runner reserves enough space for `\n[FABRIC CAPTURE TRUNCATED]\n` up front (limits smaller than this marker are rejected). The first write that cannot fit ends persistence across all three files; raw chunks may retain their fitting prefix, structured records are kept whole or omitted. The marker is appended to stdout and charged to the same cap. Concurrent scheduling determines which stream gets the remaining bytes; whole-record omission can leave unused capacity. A marker reserve can therefore trigger truncation before the nominal cap is full. Both pipes continue draining to EOF after truncation or disk-write failure, without retaining discarded output; background descendants cannot hold the pipes open after the leader exits.

Stdout is decoded concurrently with a 64 KiB maximum line buffer and serde_json's recursion limit, retaining only one decoded value and one reused bounded serialized record buffer. Oversized lines drain without accumulating; parsing resumes at the next newline. Malformed JSON/UTF-8 does not fail an otherwise successful harness. CRLF and final unterminated lines work. Each persisted event is a complete JSONL record with `line_number` (one-based), `line_bytes` (excluding LF, including any CR), `status` (`parsed`, `invalid` or `oversized`) and `value` only for parsed JSON. Invalid/oversized records store no raw line contents. Report/metadata counts cover the **entire drained stdout**, including events omitted after truncation; the structured file may consequently contain fewer records than those counts.

`capture` in JSON reports and reopened Run records contains the configured limit, actual combined persisted bytes, truncation flag, both streams' full observed byte counts, event counts, and per-file length/SHA-256. Checksums cover exactly the persisted bytes (including the marker), and all three files are synced before their capture metadata is recorded in FINALIZING. Artifact publication verifies copied output against these checksums before terminal completion. Capture failure retains the workspace and records NEEDS_ATTENTION, preserving an observed child exit and true timeout/cancel reason; output truncation itself does not change the harness exit result. Output checksums and tracked artifact manifests never grant deletion authority.

Environment selection remains the explicit allowlist above; Fabric does not discover credentials or generically redact arbitrary trusted-harness output. Avoid prompts or tool output containing secrets.

## Checks

Tests run without network access, credentials, or an installed OMP. The discovery tests use executable marker scripts to ensure no discovered tool runs and check that fixture user data remains unchanged.

```sh
cargo test --locked -p fabric-domain --test domain
cargo test --locked -p fabric-cli --test doctor
cargo test --locked -p fabric-store --test store
cargo test --locked -p fabric-cli --test project
cargo test --locked -p fabric-project --test recipe
cargo test --locked -p fabric-cli --test prepare
cargo test --locked -p fabric-workspace --test fork
cargo test --locked -p fabric-core --test workspace
cargo test --locked -p fabric-cli --test run
cargo test --locked -p fabric-runner
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

After dependencies have been cached, add `--offline` to Cargo build/test/clippy commands to prohibit registry access as well. Paid/model smoke is excluded from automated checks; AT-15 remains pending explicit approval. Project/preparation tests use temporary local Git repositories, including dirty and newly committed sources, ignored dependencies, literal argv/environment handling, hook/config suppression, retained failures, process-group/total timeouts, fail-closed managed paths, once-only cache side effects, competing CLI processes, and interrupted-stage reconciliation. Fork tests force admitted ordinary copies, compare every tree entry/content and independent inode, preserve executable/chained symlink behavior, concurrently edit siblings without base leakage, and reject insufficient admission, existing destinations and source/destination tampering.

## Harness-neutral amendment

The supplied v0.2.1 amendment is tracked separately in `PRD_HARNESS_NEUTRAL.json`. HN-01–04 implement registry-owned adapters, durable worker-attempt evidence, exact version probes, fail-closed capability planning, mixed-harness batches and explicit native-profile/credential boundaries. Existing v0.1 commands and storage remain compatible.

```sh
fabric harnesses list --json
fabric harnesses doctor fake --json
fabric harnesses doctor omp --json
```

Doctors are bounded, version-only probes, not provider/authentication or model checks. Supported pins are `fabric-fake-harness 1`, `fabric-fake-a 1`, `fabric-fake-b 1`, `omp/19.1.0` and `codex-cli 0.171.0`; other versions fail `unsupported_version`. Worker requests accept `--model`, repeatable `--require-capability`, `--placement`, `--access`, `--hard-read-only` and `--resume`, but reject capabilities not implemented by that adapter before allocating any batch or worker. Trusted-local placement currently enforces edit only; remote/BYOC, inspect/read-only and publish are not implemented. Only fake-a currently supports model override. Missing usage/cost remains unknown; resume is unsupported.


`run-many` accepts task-local harnesses and complete request overrides; `--harness` supplies an optional default. Prompt paths resolve against the task manifest's directory. For example, `tasks.json`:

```json
[
  {"id":"json-worker","prompt_file":"first.txt","harness":"fake-a"},
  {"id":"text-worker","prompt_file":"second.txt","harness":"fake-b"}
]
```

```sh
fabric run-many PROJECT --tasks tasks.json --concurrency 2 --json
```

Both distinct fixture protocols use the same pool, attempt records, isolated forks, bounded capture and safe reclaim. `events.jsonl` retains raw native records and adds normalized Started/Output/Tool/Usage/Completed/Failure events with sequence numbers; malformed or oversized frames remain bounded. Native fatal errors fail the run even when the process exits zero or the capture cap is exhausted. Recoverable native tool/message errors do not imply terminal failure. OMP event conformance uses pinned protocol fixtures, not paid live validation.

### Explicit profiles

Workers and version probes clear the inherited environment and use private HOME/XDG directories. Real OMP runs require `--profile PATH` (or a task request's `profile`), bound to `omp` / `omp/19.1.0`, with explicit nonsecret connection references. For an already configured environment credential:

```json
{
  "harness_id":"omp",
  "harness_version":"omp/19.1.0",
  "connection_refs":[{"kind":"env","name":"OPENAI_API_KEY"}]
}
```

Only approved OMP provider variable names may be selected; unselected keys, profile selectors and proxies are not inherited. The referenced value must already exist in the launching process; never put credentials in profile JSON. Native config references accept absolute existing `native-home` and `native-agent-dir` directories. A selected native `agent.db` requires explicit `{"kind":"native-auth","name":"native-home"}` authorization; Fabric checks metadata without opening or hashing the auth database.

Reports and sealed manifests retain an opaque profile digest, bounded non-auth config fingerprint and scope diagnostics. Legacy manifests without profiles remain readable. Native config is trusted-local, not a sandbox or atomic reproducible HOME snapshot; fingerprints exclude auth, plugin/skill trees and external services. Explicit skill/plugin/MCP imports that cannot be enforced fail before allocation.

### Codex adapter: source/fixture-conformant

`codex` targets upstream `rust-v0.171.0` (`codex-cli 0.171.0`). It runs ephemeral `exec --json` workers with native workspace-write sandboxing and pinned noninteractive approval behavior. Fabric still owns lifecycle, bounded capture and reclaim; this is not a hard-read-only or multitenant isolation guarantee.

An explicit profile can authorize an existing environment connection without inheriting ambient Codex configuration:

```json
{
  "harness_id":"codex",
  "harness_version":"codex-cli 0.171.0",
  "connection_refs":[{"kind":"env","name":"CODEX_API_KEY"}]
}
```

Optional `native_config_refs` accept one absolute existing `native-config-dir` path (`CODEX_HOME`). If that directory exposes `auth.json`, separately authorize `{"kind":"native-auth","name":"native-config-dir"}`; Fabric checks regular-file metadata without opening, copying or fingerprinting auth. Only bounded `config.toml` content enters the non-auth fingerprint. Selected native config is trusted-local and may reference external services; it is not portable or fully fingerprinted.

Final assistant text, tool lifecycle and token usage are normalized; cost remains unknown. Exact model override, resume/follow-up, cost caps and unsupported skill/plugin/MCP imports fail before allocation. Native raw logs remain private artifacts and may contain sensitive harness output.

`scripts/smoke-codex.sh` defaults to `NOT_VERIFIED` without invoking a provider. Explicit `--run --profile /absolute/profile.json` enables a potentially billed smoke, requires the exact installed version, and checks output, sealed evidence and cleanup before reporting PASS. No live Codex run has been verified here; deterministic fixture conformance is not provider/authentication validation.
