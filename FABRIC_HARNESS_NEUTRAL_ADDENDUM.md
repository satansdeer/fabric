# Fabric v0.2.1 — Harness-Neutral Architecture Amendment

**Status:** mandatory amendment to Fabric v0.2 / existing Rust PoC
**Implementation mode:** incremental, TDD, no rewrite
**Precedence:** this amendment supersedes any OMP-specific architectural assumptions in `FABRIC_PSTACK_UPDATE_SPEC.md`. The previous pstack acceptance tests remain important *scenario tests*, not the definition of Fabric's domain model.
**Product rule:** A user can control Fabric from one coding agent, choose a different coding agent to execute work, and place that execution locally or on a supported remote backend. Those three choices must remain independent.

## 1. Why this amendment exists

Fabric is not an `omp run` wrapper. OMP is the first **reference integration** because om-pstack gives useful acceptance scenarios. Fabric must be extensible to Factory **Droid**, Hermes Agent, Claude Code, OpenAI Codex CLI, Qwen Code CLI, Pi, OpenCode, and future agent harnesses without restructuring the core, public protocol, workspace model, or run storage.

Different harnesses use different CLI flags, authenticated account types, model catalogs, session formats, project/user configuration roots, skills/plugins, MCP support, event formats, permissions, and resume semantics. Fabric must not pretend these are identical. Behavioral promises are made only after version-specific capability negotiation and conformance tests.

## 2. Three independent axes

1. **ControlSurface** — how work is requested: OMP native extension, generic MCP server (where supported), CLI, SDK, or future harness-specific extension. A control surface submits Fabric requests but does not own execution state.
2. **HarnessAdapter** — what performs work: OMP, Droid, Hermes, Claude Code, Codex, Qwen, etc. It owns native config discovery, preparation, supported options, launch, event parsing, and session continuation where available. It does not own the lifecycle registry or managed workspace deletion.
3. **ExecutionBackend** — where the run executes: local trusted process (PoC), isolated local executor, Fabric Cloud, or customer-owned remote worker (BYOC). It owns placement/isolation/process supervision, not harness-specific prompt semantics.

Example: OMP control surface → Fabric API → Codex harness adapter → remote BYOC backend. Another example: Qwen control surface (through MCP if supported) → Fabric API → Hermes adapter → local backend.

Do not hardcode `omp`, `~/.omp`, `.omp/`, or any named vendor into `fabric-domain`, `fabric-core`, `fabric-project`, `fabric-workspace`, durable run schemas, worker service routes, or the placement backend. Such strings may appear only in dedicated adapters, feature tests, sample profiles, and scenario fixtures.

## 3. Required domain model

### Harness identity and profiles

- `HarnessId`: stable extensible identifier (`omp`, `factory-droid`, `hermes`, `claude-code`, `codex`, `qwen-code`); unknown IDs fail with a typed error.
- `HarnessVersion`: exact installed executable/SDK version (captured with probe evidence); the adapter declares supported version ranges.
- `AgentProfile`: harness ID/version, native config references or compiled **nonsecret** bundle, model selection policy, skills/plugins/MCP requirements, connection references, and digest. It must not store raw credentials.
- `HarnessCapabilityManifest`: version-specific capabilities observed by `probe()` or tested; includes supported, unsupported, and optional/conditional features with a reason. A feature advertised in upstream docs is not assumed available in the installed version.
- `Run`: durable logical task and result; independent of physical worker process, harness session ID, workspace path, or execution location.
- `WorkerAttempt`: one actual invocation tied to a run, with resolved harness version, model, profile digest, backend identity, event cursor, exit/termination info.

### Required capability vocabulary (first version)

`headless.run`, `events.structured`, `model.override`, `session.resume`, `session.followup`, `profile.import`, `skills.import`, `mcp.configure`, `execution.cancel`, `usage.report`, `tool.approval.noninteractive`, `workspace.external`, `filesystem.patch.capture`.

Each feature is one of `native`, `fabric-managed`, `unsupported`, or `requires-configuration`, with a structured reason and minimum supported version. `fabric-managed` is allowed only if Fabric itself actually provides that behavior. A capability plan is computed **before** accepting a run. If a requested guarantee cannot be provided, fail before any process starts. No silent fallback from `remote` to `local`, from requested model to default, or from durable to ephemeral.

## 4. HarnessAdapter contract

Treat the following as a conceptual interface; use idiomatic Rust (`async_trait` or boxed futures) and project conventions when implementing:

```rust
trait HarnessAdapter: Send + Sync {
    fn id(&self) -> HarnessId;
    async fn probe(&self, env: &ProbeContext) -> Result<HarnessProbe>;
    async fn plan(&self, req: &HarnessRequest, probe: &HarnessProbe) -> Result<HarnessPlan>;
    async fn launch(&self, plan: HarnessPlan, ctx: &WorkerContext) -> Result<HarnessProcess>;
    fn parse_event(&self, frame: &NativeOutputFrame) -> Result<Vec<AgentEvent>>;
    // Optional capability; returns unsupported unless implemented and tested.
    async fn resume(&self, session: NativeSessionRef, input: AgentInput,
                    ctx: &WorkerContext) -> Result<HarnessProcess>;
}
```

`probe` must be safe, bounded and non-billing by default. `plan` checks exact flags/version/config/auth/permissions and constructs an argument vector (never shell-interpolated). `launch` starts in the Fabric-owned workspace with explicit config/secret scope and no unauthorized ambient environment forwarding. `parse_event` normalizes agent-started, output, tool event (when visible), usage (when available), completed and failure; it retains raw bounded frames for debugging. `resume` must never fabricate resumed context by merely prepending a recap prompt.

An adapter may invoke a CLI subprocess, approved SDK, or local service protocol. The Fabric core must not know which method it uses. Profile layout, model/provider auth, MCP configuration and native permission controls are adapter responsibilities.

### Public request shape (conceptual)

```json
{
  "source": { "kind": "git", "commit": "<pinned SHA>" },
  "placement": "local",
  "tasks": [
    {"key":"implement","harness":"codex","profile":"coding", "prompt":"Implement the fix", "access":"edit"},
    {"key":"review","harness":"claude-code","profile":"review", "prompt":"Review independently", "access":"inspect"}
  ],
  "concurrency": 2,
  "background": true
}
```

`harness` is **per task**, not a batch-wide constant. The result records `harnessRequested`, `harnessResolved`, `harnessVersion`, `modelRequested`, `modelResolved` where verifiable, `profileDigest`, `placement`, `baseCommit`, artifacts, evidence, and typed failures. Batch orchestration never assumes all children use the same harness or model.

## 5. Inbound control surfaces are distinct from outbound harness adapters

An OMP extension is one **client** of Fabric; it is not the Fabric integration architecture.

- Keep the native OMP extension for the first pstack scenario.
- Provide a versioned core CLI/JSON API so any harness can invoke Fabric through a command/tool integration.
- Design a small Fabric MCP server as a second, generic control surface for harnesses that support MCP and can expose it as an agent tool; do not require MCP for execution, do not assume all harnesses implement every MCP transport.
- All control surfaces call the same run and policy services. No duplicated scheduling or security decisions.
- Remote execution does **not** require that the controlling harness has a native Fabric plugin. Native extensions can add superior UX/progress display but are not mandatory to submit work.

### Do not conflate native harness subagents with Fabric workers

Some harnesses already offer their own subagents or cloud sessions. Fabric worker fan-out and workspace ownership are independent; adapter-native subagents are opaque internal work unless an adapter explicitly exposes them. Never count one invocation's internal subagents as separately isolated Fabric workers without proof.

## 6. Configuration and credentials

Each adapter has explicit rules for: native global/project configuration, skills/extensions/plugin import, MCP topology, model selection, auth probe, approved credentials, and runtime HOME/XDG isolation. A user's working local harness is not automatically a reproducible cloud profile.

- **Local trusted mode:** may opt in to native home/config inheritance with clear diagnostics; label it `trusted-local`, not isolated/multi-tenant.
- **Remote/BYOC:** use an explicit profile manifest and credential connections, not entire HOME/XDG, `.env`, keychains, or opaque auth directories. Detect paths and services that cannot be reproduced remotely.
- **Model selection:** if the adapter cannot enforce or observe the requested model, report `MODEL_UNSUPPORTED` or `MODEL_UNVERIFIED`, as appropriate; do not claim model diversity in an arena.
- **Permissions:** preserve requested read/edit/publish distinctions in Fabric's outer policy; a harness-specific `--yolo`, trust, or approval flag may not be enabled by default. If an `inspect` run cannot be made effectively read-only in the selected backend, report that limitation or reject a hard read-only requirement.
- **Portability:** store an immutable native configuration fingerprint and a migration/compatibility report; don't flatten all harness settings into one lossy universal schema.

## 7. Initial implementation scope and order

**P0 contract (before generalizing pstack integration):**

1. Extract all OMP assumptions from the shared Rust worker request and durable data model; register adapters by HarnessId.
2. Implement fake adapters `fake-a` and `fake-b` with deliberately different flags, event formats and capability sets. Prove mixed-harness batches use one orchestration path.
3. Keep the existing OMP adapter working via the registry; do not rewrite its functioning execution logic.
4. Add one *different* real CLI adapter (prefer Codex or Qwen based on available test environment) that supports non-interactive execution and event parsing. No live model invocation in CI.
5. Produce `fabric harnesses list`, `fabric harnesses doctor <id> --json`, capability negotiation, and typed unsupported errors. Preserve `fabric run ... --harness omp` for backwards compatibility.
6. Integrate OMP's Fabric control extension against the generic API, not OMP-only Rust service endpoints. pstack `/swarm` is a consumer acceptance test, not the domain model.

**P1 integrations:** Factory Droid, Hermes, Claude Code, Codex/Qwen (whichever was not the P0 second adapter) as independently version-pinned adapter modules, with provider/session smoke tests run opt-in on machines where each is installed. No release should label an adapter fully supported until its conformance matrix passes.

**P2:** native extensions for other harnesses where they meaningfully improve UX, durable event continuation, automations, managed profile portability, cloud/BYOC deployments. Do not implement stubs and call them features.

## 8. TDD acceptance tests (additional to existing v0.2 AC/RC gates)

- **HN-01 separation:** `fabric-domain` and run persistence compile and unit-test without importing any OMP module or referencing OMP paths; source scan/test rejects introducing vendor config paths into core.
- **HN-02 mixed adapters:** `spawn_many` accepts tasks targeting `fake-a` and `fake-b` in one batch; each receives the right prompt, model, workspace, revision and permission policy; completed results preserve original harness identity.
- **HN-03 capability failure:** `fake-b` lacks `session.resume` and `model.override`; requests requiring either fail before worker launch with explicit error. No default model substitution.
- **HN-04 stream normalization:** fake-a emits JSONL, fake-b emits plain text and exit status; both normalize to ordered run events, bounded artifacts and reliable terminal status. Missing usage information remains unknown, not zero.
- **HN-05 credentials/config:** probe and launch do not log secrets or forward arbitrary credential environment variables; cloud profile cannot inherit a local HOME; absent authorization yields an actionable error.
- **HN-06 control-plane neutrality:** the same batch request can be submitted via CLI/JSON API and OMP extension and yields the same run semantics and result schema.
- **HN-07 second real harness:** opt-in smoke runs a non-OMP CLI in a private Fabric workspace with a harmless test prompt, records installed version and authentic capability results, and captures artifacts/cleanup. If unavailable, report `NOT_VERIFIED`, not `PASS`.
- **HN-08 compatibility:** existing `fabric run ... --harness omp` and v0.1 storage/cleanup tests continue to pass after refactor.
- **HN-09 multi-harness pstack:** a swarm request can select different harnesses per worker without the coordinator needing to write harness-specific launch scripts; incompatible selection is rejected explicitly.

These tests are additive to the existing v0.2 pstack tests. Tests that call live models are opt-in and budget-aware, not part of default CI.

## 9. Requirements for the developer agent / Ralph loop

- Audit the current checkout first; do not assume the earlier specification reflects implemented code accurately.
- Implement one scoped story per iteration, write a failing test first, change the smallest surface, run full relevant regressions, and record exact evidence.
- Implement *interfaces and two working adapters*, not six unfinished placeholders.
- Never let a requested provider, model, permission, placement or resume capability silently degrade.
- Maintain transparent coverage: `supported`, `limited`, `unsupported`, `not-verified` by harness version and backend.
- If a prior story conflicts with this amendment, document a migration ADR and preserve public backwards compatibility, or provide an explicitly versioned migration.

**Success criterion:** OMP is a first-class Fabric user, not a special case in Fabric's internals. Adding the next harness must require an adapter and conformance tests—not changes to the scheduler, run storage, workspace cleanup or public API.

## 10. References to validate against installed versions

- Factory Droid CLI/SDK: https://docs.factory.com/droid-cli/overview and https://docs.factory.com/sdk/typescript
- Hermes CLI: https://github.com/NousResearch/hermes-agent/blob/main/website/docs/reference/cli-commands.md
- Claude Code CLI: https://code.claude.com/docs/en/cli-reference
- Codex CLI: https://learn.chatgpt.com/docs/cli/reference
- Qwen Code headless: https://qwenlm.github.io/qwen-code-docs/en/users/features/headless/

Documentation demonstrates available integration styles, not guaranteed account entitlements, installed versions, license permissions, or behavioral parity. Verify all adapters with actual executable/version probes.
