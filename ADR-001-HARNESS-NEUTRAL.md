# ADR-001: Harness-neutral execution with v0.1 compatibility

Status: accepted for the v0.2.1 amendment.

## Context

The working v0.1 checkout has a logical Run whose public record also contains physical worker/workspace fields. Its CLI selects one harness for a batch. The harness-neutral amendment requires independent control surface, worker adapter and execution backend choices, and per-task selection without a storage/workspace rewrite.

The amendment references `FABRIC_PSTACK_UPDATE_SPEC.md`, but that document was absent from the supplied ZIP, Fabric checkout and inspected `satansdeer/om-pstack` tree. Its unspecified AC/RC gates cannot be claimed as verified. The supplied addendum and HN stories define this implementation; original FAB story evidence remains unchanged.

## Decision

Keep existing Run fields and v0.1 CLI/storage contracts. Add separate WorkerAttempt records through an additive schema migration; do not manufacture attempts for historical runs. New invocation evidence records the requested/resolved harness, exact executable version, model evidence where available, profile digest, backend, event cursor and observed termination independently of legacy Run fields.

Resolve extensible harness IDs through an adapter registry. Native argument construction, configuration discovery, capability/version support and event translation belong to adapters. Shared policy, preparation, workspace ownership, storage admission, process supervision, capture and cleanup remain shared.

Version the generic JSON request/result protocol independently of native harness protocols. CLI and OMP control integration must submit to the same policy and lifecycle services. Select harnesses per task, not through coordinator-generated vendor launch scripts.

Only the existing trusted-local execution boundary is implemented. Reject unsupported placement, hard read-only guarantees and capabilities before workspace/worker allocation; do not emulate remote isolation or session continuation. Native HOME/config inheritance requires an explicit trusted-local profile policy. No default permission bypass or paid model smoke.

## Consequences

Legacy Run fields remain compatibility projections, not a claim that a logical run is a process. Read-only inspection must keep working on older schemas without migration. Working adapter conformance is distinct from live provider verification. Original AT-15 and unavailable second-harness live smoke remain explicit unverified gates, not fabricated acceptance.
