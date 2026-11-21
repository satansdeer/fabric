# Fabric

A local-first Rust PoC for reusable agent workspaces. **Trusted local code only:** Fabric is not a sandbox or multi-tenant security boundary. macOS is the primary supported platform; Linux and other platforms are not currently claimed as supported.

## Implemented: FAB-01 and FAB-02

The seven-package Cargo workspace implements local diagnostics and an I/O-free domain model: validated typed IDs, idempotent lifecycle transitions with typed errors, terminal READY prepared states, and validated storage/retention policies. Project preparation, execution, persistence and cleanup commands are not implemented yet.

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

`fabric-domain` uses opaque caller-supplied IDs (1–128 ASCII letters, digits, `_` or `-`); it does not generate IDs or perform filesystem operations. State wire names follow the specification. Run transitions follow its queued/preparing/running/finalizing graph; terminal states cannot be rewritten. Repeating a state is a no-op. Workspace transitions never authorize deletion: the future storage/cleanup layers must still verify ownership, leases and durable recovery artifacts.

`StoragePolicy::default()` uses the specification's 20 GiB managed limit, 5 GiB free reserve, 2 GiB workspace limit, 10 MiB logs, 100 MiB artifacts and 24-hour recovery retention. Limits must be positive, combined per-run caps must fit the managed budget without overflow, and retention hours must fit in seconds. The reserve is independent of the managed budget. JSON deserialization applies defaults and rejects invalid limits, unknown fields and unsafe/unknown retention mode names; direct field edits require `validate()`. These are best-effort policies, not disk quotas or permission to discard uncaptured changes. Storage-root validation remains an I/O-layer responsibility.

## Checks

Tests run without network access, credentials, or an installed OMP. The discovery tests use executable marker scripts to ensure no discovered tool runs and check that fixture user data remains unchanged.

```sh
cargo test --locked -p fabric-domain --test domain
cargo test --locked -p fabric-cli --test doctor
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

After dependencies have been cached, add `--offline` to Cargo build/test/clippy commands to prohibit registry access as well. No paid model smoke run is part of these stories.
