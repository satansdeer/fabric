# Fabric v0.3 corrective progress

Baseline: `8e926f934f6508d114579e87d53e60a6d9ebdaf6`.

Execution contract: one bounded acceptance criterion at a time, failing behavior test before production changes, minimal implementation by subagents, parent review and independent focused/full verification before commits. Preserve the v1 CLI/API and all lifecycle/credential safety invariants. No paid model calls without authorization.

| Story | State | Next acceptance gate |
|---|---|---|
| C-00 baseline/evidence | TESTED_WITH_FAKE | Baseline reproduced: fmt/clippy, 181 tests/29 suites, Bun, actual OMP no-model two-worker proof with 28 artifact digests/reclaimed workspaces. |
| C-01 onboarding | TESTED_WITH_FAKE | Native discovery/consent, guided CLI, shared prelaunch revocation and explicit-profile compatibility; 190 tests/31 suites plus actual installed OMP read-only discovery. |
| C-02 async workers | DONE | Eight async lifecycle scenarios and full compatibility gates passed; schema v10 preserves historical evidence. |
| C-03 generic request | DONE | Eight generic-worker scenarios, existing v1 SIGTERM/privacy regression, and full compatibility gates passed: 211 tests/33 suites plus Bun conformance. |
| C-04 immutable sources | NOT_VERIFIED | Explicit READY state/SHA remains runnable after source HEAD moves or checkout disappears. |
| C-05 OMP/pstack | NOT_VERIFIED | Typed tools and explicit local swarm bridge; real installed OMP provider-free four-worker proof. |
| C-06 recovery/storage | NOT_VERIFIED | Safe recoverable capture/reclaim and protected, actionable retained-workspace reporting. |
| C-07 release gates | NOT_VERIFIED | Exact CI/evidence gates and opt-in live scripts; live provider calls require approval. |

## C-00

Completed baseline reproduction and read-only compatibility/controller audits. `docs/PRODUCT_BOUNDARY.md` and `docs/VERIFICATION.md` record scope, migration/lifecycle invariants and exact verification. No production behavior changed. Missing rust-analyzer component installed; source fallback used for initial audit while LSP indexed. Live provider execution remains blocked on explicit spending authorization.

## C-01

Implemented native OMP/Codex discovery without auth-store reads, exact canonical exposure, private atomic consent, implicit prelaunch revocation and guided `init` / connect / revoke. Existing explicit profiles remain independent. RED preceded implementation; parent corrected test fixture version, canonical path/private mode and missing prompt/constructor assumptions before GREEN.

Focused checks, fmt, warning-denying clippy, all 190 tests across 31 suites, binary build and v1 Bun conformance passed. Actual installed OMP 19.1.0 `init --check` displayed native paths, metadata-only auth availability, source/READY status and next actions without creating the supplied absent storage root. No model/authentication call was made; live native authentication remains NOT_VERIFIED.

## C-02 acceptance

- RED: the four-worker async CLI acceptance test compiled, then failed on the missing `submit` command before production changes (0 passed, 1 failed, 6 filtered; 11.19s). Raw evidence: `local://291.log`.
- First vertical gate passed: four distinct workers survive submitter exit; fresh clients retrieve durable results, verified artifact sizes/digests, and reclaimed-workspace metadata (1 passed, 6 filtered; 6.39s).
- macOS regression found and fixed: accepted Unix sockets inherited nonblocking mode, truncating larger terminal replies. An owned foreground-manager probe isolated the failed terminal `results` response; accepted streams now explicitly become blocking before bounded timeouts.
- All eight async CLI acceptance checks passed, including a globally bounded pool across concurrent submissions; atomic admission, queued cancellation/restart, v9 migration, frame bounds and consent-stamp checks also passed. Legacy migration fixtures now remove v10 async triggers/tables before reconstructing old schemas; ownership/evidence assertions are unchanged. Both formerly failing fixture checks passed.
- Final gate: `cargo fmt --all -- --check`; warning-free offline workspace Clippy; `cargo test --offline --workspace` (203 passed, 32 suites, 128.95s); offline workspace binary build; Bun OMP conformance. All passed in 149.71s including the two focused migration checks. Raw evidence: `local://303.log`. No provider calls. Next: C-03 generic worker requests with optional native-default models and explicit unsupported-placement rejection.

## C-03 acceptance

- First criterion: a concise version-free request starts two different fake event protocols, inherits native-default models, returns durable handles before completion, and exposes verified retained artifacts after reclaim.
- RED: `cargo test --offline -p fabric-cli --test workers generic_fake_protocols_return_early_handles_and_persist_results -- --exact` compiled, then failed on unrecognized `workers` (0 passed, 1 failed, 1.17s; 6.43s total). Raw evidence `local://306.log`. Approved optional native/explicit execution contexts and enforceable per-task limits; no portable skill/plugin/MCP fields in the new contract.
- First GREEN: after fixing a `ModelPolicy::NativeDefault {}` construction error, the same focused command passed (1 passed, 8 filtered, 11.30s test runtime; 21.49s total). Legacy file prompts retain per-task prelaunch failures through the same internal execution path. Remaining compatibility and negative cases are not yet verified.
- Remaining eight-test gate: 4 passed, 4 failed (28.24s test runtime; 31.32s total; raw `local://310.log`). Model/capability rejection incorrectly started the manager before failing. Two other failures were test assumptions: v1 still requires `concurrency`, and `copy_limit_bytes` bounds byte-copy fallback, not successful APFS clones. Correct the real preflight path and exercise the actual fallback budget without weakening its contract.
- Follow-up focused gate: 7 passed, 1 failed (16.34s test runtime; 18.67s total; raw `local://313.log`). Preflight now precedes manager startup and native/v1 paths pass. Invalid timeout options still use runtime `output` error classification instead of `invalid_arguments`; shared option validation is the remaining fix. The incorrect APFS fallback test was removed; existing forced-copy backend tests cover the fallback algorithm, not native runtime fallback end-to-end.
- Invalid-option regression now passes; formatting and strict Clippy pass. The subsequent full workspace gate found an existing v1 SIGTERM/privacy regression: `batch_tasks.prompt_file` points to a missing file while its worker is active (`crates/fabric-cli/tests/run.rs:4189`; raw `local://314.log`; 115.93s total). Restore real private prompt-spool provenance and lifetime before continuing.
- Final GREEN: the focused v1 SIGTERM/privacy regression passed (5.07s), then formatting, strict offline workspace Clippy, all 211 tests across 33 suites (166.57s), binary build and Bun conformance passed. Combined command: 203.77s, raw `local://317.log`. The spool fix uses the canonical storage root without an extra `storage` component. No provider calls. Native runtime byte-copy fallback remains NOT_VERIFIED on APFS; existing forced-copy backend tests pass.
