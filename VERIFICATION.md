# External verification

Operator platform override: macOS first. Verified on arm64 macOS; Rust/cargo 1.91.0. The parent orchestrator, not the implementer, owns this record and PRD status.

## FAB-01

- Reviewed doctor implementation, command/error dispatch and all five integration tests.
- Independently ran `cargo test --offline -p fabric-cli --test doctor`: 5 passed.
- Launched `target/debug/fabric doctor --json`: valid JSON, `os=macos`, `arch=aarch64`, `supported=true`, Git and OMP discovered.
- Launched `target/debug/fabric --help`: doctor/help/version exposed.
- Copied only Cargo manifests/lockfile and crates to a fresh temporary source tree; `cargo test --offline --workspace --manifest-path <temporary>/Cargo.toml` passed.
- Implementer supplied observed RED test evidence and passing fmt/clippy/workspace checks.

## Final demonstration constraint

`omp --version` reports `omp/19.1.0`; `omp --help` confirms the headless JSON interface. No paid/model invocation has been authorized or performed. The final real-OMP acceptance smoke remains a separate approval-gated requirement; do not claim the PoC demonstrated without it.

## FAB-02

- Reviewed typed-ID validation, pure state-transition graphs and policy validation/deserialization.
- Independently ran `cargo test --offline -p fabric-domain --test domain`: all 8 tests passed, including illegal transitions, immutable READY states and invalid/overflowing limits.
- Implementer supplied observed RED evidence and passing fmt/clippy/workspace checks (13 tests).

## FAB-03

- Reviewed schema constraints, transactional migration/publication, reciprocal ownership foreign keys and compare-and-set transitions.
- Independently ran `cargo test --offline -p fabric-store`: 10 passed; doctor integration tests: 5 passed.
- Verified checks exercise concurrent creation/migration, rollback, immutable READY records and persistence across reopen.
- Authorized a prerequisite test-only fix: wall-clock/PID temporary names collided under parallel macOS doctor tests; implementer added a uniqueness counter.
- Implementer supplied RED evidence and passing fmt/clippy/workspace checks (23 tests).

## FAB-04

- Reviewed local-only Git resolution/environment isolation, registration, durable source pin and private-root checks.
- Independently ran seven project CLI tests: all passed.
- Actual CLI registered the independently prepared dirty fixture, warned without importing edits, and returned the committed SHA `ed5d65262395a2dc5f95c457ca223c62956c2295` in add/status JSON.
- Implementer supplied RED evidence and passing fmt/clippy/workspace checks (30 tests).

## FAB-05

- Reviewed strict recipe validation and streaming domain-separated SHA-256 identity; selected environment values are not returned as metadata.
- Independently ran all four recipe tests: passed (canonical ordering/defaults, invalidation, argv boundaries and invalid-input rejection).
- Implementer supplied RED evidence and passing fmt/clippy/workspace checks (34 tests).

## FAB-06

- Independently passed seven preparation tests and actual CLI preparation against the dirty operator fixture: ignored marker present, committed-only base, original dirty checkout unchanged.
- Reviewed preparation process groups/timeouts, private clone, freeze and durable READY publication.
- Initial review found regular-only freezing excluded ordinary dependency symlinks; required correction before acceptance. Independently passed internal-link/declared-PATH and escaping-link regressions after fix; outside target unchanged.
- One initial test command targeted the wrong crate; corrected to CLI. One narrow name filter selected zero tests; corrected using observed test names. No passing claim based on either.
- Implementer final fmt/clippy/workspace gates passed (43 tests).

## FAB-07

- Reviewed kernel-lock lifetime, cached provenance/ownership validation and fail-closed orphan preparation handling.
- Independently passed all 15 preparation tests, including reuse, invalidation, cross-process serialization and interrupted staging.
- Actual CLI repeated the operator fixture preparation and returned the original state ID unchanged.
- Operator decision: uncertain FAILED keys retain evidence and refuse same-key retry; changing fingerprint inputs permits a new attempt.
- Implementer final fmt/clippy/workspace gates passed (49 tests).

## FAB-08

- Reviewed full-tree copying, macOS `fclonefileat`, no-follow source checks, whole-tree copy admission and independent writable fork permissions.
- Independently ran workspace tests: six passed, including forced-copy dependencies/symlinks, distinct inodes and base/sibling isolation.
- Fork staging has no cleanup authority; run ownership is the next story.
- Implementer final fmt/clippy/workspace gates passed (55 tests).

## FAB-09

- Reviewed durable ownership-before-backend ordering, marker binding, fresh runner validation and retained failure handling.
- Independently ran eight core ownership/isolation tests: all passed.
- Safety clarification: a fork that never starts a process may transition PREPARING directly to NEEDS_ATTENTION, rather than falsely recording RUNNING. Normal execution retains the specified lifecycle.
- Implementer final fmt/clippy/workspace gates passed (63 tests).

## FAB-10

- Reviewed foreground prompt snapshot/provenance, current committed-HEAD selection, launch ordering and retained failures.
- Independently ran both CLI worker regression tests: passed, including nonzero exit and unprepared-HEAD refusal.
- Actual operator fixture CLI launched external fake worker, read ignored `.deps/ready`, returned SUCCEEDED/exit 0 and exact committed SHA. Run: `r-1795309396406779000-25375-0`.
- Implementer final fmt/clippy/workspace gates passed (67 tests). No cleanup eligibility or artifact claim yet.
