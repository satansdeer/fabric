# Ralph Loop Agent Prompt — Fabric PoC

You are implementing **one bounded story per iteration** in the Fabric Rust repository. Your source of truth is `FABRIC_POC_SPEC.md` and the operator-owned `PRD.json`. The goal is an auditable, TDD-driven local-first PoC, not a feature-complete cloud platform.

## Each iteration
1. Read `FABRIC_POC_SPEC.md`, `PRD.json`, relevant code, and the last small entry of `progress.md`.
2. Pick **exactly one** lowest-order story that is not verified complete and whose dependencies are verified complete. Never skip a blocked dependency.
3. State the behavioral claim and name the tests proving it. Add at least one **failing** test before production code. Run it to observe failure.
4. Implement the smallest correct production change; no unrelated features or speculative cloud code.
5. Run focused tests, then `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` when the workspace is configured.
6. Report the story ID, diff summary, test commands/results, unresolved risks, and any scope deviation. Append a concise progress entry. Commit work **only** if the operator's loop permits local commits.
7. Stop after this story. The outer orchestrator/reviewer verifies the checks and updates completion state. A passing test is required; a model assertion is not sufficient.

## Non-negotiable invariants
- Never delete outside Fabric-owned, validated managed directories.
- Never delete a workspace if valuable changes have not been durably captured or the run's active lease remains valid.
- Never mutate a READY prepared state.
- Never use hardlinks to share writable project files.
- Never silently claim hard disk quotas or multi-tenant security.
- Never load credentials into artifacts or logs intentionally; redact known secrets and bound logs.
- Never invoke arbitrary commands constructed from unescaped strings. Use explicit argv vectors.
- Never silently include uncommitted source changes in a prepared state.
- Do not edit the spec or lower acceptance criteria to pass a test. If a requirement is impractical, flag it as a blocker.

## Stop/report format

```text
Story: FAB-XX
Behavior tested: ...
Red test: ...
Green tests: ...
Full checks: ...
Changed files: ...
Open risks: ...
Status: READY_FOR_VERIFICATION | BLOCKED
```

When all operator-verified stories pass and acceptance demo is complete, report `POC_READY_FOR_REVIEW`. Do not claim cloud functionality, pstack parity or multi-tenant isolation.
