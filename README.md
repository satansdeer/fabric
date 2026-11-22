# Fabric

A local-first Rust PoC for reusable agent workspaces. **Trusted local code only:** Fabric is not a sandbox or multi-tenant security boundary. macOS is the primary supported platform; Linux and other platforms are not currently claimed as supported.

## Implemented: FAB-01 through FAB-09

The seven-package Cargo workspace implements local diagnostics, an I/O-free domain model, SQLite persistence, pinned local Git project registration, validated preparation recipes/fingerprints, reusable private prepared snapshots, full-tree forks and durable run-owned workspace orchestration. Successful preparation publishes a durable read-only READY tree, including ignored dependencies. Worker execution and cleanup commands are not implemented yet.

Requires Rust 1.85 or newer. Resolved dependencies are pinned in `Cargo.lock`.

```sh
cargo build --locked -p fabric-cli
./target/debug/fabric doctor
./target/debug/fabric doctor --json
# Or run directly from the workspace:
cargo run --locked -- doctor --json
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

Registration accepts a working tree or its subdirectory, including detached HEAD, and stores the canonical repository root and full committed HEAD SHA. The existing schema's `default_ref` holds this exact SHA, not a moving branch. Status reads persisted metadata without consulting the checkout; JSON contains `project_id`, `name`, `source_path`, `default_ref`, `source_commit_sha` and `created_at` (Unix seconds). Its SHA stays pinned after HEAD moves or the source directory becomes unavailable. Project names are nonblank strings and unique within one root.

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

Ordinary copy requires explicit admission of the **whole measured logical tree size**, before creating a forced-copy destination or writing a clone fallback. Clone fallback is allowed only for unsupported/cross-filesystem clone operations, not arbitrary I/O errors. A zero allowance disables copying; CoW may still succeed. This allowance is not a free-space check or hard quota; policy-based admission/monitoring belongs to the later storage story. Copy is the deterministic correctness-test path; no instant-fork performance claim is made on arbitrary filesystems.

Calling this low-level backend directly produces **unregistered staging forks**, not runnable managed workspaces. Failed or partial destinations remain as evidence and are never overwritten or deleted; their IDs cannot be retried. Use the ownership orchestration below before any future worker launch. Directory existence, a returned `PreparedFork`, and a caller-supplied workspace ID never authorize execution or deletion.

### Register an owned workspace (library use case)

`fabric_core::workspace::create_workspace(root, backend, &request, copy_limit_bytes)` reuses the existing local backend and SQLite store. `WorkspaceRequest` supplies stable Run/Workspace IDs, a persisted prepared-state ID, harness name, prompt digest, creation time and startup reservation expiry (Unix seconds). The Run/Workspace pair commits atomically before the backend can touch its destination; reciprocal foreign keys, unique Run/Workspace IDs and a unique managed path enforce exactly one owner. The Run enters PREPARING, not RUNNING: no agent process is launched.

The same `fork-staging/<workspace-id>/tree` becomes managed only after successful forking, an owner-only fsynced container `.fabric-owner` binding the canonical root and Run/Workspace/prepared-state IDs, and a durable fork-completion update. `RunnerWorkspace` has private fields and is returned only after fresh ownership/path/marker validation. `runner_workspace(root, &run_id)` reopens the store and validates again; use it immediately before future process launch. Unknown roots never get a new database here. Missing/incomplete/failed ownership, outside or non-exact recorded paths, root/database/directory/marker symlinks, hardlinked markers/databases, permissive managed metadata and incorrect markers are refused without deleting anything. Safe relative links inside project contents remain supported; neither this validator nor the backend is a same-user adversarial sandbox.

Fork failures atomically retain the stable Run as NEEDS_ATTENTION with `workspace_fork` diagnostics and its Workspace as RECOVERY_REQUIRED. Existing/partial files and reservation leases remain intact; no retry overwrites or adopts them. An interrupted pending record is not runner-ready, even if a complete tree happens to exist. No process states or exit codes are fabricated.

`lease_expires_at` currently means a **startup reservation**, supplied explicitly and persisted before forking. Missing/expired reservations cannot yield a runner-ready workspace. It is not a heartbeat or proof of a live owner; this story does not renew leases or reconcile crashes. Lease-owner identity/renewal/reconciliation belong to FAB-19, and expiry alone never authorizes cleanup. Markers and ownership do not replace the future active-lease, artifact-capture and deletion checks.

`fabric-domain` uses opaque caller-supplied IDs (1–128 ASCII letters, digits, `_` or `-`); it does not generate IDs or perform filesystem operations. State wire names follow the specification. Run transitions follow the queued/preparing/running/finalizing graph, with PREPARING → NEEDS_ATTENTION for fork failures before execution; terminal states cannot be rewritten. Repeating a state is a no-op. Workspace transitions never authorize deletion: the future storage/cleanup layers must still verify ownership, leases and durable recovery artifacts.

`StoragePolicy::default()` uses the specification's 20 GiB managed limit, 5 GiB free reserve, 2 GiB workspace limit, 10 MiB logs, 100 MiB artifacts and 24-hour recovery retention. Limits must be positive, combined per-run caps must fit the managed budget without overflow, and retention hours must fit in seconds. The reserve is independent of the managed budget. JSON deserialization applies defaults and rejects invalid limits, unknown fields and unsafe/unknown retention mode names; direct field edits require `validate()`. These are best-effort policies, not disk quotas or permission to discard uncaptured changes. Storage-root validation remains an I/O-layer responsibility.

`fabric-store::Store::open` opens a file-backed SQLite database with WAL, full synchronous commits, foreign keys, a five-second busy timeout, and transactional versioned migrations. Its parent directory must exist; new database files have owner-only permissions on Unix. Unsupported schema versions fail closed. Typed project, prepared-state, run and workspace records survive reopening. Prepared-state keys are unique per project, READY metadata cannot be updated, and runs require READY state with matching project/commit provenance. Run/workspace rows have reciprocal one-to-one foreign keys and unique managed paths; both commit before the caller may use a workspace. Atomic state compare-and-set validates the domain transition graphs, rejects stale competing intent, and accepts exact replay without rewriting timestamps.

The store itself does not resolve Git, prepare files, launch processes, validate canonical filesystem ownership, authorize deletion, or reconcile interrupted workers. Project registration, prepared-tree forks and core ownership orchestration validate private roots and managed ownership at their boundaries; deletion authorization remains unimplemented. Stored managed-path uniqueness is textual; core additionally requires the exact canonical managed descendant and matching private marker. Lease expiry and lifecycle state alone never authorize cleanup. Byte counts must fit SQLite's signed 64-bit integer range. No artifact registration or cloud adapters are implemented.

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
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

After dependencies have been cached, add `--offline` to Cargo build/test/clippy commands to prohibit registry access as well. No paid model smoke run is part of these stories. Project/preparation tests use temporary local Git repositories, including dirty and newly committed sources, ignored dependencies, literal argv/environment handling, hook/config suppression, retained failures, process-group/total timeouts, fail-closed managed paths, once-only cache side effects, competing CLI processes, and interrupted-stage reconciliation. Fork tests force admitted ordinary copies, compare every tree entry/content and independent inode, preserve executable/chained symlink behavior, concurrently edit siblings without base leakage, and reject insufficient admission, existing destinations and source/destination tampering.
