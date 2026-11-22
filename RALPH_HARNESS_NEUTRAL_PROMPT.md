# Ralph instructions — Fabric v0.2.1 harness-neutral amendment

Read `FABRIC_POC_SPEC.md`, `FABRIC_PSTACK_UPDATE_SPEC.md`, and `FABRIC_HARNESS_NEUTRAL_ADDENDUM.md` in that order; the last document supersedes OMP-specific architecture assumptions. Read `PRD_HARNESS_NEUTRAL.json` and prior progress. Audit the **actual checkout**, not a presumed implementation.

Choose one eligible HN-* story per iteration (dependencies satisfied). Write and observe a failing test, implement minimally, run focused and existing regressions, and record exact commands, outcomes, files, and known limitations. Keep full backwards compatibility for working OMP CLI flows.

Never report support for an untested harness, model override, native session resume, or remote security boundary. CLI capability probes should be offline and non-billing. Live harness calls are opt-in. If a dependency is unavailable, mark the smoke test NOT_VERIFIED and continue deterministic coverage.

Do not implement six empty harness classes. Prove the design with OMP + one non-OMP adapter, including a mixed fake-harness batch. Keep the controlling harness, worker harness, and execution backend independent. An outside verifier—not the implementing agent's assertion—decides acceptance.
