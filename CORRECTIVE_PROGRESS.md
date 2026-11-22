# Fabric v0.3 corrective progress

Baseline: `8e926f934f6508d114579e87d53e60a6d9ebdaf6`.

Execution contract: one bounded acceptance criterion at a time, failing behavior test before production changes, minimal implementation by subagents, parent review and independent focused/full verification before commits. Preserve the v1 CLI/API and all lifecycle/credential safety invariants. No paid model calls without authorization.

| Story | State | Next acceptance gate |
|---|---|---|
| C-00 baseline/evidence | TESTED_WITH_FAKE | Baseline reproduced: fmt/clippy, 181 tests/29 suites, Bun, actual OMP no-model two-worker proof with 28 artifact digests/reclaimed workspaces. |
| C-01 onboarding | NOT_VERIFIED | Authless discovery, exact native-context consent, owner-only selection, revocation and old-profile compatibility. |
| C-02 async workers | NOT_VERIFIED | Durable fast four-worker submission, controller exit, cancel, queue restart/crash, idempotency and concurrent clients. |
| C-03 generic request | NOT_VERIFIED | Concise harness-independent tasks and typed unsupported capability/placement rejection. |
| C-04 immutable sources | NOT_VERIFIED | Explicit READY state/SHA remains runnable after source HEAD moves or checkout disappears. |
| C-05 OMP/pstack | NOT_VERIFIED | Typed tools and explicit local swarm bridge; real installed OMP provider-free four-worker proof. |
| C-06 recovery/storage | NOT_VERIFIED | Safe recoverable capture/reclaim and protected, actionable retained-workspace reporting. |
| C-07 release gates | NOT_VERIFIED | Exact CI/evidence gates and opt-in live scripts; live provider calls require approval. |

## C-00

Completed baseline reproduction and read-only compatibility/controller audits. `docs/PRODUCT_BOUNDARY.md` and `docs/VERIFICATION.md` record scope, migration/lifecycle invariants and exact verification. No production behavior changed. Missing rust-analyzer component installed; source fallback used for initial audit while LSP indexed. Live provider execution remains blocked on explicit spending authorization.
