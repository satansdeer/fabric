# Scope removals, deferrals and compatibility

C00–C07 are an incremental correction, not a runtime rewrite. This inventory distinguishes removed **product assumptions** from deleted code. It does not claim the legacy schemas, fixture adapters or enforcement types were deleted.

## Removed from the normal user contract

| Former assumption | Current contract and reason |
| --- | --- |
| A worker requires handwritten credential/profile JSON on every invocation | `init` / `harness connect` explicitly select a native context once. Harnesses own auth; Fabric stores private consent references, not copied secrets. Advanced explicit profiles remain available. Consent does not authorize provider spending. |
| Portable `skills`, `plugins`, `mcp` lists import a universal configuration | Absent from generic `WorkerTask` / `ExecutionContext`. Configure these in the selected native harness. Legacy `AgentProfile` fields still parse, but nonempty lists fail with a native-configuration migration message; they are never silently ignored. |
| `/fabric <raw JSON>` is the expected swarm UX | Typed `fabric_spawn/status/results/cancel/wait` and `workers spawn` are the normal controls. Raw v1 remains diagnostic compatibility, not the advertised delegation contract. |
| Every launch must consult the developer checkout | Explicit READY-state/commit selection pins immutable prepared bytes and remains usable without that checkout. `current-head` remains an explicit convenience; missing/unprepared HEAD never falls back to an older revision. Dirty changes are excluded. |
| Foreground batch handles imply survival after controller exit | Durable `workers spawn` / `submit` admission and the local manager provide that behavior. Foreground compatibility commands retain their documented lifetime/cancellation limits. |
| Cursor `Task` fields, native OMP `task`, and Fabric runs are interchangeable | No Cursor emulation or native-tool monkeypatch. The om-pstack OMP adapter routes only an explicitly requested local Fabric swarm through typed controls; ordinary `/skill:swarm` keeps native OMP `task`. |
| Native tool registration or skill discovery proves live model routing/cloud parity | C05 proves installed OMP control-plane behavior with fake workers and no model turn. Model-selected routing and actual OMP/Codex execution require separate opt-in live evidence. No remote backend exists. |

## Retained compatibility and enforcement

- **`run`, `run-many`, `api --request`:** foreground diagnostics and existing callers; they preserve the shared worker execution/capture/reclaim machinery, not a second compatibility engine. `run-many` keeps file-based tasks, relative-to-manifest prompt paths, optional task-local harnesses and complete request overrides. The minimal two-field task manifest remains valid when a default harness is supplied.
- **Strict v1 `spawn_many` JSON:** `version:1`, inline prompts, `default_harness`, per-task complete `request`, concurrency and bounded options remain supported. `submit` accepts this same wire shape with asynchronous `{accepted,batch_id,run_ids}` semantics; it is not the foreground response. Unknown versions/fields reject rather than being silently translated. Generic requests do not require v1 envelopes.
- **Legacy OMP `fabric` tool and `/fabric`:** useful literal-JSON debugging/compatibility surfaces, still foreground. Typed controls are the durable route. No invented background/resume guarantee for legacy calls.
- **`AgentProfile`, `ModelPolicy`, native config/credential connection references and profile evidence:** active version, model-policy, authorization and fingerprint enforcement. Empty legacy portable lists/defaults remain parseable for v1; nonempty unsupported imports reject. Explicit `env` connections remain a foreground advanced option; asynchronous admission rejects them rather than persisting secret values. Explicit profiles are independent authorization and are not revoked by removing an unrelated native-context consent record.
- **Capability/placement fields and exact version pins:** truthful rejection boundaries, not speculative implementations. Unsupported model override, hard cost cap, read-only guarantees, resume, remote/cloud/BYOC requests fail before allocation. OMP's separately documented foreground best-effort budget is not a universal hard billing cap.
- **`fake`, `fake-a`, `fake-b`, including current discovery visibility and `--fake-binary`:** retained runnable fixtures for deterministic lifecycle and adapter conformance. They are not real provider proof; fixture-only discovery hiding is deferred, not implemented.
- **Persisted history and old manifests:** source/run/attempt/artifact identities remain evidence. Legacy manifests without profiles stay readable. Schema v12 adds immutable recovery context; older or incomplete evidence is retained, not retroactively invented. Inspect-default `recover RUN_ID --json` never launches/requeues a worker; `--apply` needs positive inactive authority, exact ownership and complete capture. Partial, unknown or corrupt evidence stays protected.

Existing regression coverage includes `v1_submit_keeps_its_response_and_nonempty_portable_profiles_require_native_migration`, `run_many_preserves_mixed_exit_and_prelaunch_failure_and_rejects_invalid_input`, `registered_profile_digest_verification_is_strict_and_legacy_absence_is_compatible`, and the raw-v1 OMP conformance scenario. See [verification](VERIFICATION.md) for what was actually run; this inventory is not a new test report.

## Deferred / never shipped

Cloud/BYOC execution, distributed scheduling, credential brokerage/vaults, portable skill/plugin/MCP import, custom MCP transport, session resume, side-effecting retries, workflows/DAGs, Cursor equivalence, additional harness families and Linux support remain outside this release. Unsupported fields are guardrails, not hidden implementations. Generic MCP mapping in [control surfaces](CONTROL_SURFACES.md) is design-only.

No artifact-history TTL/purge or automatic deletion of uncertain recovery data is shipped. Storage reports run IDs, actions and logical bytes, not APFS-exclusive usage or a hard quota. Expiration is not deletion authority. No cosmetic crate restructuring or replacement database was introduced for these scope decisions.

Before replacing a binary/schema, drain active/queued work and stop the **old** manager with `fabric --root "$ROOT" manager stop --json`. Successful stop releases endpoint and ownership before acknowledging; never kill active work to force an upgrade. See [product boundary](PRODUCT_BOUNDARY.md) and [live smoke prerequisites](LIVE_SMOKES.md).
