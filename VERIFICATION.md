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

## FAB-11

- Reviewed OMP discovery/version validation, literal argv prompt boundary and explicit environment allowlist.
- Independently ran both OMP CLI regressions: passed, including offline JSONL/argv fixture and missing-executable refusal before allocation. Initial nonexistent `omp` test-target selection was corrected to the existing `run` target.
- Implementer final gates passed (73 tests), after disclosed test-only startup-budget and unique-fixture corrections.
- Real OMP help/version only; paid model smoke AT-15 remains unverified.

## FAB-12

- Reviewed shared capture budget, reserved truncation marker, concurrent stream draining, bounded event serialization and streamed checksums.
- Independently ran all five CLI worker tests: passed. The 100 MiB concurrent-output regression verifies combined persisted logs/events stay within 10 MiB and reopened metadata/checksums.
- Implementer fmt/clippy/workspace gates passed (79 tests). Output limits do not claim workspace quotas or artifact completion.

## FAB-13

- Reviewed fixed-size worker pool, once-per-batch prepared-state/version selection and durable ordered task results.
- Independently ran both batch CLI regressions: passed. Four external workers demonstrate peak two with cap two; mixed exit/prelaunch failures preserve individual outcomes and make batch unsuccessful.
- Implementer final fmt/clippy/workspace gates passed (83 tests).

## FAB-14

- Reviewed macOS owned/unreaped process-group identity, TERM/grace/KILL escalation, cancellation and bounded pipe draining.
- Independently ran three timeout and two cancellation regressions: passed. Tests include TERM-ignoring descendants, heartbeat cessation, Ctrl-C and preserved observed outcomes.
- Implementer final fmt/clippy/workspace gates passed (93 tests). No stale persisted PID is adopted for signalling; model cost controls remain best effort.

## FAB-15

- Reviewed pinned-base binary tracked diff, private synced artifact staging, no-replace publication, checksums and atomic terminal registration.
- Independently passed binary staged/unstaged patch round-trip and capture-failure retention regressions. Failed publication/registration, bounded capture and tampered output cannot claim terminal success.
- Implementer final fmt/clippy/workspace gates passed (96 tests). Workspaces remain retained: untracked archive and deletion authority are not yet implemented.

## FAB-16

- Reviewed bounded tar writing, no-follow descriptor traversal, ignored exclusion, symlink refusal and complete-capture cleanup eligibility.
- Independently passed real archive extraction (bytes, modes, links, unusual paths) and capture-failure retention regressions, including outside-link and size-limit cases.
- Implementer final fmt/clippy/workspace gates passed (98 tests). Complete manifests permit CLEANUP_PENDING; this story performs no deletion.

## Operator smoke — complete capture before reclamation

- Ran the actual CLI against the retained dirty-source fixture with two fake workers, concurrency two, prepared ignored dependencies and distinct same-path edits.
- Batch `b-r-1795314365266364000-74935-2` succeeded. Independently checked each published manifest, declared file size/SHA-256, tracked patch and restorable unique untracked bytes; sibling results and ignored dependencies are absent from archives. Original developer edit remained unchanged.
- Actual `storage gc --dry-run` classified both captured forks as reclaimable; `--apply` deleted both containers. Independent post-deletion checks still recovered exact unique archive bytes and verified all declared hashes, tracked patches and logs. The earlier uncaptured workspace was refused; developer checkout remained unchanged.
- These are offline fake-harness results, not a model-backed OMP demonstration.

## FAB-17 — verified capture precedes lease-protected reclamation

- Inspected descriptor-rooted cleanup, immutable intent authority, artifact revalidation, native read-only WAL inspection and lock release.
- Independent checks: four CLI GC regressions; two read-only store regressions; four cleanup/crash-resume regressions; four concurrent store regressions all passed.
- Parent caught inherited-flock leakage under concurrent forks. Explicit guard unlock and a live inherited-descriptor child regression fix it; busy cleaner refusal remains enforced. The final gate also exposed initial WAL-transition contention; bounded retry applies only to that idempotent transition.
- Implementer final fmt, strict clippy and workspace gate passed: 111 tests across 24 suites (`local://133.log`). Operator plan/apply evidence above independently proves artifacts survive actual workspace deletion.

## FAB-18 — bounded storage admission and owned-worker monitoring

- Reviewed native free-space metering, bounded descriptor-rooted logical scans, serialized admission/reservations, reference-preserving cache eviction and crash-resumable same-key rebuild.
- Independent checks passed: seven storage CLI regressions, cache-eviction crash/rebuild regression, and pressure-time READY reuse regression preserving setup count and tree identity.
- Parent review found admission preceded no-allocation READY reuse; fixed by using the existing locked identity/provenance path before fresh-tree admission.
- Actual CLI: storage JSON reports logical accounting/retention and zero leaked reservations; an impossible free-space reserve rejects before allocating any run.
- Implementer final fmt, strict clippy and workspace gate passed: 120 tests across 24 suites (`local://154.log`). Accounting remains best effort, not an APFS physical quota; stale crash reservations await FAB-19 reconciliation.

## FAB-19 — ownership-based crash reconciliation

- Reviewed persisted kernel-lock identity, macOS process start identity/group inspection, inherited worker ownership and atomic reconciliation. Persisted PIDs are inspected, never used as signal authority; unknown ownership remains conservative.
- Independent checks passed: seven lease unit regressions, expired-live-worker/crashed-owner regression, closed-inherited-descriptor orphan protection and GC live-lease regression.
- Actual CLI crash: killed only the owned supervisor after observing a unique worker write. Automatic GC reconciliation changed run `r-1795320617074185000-29155-0` to NEEDS_ATTENTION / RECOVERY_REQUIRED, retained exact unique bytes and released its proven-inactive storage reservation. A second pass left the logical database unchanged.
- The first crash probe killed before the fake worker wrote; its empty interrupted run was also retained and its inactive reservation released. No fabricated work or exit result was recorded.
- Implementer final fmt, strict clippy and workspace gate passed: 129 tests across 24 suites (`local://169.log`). Legacy/unverifiable owners cannot grant deletion or reservation-release authority.

## Final operator fixture — prepare, four workers, artifacts and reclamation

- Fresh root `/tmp/fabric-operator-demo-tbkuSc/final-data`: project add and two prepares reused exactly one READY state, `s-1795321010248896000-29990-0`. Ignored `.deps/ready` contained recipe-produced `ready`; the prepared tracked file retained committed bytes while the developer's dirty file remained unchanged.
- Actual `run-many --concurrency 2 --harness fake` completed four tasks successfully. All four workspaces were already RECLAIMED when inspected; subsequent GC dry-run/apply correctly reported `already_reclaimed`, not fresh deletion.
- Independently read all four durable manifests after workspace deletion: exact SHA-256 and length verified for all 52 published files, prompt digests and pinned prepared/source provenance matched, tracked patches contained only candidate edits, untracked archives contained exactly their candidate result, and ignored dependencies were excluded.
- Initial fixture assertions incorrectly guessed a SQLite column and a trailing newline in the recipe output; corrected the throwaway checks against actual API output and `printf ready`. No product change or weakened product test was needed.

## FAB-20 — automated acceptance verified; manual OMP gate blocked

- Reviewed read-only project/run/artifact inspection, stable ID ordering, explicit not-found errors and macOS CI configuration. Native private-directory validation and existing safe SQLite read-only opening do not migrate or reconcile state.
- Parent independently passed both new consumer-level CLI tests: four isolated runs with artifact recovery after reclaim, and live-WAL inspection with unchanged DB/WAL bytes and schema version.
- Actual final-fixture CLI proof passed: doctor reports supported macOS; project readiness and ordered/stable run JSON are inspectable; all four reclaimed runs remain discoverable. Verified SHA-256/length of all 56 registered artifacts (including four manifests) through `runs artifacts`; missing IDs fail with empty stdout and structured not-found stderr. All inspection commands left the logical database unchanged.
- Implementer final fmt, strict clippy and workspace gate passed: independently counted 131 tests across 24 suites (`local://175.log`). The workspace includes actual 100 MiB concurrent output against a shared 10 MiB durable cap. AT-01–14 runnable coverage is mapped in README; macOS CI is configured, not remotely executed.
- **Remaining blocker: AT-15 real OMP execution requires explicit spending approval, not received.** Installed version/help were probed, but no provider/model request was made. Manual procedure is documented; FAB-20 stays blocked rather than falsely verified, and `POC_READY_FOR_REVIEW` is not claimed.

## HN-01 — harness-neutral identity and worker evidence

- Two implementers produced behavioral RED before changes: unknown harness returned `invalid_arguments` rather than `unknown_harness` (`local://179.log`); opening a v8 database retained schema 8 instead of migrating to 9 (`local://178.log`).
- Parent independently passed domain/store tests (32 tests, `local://190.log`), then strict workspace/all-target clippy and all 138 workspace tests across 24 suites (`local://191.log`). Initial fmt check found layout-only issues; anchored corrections passed `cargo fmt --all -- --check`.
- Registry construction and executable discovery now belong to adapters. Shared core uses extensible IDs, persists immutable invocation provenance and observed process/event evidence, and exposes attempts through read-only inspection. Legacy Run fields and cleanup/capture behavior remain compatible; legacy runs acquire no invented attempts.
- HN-01/HN-08 automated compatibility is verified. The referenced `FABRIC_PSTACK_UPDATE_SPEC.md` was absent from supplied files and both repositories; its additional AC/RC gates cannot be claimed. Live OMP remains unexecuted without spending authorization.

## HN-02 — version/capability admission

- Independently reviewed request validation, adapter version pins and all-task common-plan admission before mutable core operations. Missing/configuration-required capabilities fail closed; non-finite/nonpositive/conflicting budgets fail before allocation.
- Implementer RED artifacts: `local://194.log`, `local://196.log`. Parent strict all-target clippy and 150 workspace tests across 24 suites passed (`local://203.log`); initial formatting-only failures were corrected with anchored edits and the subsequent fmt check passed.
- Actual CLI proof (`local://204.log`): list reports registered IDs, fake doctor returns all 13 capability statuses and exact pin, installed OMP doctor observes `omp/19.1.0`. Probes are bounded version-only invocations; no billing/model request occurred. Unsupported requests retain unchanged run/workspace/batch/attempt counts in permanent CLI coverage.

## HN-03 — mixed protocols and normalized capture

- Implementer RED: distinct fake adapters were unknown (`/tmp/fabric-hn03-protocols-red.log`); task-local harnesses were rejected because batch `--harness` was mandatory (`/tmp/fabric-hn03-core-red.log`).
- Two real fixture subprocess protocols now share batch admission, concurrency, prepared state, attempts, capture and cleanup. Permanent coverage checks model/usage attribution, absent usage, atomic rejection of a late unsupported request, native fatal with OS exit zero, post-cap fatal detection, malformed/oversized frames and recoverable OMP errors.
- Parent found and fixed strict artifact-verifier drift for the new capture evidence; normalized counters/native failure are compared during reclaim, including terminal reason preservation. Registered adapter expectations and the existing store capture fixture were migrated.
- Parent GREEN: `cargo fmt --all -- --check`, strict workspace/all-target Clippy and all 162 tests across 26 suites passed (`local://226.log`). No provider/model requests occurred. OMP normalization is source-checked against v19.1.0 and exercised by fixtures, not a live billed run.
