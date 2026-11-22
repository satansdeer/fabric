# Fabric

Fabric is a macOS-first, harness-neutral local worker layer: durable asynchronous handles, immutable prepared sources, independent writable workspaces, captured evidence and verified reclaim. **Trusted local code only:** not a sandbox, cloud backend or multi-tenant security boundary.

## Implemented: FAB-01 through FAB-20 (automated scope)

The seven-package Cargo workspace implements local diagnostics, an I/O-free domain model, SQLite persistence, pinned local Git project registration, validated preparation recipes/fingerprints, reusable private prepared snapshots, full-tree forks, durable run-owned workspaces and foreground or detached local harness workers. Successful preparation publishes a durable read-only READY tree, including ignored dependencies. Runs publish durable tracked and nonignored untracked artifacts before lease-aware, path-safe reclaim; `storage gc` reports and retries eligible cleanup.

Requires Rust 1.85 or newer. Resolved dependencies are pinned in `Cargo.lock`.

Native prerequisites: macOS, Xcode Command Line Tools (`xcode-select --install`) for Git/linking, and a Rust toolchain with rustfmt/clippy. CI uses Rust 1.91 on `macos-latest`. APFS supports native `clonefile` full-tree forks; other local filesystems require an explicitly admitted ordinary copy. SQLite is bundled. OMP is optional for deterministic tests and must be installed/configured separately for an approved model run. No Linux support or sandbox isolation is claimed.

**AT-15 is NOT EXECUTED.** Automated acceptance is not model-demonstrated acceptance; the project is not declared `POC_READY_FOR_REVIEW`.

### Release evidence

| Independent gate | Evidence and limitation |
| --- | --- |
| `LOCAL_WORKER_API_READY` | Provider-free local lifecycle/API acceptance; see the recorded [verification](docs/VERIFICATION.md), not a provider-authentication claim. |
| `PSTACK_LOCAL_SWARM_READY` | Provider-free typed controls, installed OMP host and explicit local pstack bridge; model-selected `/skill:swarm` routing remains **NOT_VERIFIED**. |
| `LIVE_HARNESS_VERIFIED` | **NOT_VERIFIED / BLOCKED** pending explicit spending authorization and actual OMP, Codex and mixed-harness evidence. |

The C05 installed OMP 19.1.0 proof used real tools and separate fake workers, but **no agent/model turn**. Native skill discovery does not prove a model chose that route. Default checks never invoke paid providers. Native-context consent is not consent to provider spending; local pstack execution is not cloud parity.

See [control surfaces](docs/CONTROL_SURFACES.md), [product boundary](docs/PRODUCT_BOUNDARY.md), [removals and compatibility](docs/REMOVALS.md), and the explicitly opt-in [live smoke guide](docs/LIVE_SMOKES.md).

Issue-driven development: [local approval and worker workflow](docs/ISSUE_WORKFLOW.md), [roadmap #1](https://github.com/satansdeer/fabric/issues/1), and [bootstrap #16](https://github.com/satansdeer/fabric/issues/16). The first loop uses the existing local runtime; hosted Connect and plugin migration are not prerequisites. Provider-free checks do not prove model-backed dogfooding.

Hosted GitHub intake: [Connect deployment and supervised consumer](docs/CONNECT.md), tracked in [#18](https://github.com/satansdeer/fabric/issues/18). Implementation passes provider-free end-to-end checks; launch is blocked by exhausted Netlify credits and missing `polyglo.tv` registration/delegation. A signed webhook queues work but cannot authorize execution or model spending.

**Upgrade:** finish active/queued work, then run the old binary's `fabric --root "$ROOT" manager stop --json` before switching binaries/schema. Stop refuses active work; a successful reply guarantees endpoint/ownership release. Do not kill active work or attempt a live in-place manager upgrade. Schema v13 adds bounded local observations to v12's immutable capture context; older runs have no retrospective activity samples and lacking capture evidence remain retained.

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

### Discover and connect a native harness

Run from the project's Git checkout:

```sh
fabric --root /private/path/to/fabric init --harness omp --check
fabric --root /private/path/to/fabric init --harness omp
# Or connect explicitly without the guided project/recipe workflow:
fabric --root /private/path/to/fabric harness connect omp --yes
fabric --root /private/path/to/fabric harness revoke omp
```

`init --check` and noninteractive `init` only report discovery, source/READY status and next actions. Interactive `init` asks separately before connecting, registering the project, creating a private recipe, and executing the reviewed preparation recipe. It never adds a recipe to the source checkout.

OMP selects `$PI_CODING_AGENT_DIR` or `~/.omp/agent`; Codex selects `$CODEX_HOME` or `~/.codex`. Only adapter-approved exact versions are accepted. Discovery examines credential-file metadata, not credential contents, and does not authenticate or invoke a model. Connect displays the exact selected native paths and exposure: native configuration, skills/extensions/plugins, MCP and commands are trusted local code, not sandboxed.

Consent is an owner-only, bounded, atomic local reference record, never copied credentials. Workers retain private HOME/XDG directories and do not inherit arbitrary provider keys. Native configuration/path identity changes require reconnecting. Revocation blocks future implicitly authorized launches, including already planned launches; it neither kills running workers nor invalidates separately supplied explicit profiles. Machine-readable connect requires `--yes --json` and emits no interactive prompt.

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

### Watch a running worker locally

```sh
# Metadata only, before exit or after capture/reclaim:
fabric --root "$ROOT" runs logs "$RUN" --json
fabric --root "$ROOT" runs logs "$RUN" --after "$NEXT_CURSOR" --json
fabric --root "$ROOT" runs logs "$RUN" --follow
# Ctrl-C stops only this viewer. It does not cancel the worker.

# Explicit sensitive-content opt-in; never post/upload this output:
fabric --root "$ROOT" runs logs "$RUN" --raw stdout --limit-bytes 4096 --follow
# --raw stderr or --raw events selects the other local stream.

# Inspect twice, compare timestamps/counters and actual work, then decide:
fabric --root "$ROOT" runs logs "$RUN" --json
sleep 30
fabric --root "$ROOT" runs logs "$RUN" --json
# Only an explicit operator decision stops the owned process group:
fabric --root "$ROOT" cancel "$RUN" --json
fabric --root "$ROOT" runs artifacts "$RUN" --json
```

`runs logs` correlates project/batch/run/attempt/harness with queued/start/exit, context validation, capture and cleanup timestamps. Metadata is an at-most-8-KiB persisted snapshot per run, sampled every 250 ms while supervising; it continues counting observed stdout/stderr bytes, bounded frames and normalized/tool events after the combined raw/event capture budget fills. This is a coalesced activity view, not a replay journal or synthetic model progress. `verified_progress: not_assessed` is deliberate: output, a recently **observed** alive process, and correct task progress are different facts. Compare `sampled_at_ms`, `process_alive_observed_at_ms` and output/tool timestamps; persisted PID metadata is never liveness authority. Silence alone is not a stall verdict. Current/last tool is an allowlisted category (unknown names become `other`), a last-observed slot rather than a concurrent-tool inventory.

Metadata cursors are revisions; raw cursors are byte offsets, separate for stdout/stderr/events. Use the returned `next_cursor` only with the same run/mode/stream. Reads accept 1–65536 raw bytes (default 16384), reject future/malformed cursors, and emit bounded JSON records; raw chunks are JSON-escaped, lossy UTF-8, so split/invalid characters can be replaced. Follow emits JSON lines until terminal/EOF or Ctrl-C. Core opens only owned run-container or registered sealed artifact files, rejecting symlinks, special files and foreign paths; it never accepts a caller-supplied log path.

Default diagnostics omit native content and raw errors. **Opt-in raw logs can contain prompts, credentials, model reasoning, arbitrary tool arguments/results and native errors. There is no universal secret redaction.** Keep them private and local, never upload to GitHub/Connect. No new service, provider SDK or sandbox is involved; native environment fidelity and trusted-local execution remain unchanged.

Healthy long-running example (illustrative, not a native-run verification): explicitly select a 7200-second emergency deadline with 64 MiB combined raw/event capture and 128 MiB artifacts using the [bridge limit flags](docs/ISSUE_WORKFLOW.md#dispatch-one-bounded-worker). At minute 15, raw capture may be truncated while observed-byte/tool counters still advance and process observations remain fresh. Leave healthy work running; investigate actual task evidence if counters are quiet. The configured deadline is reported from the observed start (the supervisor's monotonic timer is authoritative), not an inactivity detector or spending cap. There is no inactivity kill, automatic restart or 600-second restart loop. Existing explicit cancel still TERM/KILLs and reaps the owned group, then captures artifacts and attempts protected reclaim.

### Inspect or reclaim captured workspaces

```sh
fabric --root /private/path/to/fabric storage gc --dry-run --json
fabric --root /private/path/to/fabric storage gc --apply
```

Choose exactly one mode. Reports are sorted by workspace ID, with `action`, stable `reason`, and diagnostic `detail`; unknown fork-staging entries are reported separately and never adopted. Dry-run opens validated private regular DB/WAL/SHM paths in SQLite read-only mode, seeing committed WAL updates from live writers rather than a stale immutable snapshot. It never migrates the schema, logically writes database data, records an intent, changes workspace/artifact data or permissions, or deletes managed files. Native SQLite lock/shared-memory coordination may create or update its normal private WAL/SHM sidecars, even when the database was initially closed; this is not a zero-I/O inspection contract. Competing cleaners refuse `cleanup_lease_busy`.

Under an exclusive kernel cleanup lease, GC first attempts conservative recovery of RECOVERY_REQUIRED workspaces, then requires a terminal Run, CLEANUP_PENDING without retention or a live lease, exact canonical managed ownership, private real containers/tree, and a complete checksummed registered manifest matching provenance. Outside/nonexact recorded paths, missing/mismatched markers, symlink containers/tree, incomplete/corrupt evidence and live or unverifiable owners remain retained. A durable immutable SQLite intent binds directory and marker identities before deletion. Descriptor-relative no-follow traversal unlinks internal symlinks without following targets; it refuses foreign ownership and mounted filesystems. Interrupted cleanup can resume with partial trees, a missing marker or a missing container only against that saved authority; substitutions fail closed. Ownership records and intents remain after deletion. No PID is deletion authority.

Cleanup leases explicitly unlock on scope exit, so unrelated forked helpers cannot prolong a completed cleanup by retaining an inherited descriptor.

### Interrupted workers and recovery

Schema v8 persists each worker's owner, expiry and private lock-file identity **before** reserving storage, including the pre-Run crash window. Preparation startup, worker admission and GC `--apply` reconcile under the root cleanup lock; read-only status and GC dry-run do not reconcile. Expiry is diagnostic, never death or deletion authority. An inherited kernel lock protects orphaned workers; macOS also compares native PID/start-time identity and inspects the original process group, protecting live workers even after expiry or closing the inherited descriptor. Persisted PIDs are never signalled: only the existing live foreground supervisor may signal its own process group.
Normal foreground completion durably releases ownership and explicitly unlocks it, so unrelated concurrent forks cannot prolong a finished worker's reservation. Dropping an unreleased owner never explicitly unlocks an orphan worker's inherited descriptor.

When that authority proves the owner and worker inactive, stale reservations are released before admission. Interrupted nonterminal Runs become NEEDS_ATTENTION and their workspaces RECOVERY_REQUIRED, retaining original files, observed exit/capture metadata and registered artifacts without inventing an exit or success. Fully captured terminal Runs keep their recorded outcome and may resume the existing verified cleanup intent. Repeating reconciliation changes neither evidence nor outcomes.

```sh
fabric --root /private/path/to/fabric recover RUN_ID --json
fabric --root /private/path/to/fabric recover RUN_ID --apply --json
```

Recovery defaults to inspection. Apply requires positively inactive ownership, exact managed identity and complete durable capture evidence. Schema v12 saves the resolved profile and artifact limit before execution; historical runs without sufficient evidence remain retained. Recovery can seal and register a complete interrupted capture, or capture a valid owned tree when the original capture metadata and context were durably recorded. Partial/corrupt staging, substituted paths and unknown completion remain evidence, not permission to guess or delete.

Verified artifacts are registered atomically before the existing safe cleanup path runs. Recovery preserves observed outcomes, times, capture metadata, worker attempts and accepted sources; it never requeues or launches a worker. Terminal asynchronous results expose the recovered artifact inventory and cleanup state. `storage gc --apply` uses the same recovery path; dry-run remains nonmutating.

Missing, replaced or unverifiable authority, a crash between launch intent and durable worker identity, unsupported native inspection, and pre-v8 reservations without reliable ownership remain conservatively retained. These require operator recovery rather than hidden PID-based killing or automatic deletion; live/unverifiable workers continue to consume reserved headroom.

### Storage budgets and usage

```sh
fabric --root /private/path/to/fabric storage status --json
fabric --root /private/path/to/fabric project prepare my-project --recipe ./fabric.prepare.toml \
  --max-managed-bytes 21474836480 --min-free-bytes 5368709120 --max-workspace-bytes 2147483648
fabric --root /private/path/to/fabric run my-project --harness fake --prompt-file ./task.json \
  --max-managed-bytes 21474836480 --min-free-bytes 5368709120 --max-workspace-bytes 2147483648 --json
```

`project prepare`, `run`, `run-many` and `storage status` accept these three byte flags with the defaults shown. Worker log/artifact caps retain their existing flags. Overrides are per invocation, not persisted configuration. Status is a read-only inspection with the same native SQLite coordination-sidecar caveat as GC dry-run; its JSON includes policy, free space, managed logical usage, outstanding admission reservations and per-cache/workspace usage, retention reasons and measurement diagnostics. Workspace entries include `run_id` and an operator `action`, with largest known retained usage first. A missing or unsafe retained path reports unknown usage (`null`), not invented zero; safely reclaimed workspaces report zero with artifacts preserved.

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

### Submit durable background workers

Connect the native harnesses first, then write a version-free `workers.json`:

```json
{
  "project": "my-project",
  "max_concurrency": 2,
  "tasks": [
    {"task_id": "review", "harness_id": "omp", "prompt": "Review the prepared source."},
    {"task_id": "check", "harness_id": "codex", "prompt": "Check the same prepared source."}
  ]
}
```

```sh
fabric --root /private/path/to/fabric workers spawn --request workers.json --json
```

The response is `{accepted,batch_id,tasks:[{task_id,run_id,status}]}` before worker completion. Use the same `status`, `results`, `wait` and `cancel` commands below with these durable IDs. Omitted models inherit the harness's native default; explicit `model` overrides must be supported by that adapter. Placement defaults to `local`; `remote`, `cloud` and `byoc` fail preflight rather than falling back to local execution.

Each task may set `limits` (`timeout_seconds`, `copy_limit_bytes`, `max_log_bytes`, `max_artifact_bytes`), persisted and enforced independently. The copy limit bounds byte-copy fallback, not successful APFS clones or their logical size. Hard `max_cost_usd` is rejected because no registered adapter enforces a spending cap. Optional `execution_context` selects `{"kind":"native"}` (the default) or an advanced `explicit` context with `harness_version`, `native_config_refs` and `connection_refs`. Skills, plugins and MCP belong to native harness configuration, not this request; nonempty legacy v1 portable lists receive a migration error.

### Select an immutable prepared source

For one native-context worker, no request file is needed:

```sh
fabric --root /absolute/fabric-root workers spawn \
  --project my-project --harness omp --prompt 'Review this prepared source.' \
  --prepared-state "$PREPARED_STATE_ID" --json
```

Use the `state_id` returned by project preparation, or replace `--prepared-state` with `--commit "$COMMIT_SHA"` for an exact full commit SHA. Explicit refs select a READY state owned by the project; they remain usable after the original checkout is moved or deleted. No source flag means exactly the current checkout HEAD at admission, never an older cached fallback; an unavailable checkout or unprepared HEAD fails without admitting workers.

JSON tasks may independently select `source_ref: {"kind":"prepared","state_id":"..."}`, `{"kind":"commit","sha":"..."}` or `{"kind":"current-head"}`. Every admitted source is frozen and pinned through queued execution and manager restart. Workers use immutable prepared bytes, not dirty checkout changes; source evidence records the selected state/base and whether checkout dirtiness was observed. An unavailable or uninspected checkout has unknown dirtiness, not a fabricated clean status. `--request` cannot be combined with inline task flags, and `--prepared-state` and `--commit` are mutually exclusive.

### Version-1 compatibility

`submit` accepts the same strict v1 inline-prompt `spawn_many` request as `api`, but returns `{accepted,batch_id,run_ids}` after atomic durable admission, not worker completion. The existing `api`, `run` and `run-many` remain foreground operations.

```sh
fabric --root /private/path/to/fabric submit --request request.json \
  --idempotency-key review-1 --json
# Use returned batch/run IDs:
fabric --root /private/path/to/fabric status BATCH_ID --json
fabric --root /private/path/to/fabric status BATCH_ID --after CURSOR --json
fabric --root /private/path/to/fabric wait BATCH_ID --timeout-seconds 30 --json
fabric --root /private/path/to/fabric results BATCH_ID --json
fabric --root /private/path/to/fabric cancel RUN_ID --json
fabric --root /private/path/to/fabric cancel BATCH_ID --json
fabric --root /private/path/to/fabric manager stop --json
```

The first client starts a detached owner-only Unix-socket manager. Exiting the submitter does not cancel accepted work. One batch at a time uses the bounded pool; later accepted batches wait durably. `manager serve --json` runs an owned foreground manager for supervision/debugging. Stop refuses active or queued work; a successful stop reply means the endpoint and ownership lock have been released, so an immediate reconnect can start a fresh manager.

Prompts, exact plans, consent stamps, source IDs and task IDs are persisted together in schema v11; accepted v10 records retain their evidence and gain per-task source pins. Reusing an idempotency key with the same logical request returns the original handles; changing that request is an error. A failed reply is not automatically retried because admission may already have committed.

Fresh clients can read retained results and artifacts after workspace reclamation. Status events are cursor-ordered and bounded (256 per reply; 1,024 recent events per batch). `wait` uses manager notifications and a bounded timeout, not controller hot-polling. Cancellation only signals live manager-owned workers; queued cancellation never launches a worker. After manager failure, only unclaimed queued work resumes. Claimed work becomes `NEEDS_ATTENTION`, remains protected, and cannot be cancelled using persisted PIDs.

Native consent is revalidated immediately before launch. Async environment-backed connection profiles are rejected rather than persisting secret environment values or silently changing credentials after restart; use durable native configuration or the foreground API.

### Timeout and cancellation (macOS)

Both `run` and `run-many` accept `--timeout-seconds N` (positive integer, default 3600). This is a **per-worker wall-clock deadline from launch**, not an aggregate batch deadline and not a model-spending quota. Version probes separately have a five-second deadline. Ctrl-C cancels the foreground invocation: active owned groups stop, completed batch results stay durable, and queued tasks become durable `Cancelled before launch` errors without allocating a Run/workspace. Repeating cancellation is harmless. There is deliberately no command to kill a stored PID or adopt a process from SQLite.

The existing runner owns each child's private macOS process group. Timeout, cancellation and natural leader exit send group TERM, wait a bounded grace (250 ms by default), then send group KILL and reap the owned leader. The leader remains unreaped through the final signal (`waitid` with `WNOWAIT`), preventing PID reuse from redirecting a later group signal. Remaining same-credential group descendants are killed, with orphan reaping handled by macOS. Darwin's zombie-only group EPERM is accepted only after observing the owned leader's exit. Nonblocking capture drains have a bounded post-termination deadline; an inherited pipe that stays open yields an explicit incomplete-capture error/NEEDS_ATTENTION rather than a hang or fabricated complete capture. This is trusted local supervision, not a sandbox: intentionally escaped groups or processes that change credentials beyond the runner's signal permissions are not contained. SIGKILL/crash reconciliation remains unimplemented.

Library callers pass `fabric_core::run::RunOptions` to `foreground`/`run_many`, setting `limits: fabric_runner::ExecutionLimits { timeout, term_grace }` and a cloneable `fabric_runner::CancellationToken`. Calling `cancel()` on that live token is idempotent and contains no PID. API deadlines may use subsecond `Duration` values; timeout must be positive/representable and TERM grace at most 30 seconds. `ForegroundInterrupt` is an opt-in scoped SIGINT handler used by the CLI, not automatically installed by library execution.

For OMP only, `--max-cost-usd USD` (API `RunOptions::max_cost_usd`) passes its supported `--max-cost=USD` request-budget flag. Values must be finite and positive. OMP stops admitting new model requests once its reported spend reaches this budget; in-flight calls can overshoot, so it is **best effort, not an exact billing cap**. Unsupported installed OMP versions can reject the flag as an ordinary observed harness failure. Fabric enforces its own wall timeout; it does not depend on OMP `--max-time`, promise exact provider accounting, or make unapproved paid calls.

### Installed OMP adapter (live execution requires spending approval)

The adapter pins `omp/19.1.0`; Codex pins `codex-cli 0.171.0`. Version-only doctors do not authenticate or prove model availability. Both adapters currently reject Fabric model overrides; explicitly choose and authorize the native configured model before a live invocation, with no guessed model or fallback.

OMP runs `omp -p --mode json --no-session --no-title -- PROMPT` in a freshly validated private fork. The prompt is one literal argv value, not a shell command; native OMP message semantics still apply. Workers/probes clear inherited environment and use private HOME/XDG. Only consented native context or an explicit advanced profile authorizes selected native configuration/credential references; arbitrary ambient keys are not forwarded. Trusted configuration may execute commands and is not a security sandbox.

#### AT-15: operator-only live acceptance

**NOT EXECUTED / BLOCKED** without separate provider-spending authorization. Follow [LIVE_SMOKES.md](docs/LIVE_SMOKES.md) for immutable-source OMP, Codex, mixed async batches and explicitly local pstack smoke recipes, outcome/artifact verification, recovery and reclaim evidence. The existing [Codex smoke](scripts/smoke-codex.sh) is provider-free without `--run`; only an explicitly authorized run with an operator-supplied profile may report live PASS.

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

Storage figures count best-effort **logical bytes**, not APFS-exclusive allocation or a hard quota. Unknown/unsafe measurements are reported rather than treated as zero. Admission reserves conservatively and monitors workers, but scans race allocations and cannot enforce an exact disk/billing ceiling. Failed/incomplete capture and uncertain ownership retain recovery workspaces; expiration of the policy's 24-hour recovery period alone does **not** delete them. Current GC does not expire recovery data or purge artifact history. `storage gc --apply` reconciles proven inactive ownership, recovers only captureable evidence, and reclaims only validated eligible workspaces with complete durable recovery artifacts; it may safely refuse or report unknown paths. It never overwrites partial/corrupt evidence or kills recorded PIDs. Back up retained evidence, inspect reasons and resolve actual failures; do not bypass ownership protections or manually edit the database.

### Deterministic acceptance map

All names below refer to permanent tests included in `RUST_TEST_THREADS=4 cargo test --offline --locked --workspace`; no installed OMP, credentials or paid calls are needed. The macOS release gate fetches locked dependencies first, then runs fmt, strict clippy and the bounded-parallel offline workspace suite. This command documents the gate, not a claim that default-parallel execution passed; see [verification](docs/VERIFICATION.md) for observed results.

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
RUST_TEST_THREADS=4 cargo test --offline --locked -p fabric-cli --test run
cargo test --locked -p fabric-runner
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
RUST_TEST_THREADS=4 cargo test --offline --locked --workspace
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

Workers and version probes clear the inherited environment and use private HOME/XDG directories. Connected native harnesses use their explicitly consented configuration without a manual profile. Advanced v1 callers may still select `--profile PATH` (or a task request's `profile`), bound to `omp` / `omp/19.1.0`, with explicit nonsecret connection references. For an already configured environment credential in a foreground invocation:

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

## Shared JSON API and native OMP controls

`fabric api --request /private/request.json --json` accepts versioned inline-prompt
batch requests through `fabric_core::api::spawn_many`, using the existing scheduler,
profiles, durable evidence and reclaim. Accepted failed batches return one complete
JSON response with a nonzero exit code; preflight failures return a typed error.
SIGINT/SIGTERM cancel owned foreground workers.

The OMP 19.1.0 extension exposes `fabric_spawn`, `fabric_status`,
`fabric_results`, `fabric_cancel` and bounded `fabric_wait`. Spawn returns early
durable IDs; controller disconnect stops observation, not admitted workers.
The legacy foreground `fabric` tool and `/fabric` command remain for debugging.
Build the workspace binaries, set `FABRIC_BINARY` and `FABRIC_ROOT`, then load
`integrations/omp/fabric.ts` with OMP's `-e` option. See
[control surfaces](docs/CONTROL_SURFACES.md) for native-context consent,
typed tasks, immutable sources, explicit local pstack routing and v1 compatibility.

Run the independent, provider-free extension checks with:

```sh
bun integrations/omp/conformance.ts "$PWD/target/debug/fabric"
bun integrations/omp/typed-conformance.ts "$PWD/target/debug/fabric"
bun integrations/omp/host-smoke.ts "$PWD/target/debug/fabric" /absolute/path/to/omp /absolute/path/to/om-pstack/omp
```

Installed OMP 19.1.0 control-plane verification passed native schemas/tools,
four independent fake workers, ordinary-context progress, disconnect/reconnect,
failure evidence, reclamation and cancellation; the actual plugin exposes
`/skill:swarm`. No agent/model turn ran. Live OMP/Codex workers and model-selected
pstack routing remain unverified; these checks do not authorize spending.
