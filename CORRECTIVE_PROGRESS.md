# Fabric v0.3 corrective progress

Baseline: `8e926f934f6508d114579e87d53e60a6d9ebdaf6`.

Execution contract: one bounded acceptance criterion at a time, failing behavior test before production changes, minimal implementation by subagents, parent review and independent focused/full verification before commits. Preserve the v1 CLI/API and all lifecycle/credential safety invariants. No paid model calls without authorization.

| Story | State | Next acceptance gate |
|---|---|---|
| C-00 baseline/evidence | TESTED_WITH_FAKE | Baseline reproduced: fmt/clippy, 181 tests/29 suites, Bun, actual OMP no-model two-worker proof with 28 artifact digests/reclaimed workspaces. |
| C-01 onboarding | TESTED_WITH_FAKE | Native discovery/consent, guided CLI, shared prelaunch revocation and explicit-profile compatibility; 190 tests/31 suites plus actual installed OMP read-only discovery. |
| C-02 async workers | NOT_VERIFIED | Durable fast four-worker submission, controller exit, cancel, queue restart/crash, idempotency and concurrent clients. |
| C-03 generic request | NOT_VERIFIED | Concise harness-independent tasks and typed unsupported capability/placement rejection. |
| C-04 immutable sources | NOT_VERIFIED | Explicit READY state/SHA remains runnable after source HEAD moves or checkout disappears. |
| C-05 OMP/pstack | NOT_VERIFIED | Typed tools and explicit local swarm bridge; real installed OMP provider-free four-worker proof. |
| C-06 recovery/storage | NOT_VERIFIED | Safe recoverable capture/reclaim and protected, actionable retained-workspace reporting. |
| C-07 release gates | NOT_VERIFIED | Exact CI/evidence gates and opt-in live scripts; live provider calls require approval. |

## C-00

Completed baseline reproduction and read-only compatibility/controller audits. `docs/PRODUCT_BOUNDARY.md` and `docs/VERIFICATION.md` record scope, migration/lifecycle invariants and exact verification. No production behavior changed. Missing rust-analyzer component installed; source fallback used for initial audit while LSP indexed. Live provider execution remains blocked on explicit spending authorization.

## C-01

Implemented native OMP/Codex discovery without auth-store reads, exact canonical exposure, private atomic consent, implicit prelaunch revocation and guided `init` / connect / revoke. Existing explicit profiles remain independent. RED preceded implementation; parent corrected test fixture version, canonical path/private mode and missing prompt/constructor assumptions before GREEN.

Focused checks, fmt, warning-denying clippy, all 190 tests across 31 suites, binary build and v1 Bun conformance passed. Actual installed OMP 19.1.0 `init --check` displayed native paths, metadata-only auth availability, source/READY status and next actions without creating the supplied absent storage root. No model/authentication call was made; live native authentication remains NOT_VERIFIED.
