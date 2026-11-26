# Operator-only live smoke guide

**Default: `LIVE_HARNESS_VERIFIED = NOT_VERIFIED / BLOCKED`.** These are recipes, not executed evidence. No provider/model calls are authorized by this document. Run sections separately; never paste the whole guide as a script. Missing auth, versions, model evidence or authorization is a blocker, not permission to substitute a model, harness or fake worker.

## Boundary and prerequisites

Provider-free checks cover fake-worker lifecycle, adapter conformance, native OMP 19.1.0 tool registration/execution, actual `/skill:swarm` discovery and explicit local routing controls. C05 did **not** run an agent turn, demonstrate a model choosing the skill/tool, authenticate a worker, or prove cloud execution. See [verification](VERIFICATION.md) for observed commands/results and [control surfaces](CONTROL_SURFACES.md) for provider-free commands.

For a live attempt, the operator must supply:

- macOS, built Fabric binaries, Python 3, Git and compatible installed harnesses: native OMP advertising the required print/JSON CLI options, or `codex-cli 0.171.0`. Record OMP's exact observed version; CLI discovery is not future wire-protocol verification. Supply absolute `FABRIC_BINARY`; do not download or substitute a different version mid-attempt.
- The **real selected native configuration directories**: `PI_CODING_AGENT_DIR` for OMP and `CODEX_HOME` for Codex, only for the harnesses in the attempt. Use existing, reviewed local configuration/auth, not the isolated dummy context used by the provider-free host smoke. Fabric checks auth-file metadata, not successful login or entitlement.
- An explicit choice of model in each selected harness's native configuration. Record the actual provider/model/account choice privately as `OMP_MODEL_CHOICE` / `CODEX_MODEL_CHOICE`; these variables are operator evidence, **not** Fabric configuration. Both real adapters reject Fabric `model` overrides and exact-model profile requests. If the native default is not known/reviewed, stop. No guessed model IDs, fallback, or automatic reconfiguration.
- Explicit user authorization covering the particular harnesses, selected models, controller turns if any, task count, account limits and potential charges. Only after that authorization, set `FABRIC_LIVE_APPROVED=yes` in the operator shell. This guard is an operator interlock, not proof that approval was obtained.

Native-context consent and provider-spending authorization are **separate**. Native configuration, skills, extensions, plugins, MCP and commands are trusted local code and can access local credentials/services; this is not a sandbox. Do not place secrets in shell examples, profiles, prompts, recipes or published reports. Keep evidence private; logs may contain sensitive native output.

## Scripted local model API (no external provider)

Raw `runs logs --follow` may start during workspace preparation: a pending fork
reports empty, non-EOF output until its tree and ownership marker are ready.
It does not open pending workspace paths; ordinary ownership checks still apply
once the fork completes.

Run the actual Fabric OMP adapter and native OMP against the test-only
`scripts/scripted-provider.mjs` HTTP server, not a fake harness:

```sh
bun integrations/omp/scripted-provider-smoke.mjs /absolute/path/to/fabric /absolute/path/to/omp /absolute/path/to/reference-plugin
```

Requires macOS, Bun, Git, `/usr/sbin/lsof`, Fabric, its reference adapter executable
and native `omp/19.1.1`. For a release bundle, pass `plugins/omp/plugin` as the third
binary; for a source build, pass `target/release/fabric-reference-harnesses`.
The existing SDK installs and content-pins that adapter in a disposable registry.
Missing dependencies or another OMP version fail; the runner never installs or
updates a native harness, logs in or silently skips. CI uses its existing
checksum-verified native OMP download.

The runner creates a disposable Git repository, private HOME/XDG/native profiles
and empty `agent.db`; child environments are literal allowlists. It never copies
user credentials, native configuration, history, bridge, proxy or dotenv values.
The sole custom model uses `openai-completions` at literal
`http://127.0.0.1:<ephemeral-port>/v1` with a synthetic key. Every model request
must match the expected route, authorization, model and conversation.

Native acceptance requires split SSE text, a real `write` tool call and actual
tool result in the next model request, a recoverable checksummed file patch,
non-retryable HTTP failure, incomplete-stream failure, and cancellation after
observing a real native partial-text event. It checks terminal states, one native
attempt, process-group disappearance, socket closure, prepared-source provenance,
artifact checksums and reclaimed workspaces. Watchdogs are failures, not success
conditions; no elapsed-time worker restart is used.

Configuration/discovery controls are reviewed against
[OMP v19.1.1 source](https://github.com/can1357/oh-my-pi/tree/31ffee513f27a01bba7d4dfbe12bae239cdc646a):
custom `models.yml`, default model role, all built-in/local provider IDs disabled,
no retry/fallback/auxiliary model, update, marketplace, telemetry or LSP activity.
`models.offline` alone does **not** stop registry discovery. Review the finite
disabled-provider list and settings before accepting another native version.
This test pin does not restrict Fabric's normal native-version discovery.

Stdout and retained `proof.json` report the observed version, scenario outcomes,
request counts, run IDs, artifact hashes and cleanup. A nonzero exit is failure;
private diagnostics remain in the reported `/private/tmp/fabric-scripted-omp-*`
directory. Share concise proof, not native transcripts, auth DBs or config files.
The server is loopback-only with no forwarding and bounded requests/responses.
Run its provider-free regression with `node scripts/test-scripted-provider.mjs`.

### Codex Responses and Claude Messages

Separate native runners exercise the same five scenarios; an OMP result never
substitutes for another harness's wire, tool-loop or cancellation evidence:

```sh
bun integrations/codex/scripted-provider-smoke.mjs /absolute/path/to/fabric /absolute/path/to/codex /absolute/path/to/reference-codex-plugin
bun integrations/claude/scripted-provider-smoke.mjs /absolute/private-inputs.json
```

Codex requires `codex-cli 0.171.0`; release bundles supply `plugins/codex/plugin`.
It uses the native Responses provider and actual `apply_patch`, not an ACP shim.
`exec --json` exposes completed message items, not individual text deltas: held
stream checks observe an actual message item before disconnect/cancel.

Claude uses official `claude-agent-acp` 0.112.0 and its bundled Claude Code
2.1.331 through the **generic** ACP bridge. Its private, owned `0600` inputs JSON
contains `bun`, `fabric`, `node`, `shim`, `claude`, `localPlugin` and `bootstrap`,
each `{ "path": "/canonical/absolute/file", "sha256": "<file-sha256>" }`,
plus `"claudeVersion": "2.1.331 (Claude Code)"`. `bootstrap` selects the adjacent
`integrations/claude/fixture-launcher.mjs`; use Node >=22.18, the actual Bun
runner and `plugins/local/plugin` (or built `fabric-local-target`) for `localPlugin`.
The launcher replaces the native child's environment with a fixture-only
allowlist and dummy Anthropic Messages endpoint. Real native Read/Write results
must reach the next request; Fabric's ACP JSONL is not a native SDK transcript.

Claude's pinned SDK concurrently requests a session title. Its fixture matches
one optional title call per scenario by exact prompt and output schema, with a
private canary that must never enter public events. The large native Read also
requires one exact `/v1/messages/count_tokens?beta=true` JSON call before the
real Read/Write continuation. Every required call must occur once; duplicate,
unknown, ambiguous and malformed requests still fail. Only this fixture opts
into bounded unordered matching; other scripted providers remain ordered.

CI downloads official architecture-specific releases with checked archive hashes;
the runners never install/update host harnesses or import user authentication.
Each reports its own retained proof/failure directory and exits nonzero on failure.
These are **native harness + scripted API** checks, not real-provider
authentication/parity, generic ACP conformance or OS network/keychain isolation.

## 1. Prepare a disposable immutable source (no model invocation)

Use Bash from the Fabric checkout. Choose `SMOKE_CASE` explicitly as `omp`, `codex` or `mixed`; repeat this procedure in a new directory for each case. Supply the prerequisite variables above before running:

```sh
: "${FABRIC_BINARY:?set an absolute path to the built Fabric binary}"
: "${SMOKE_CASE:?choose omp, codex, or mixed}"
case "$SMOKE_CASE" in
  omp) HARNESSES=omp ;;
  codex) HARNESSES=codex ;;
  mixed) HARNESSES='omp codex' ;;
  *) printf 'Unsupported smoke case\n' >&2; exit 1 ;;
esac
export FABRIC_BINARY SMOKE_CASE
for harness in $HARNESSES; do
  if test "$harness" = omp; then
    : "${PI_CODING_AGENT_DIR:?select the actual native OMP directory}"
    : "${OMP_MODEL_CHOICE:?record the reviewed native OMP model}"
    export PI_CODING_AGENT_DIR OMP_MODEL_CHOICE
    selected_dir="$PI_CODING_AGENT_DIR"
  else
    : "${CODEX_HOME:?select the actual native Codex directory}"
    : "${CODEX_MODEL_CHOICE:?record the reviewed native Codex model}"
    export CODEX_HOME CODEX_MODEL_CHOICE
    selected_dir="$CODEX_HOME"
  fi
  case "$selected_dir" in /*) test -d "$selected_dir" || exit 1 ;; *) exit 1 ;; esac
done
umask 077
export SMOKE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fabric-live.XXXXXX")"
export FABRIC_ROOT="$SMOKE_DIR/storage"
export SOURCE="$SMOKE_DIR/source"
mkdir "$SOURCE"
git -C "$SOURCE" init --quiet
printf 'original\n' > "$SOURCE/tracked.txt"
printf '.deps/\n' > "$SOURCE/.gitignore"
git -C "$SOURCE" add tracked.txt .gitignore
git -C "$SOURCE" -c user.name='Fabric Smoke' -c user.email=smoke@example.invalid \
  commit --quiet -m 'Disposable live smoke source'
git -C "$SOURCE" rev-parse HEAD > "$SMOKE_DIR/source-sha.txt"
printf "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n" \
  > "$SMOKE_DIR/prepare.toml"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" project add --name live-smoke --path "$SOURCE" \
  > "$SMOKE_DIR/project.txt"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" project prepare live-smoke \
  --recipe "$SMOKE_DIR/prepare.toml" --json > "$SMOKE_DIR/prepared.json"
for harness in $HARNESSES; do
  "$FABRIC_BINARY" harnesses doctor "$harness" --json > "$SMOKE_DIR/$harness-version.json" || exit 1
  # Review the exact discovered paths/exposure, then explicitly approve interactively.
  "$FABRIC_BINARY" --root "$FABRIC_ROOT" harness connect "$harness" || exit 1
done
```

Before connecting, ensure each selected directory is the intended absolute path. `connect` must succeed with the intended context; a credential file's presence is not an authentication PASS. Connection creates an explicit resolved native profile/consent reference; no manually authored Fabric profile is required for this route. For advanced explicit contexts, use the actual `ExecutionContext::Explicit` contract in [README](../README.md), not a guessed profile schema.

Create the version-free request locally. The required model-choice variables record the operator's reviewed native choice; no `model` field is sent. The source selector uses the **actual** READY state returned above, never a fabricated ID or moving HEAD:

```sh
export SMOKE_CASE
python3 - <<'PY'
import json, os
from pathlib import Path
p = Path(os.environ['SMOKE_DIR'])
prepared = json.loads((p / 'prepared.json').read_text())
assert prepared['status'] == 'READY'
assert prepared['source_commit_sha'] == (p / 'source-sha.txt').read_text().strip()
harnesses = {'omp': ['omp'], 'codex': ['codex'], 'mixed': ['omp', 'codex']}[os.environ['SMOKE_CASE']]
choices = {}
tasks = []
for harness in harnesses:
    directory = os.environ['PI_CODING_AGENT_DIR' if harness == 'omp' else 'CODEX_HOME']
    assert Path(directory).is_absolute() and Path(directory).is_dir()
    choices[harness] = os.environ[harness.upper() + '_MODEL_CHOICE']
    assert choices[harness].strip()
    task_id = harness + '-smoke'
    tasks.append({
        'task_id': task_id, 'harness_id': harness,
        'execution_context': {'kind': 'native'},
        'source_ref': {'kind': 'prepared', 'state_id': prepared['state_id']},
        'prompt': ('Read .deps/ready and require its exact contents to be ready. '
                   f'Write exactly {task_id} followed by a newline to tracked.txt and {task_id}.txt. '
                   'Do not change other files or call network tools. Report the actual edits.'),
        'limits': {'timeout_seconds': 120, 'copy_limit_bytes': 10485760,
                   'max_log_bytes': 1048576, 'max_artifact_bytes': 10485760}})
(p / 'model-choices.json').write_text(json.dumps(choices, indent=2) + '\n')
(p / 'workers.json').write_text(json.dumps({'project': 'live-smoke', 'placement': 'local',
    'max_concurrency': len(tasks), 'tasks': tasks}, indent=2) + '\n')
PY
```

Review `workers.json`, native context/model choices and approval scope before continuing. These per-worker time/log/artifact bounds are **not a spending cap**. Generic async limits reject `max_cost_usd`; OMP's foreground budget is only best effort, and Codex has no supported cost cap. If the user requires an enforceable hard dollar ceiling, report BLOCKED instead of running.

## 2. Explicit opt-in OMP, Codex or mixed async execution

This is the potentially billed step. Run only the request matching the authorized `SMOKE_CASE`; a mixed attempt requires authorization for **both** harnesses. Retain the exact submitted request and command outputs:

```sh
test "${FABRIC_LIVE_APPROVED:-}" = yes && \
  "$FABRIC_BINARY" --root "$FABRIC_ROOT" workers spawn \
  --request "$SMOKE_DIR/workers.json" --json > "$SMOKE_DIR/accepted.json"
```

Require `accepted:true`; record real `batch_id`, `task_id`, `run_id`, accepted source provenance and return timing. Acceptance is not completion or PASS. If transport is uncertain, inspect retained IDs/state rather than blindly resubmitting. Close/reopen the submitting client if desired; admitted workers are manager-owned. In a fresh client restore the recorded `FABRIC_BINARY`, `FABRIC_ROOT`, `SMOKE_DIR` and actual batch ID:

```sh
export BATCH_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["batch_id"])' "$SMOKE_DIR/accepted.json")"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" status "$BATCH_ID" --json > "$SMOKE_DIR/status.json"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" wait "$BATCH_ID" --timeout-seconds 10 --json > "$SMOKE_DIR/wait.json"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" results "$BATCH_ID" --json > "$SMOKE_DIR/results.json"
```

A bounded wait may return nonterminal state. Continue independent work and repeat a bounded wait only as needed; do not hot-poll or treat timeout as completion. `status --after CURSOR` resumes from a returned `next_cursor`. `workers show/results/wait/cancel` are corresponding aliases. Successful results retrieval is not worker success. If explicitly cancelling, use `cancel "$BATCH_ID" --json` (or a returned run ID); inspect the resulting actual outcome rather than claiming smoke success.

## 3. Evidence required before a live PASS

For each actual run ID, save these provider-free inspections (set `RUN_ID` from accepted/results evidence):

```sh
: "${RUN_ID:?select an actual returned run ID}"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" runs show "$RUN_ID" --json > "$SMOKE_DIR/$RUN_ID-show.json"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" runs artifacts "$RUN_ID" --json > "$SMOKE_DIR/$RUN_ID-artifacts.json"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" storage status --json > "$SMOKE_DIR/storage.json"
"$FABRIC_BINARY" --root "$FABRIC_ROOT" storage gc --dry-run --json > "$SMOKE_DIR/gc.json"
```

Require all of the following; record FAIL/ISSUES or NOT_VERIFIED/BLOCKED for missing evidence, never a synthetic PASS:

1. Batch `terminal:true`, `succeeded:true`; every task has `state:SUCCEEDED`, a real `result`, no task error, actual pinned harness version, zero exit and no fatal native event/stop reason. Authentication and model invocation must actually have succeeded. Record observed native provider/model evidence separately; an operator choice, null model field, exit zero or worker self-report does not establish the model actually used.
2. Exact same accepted prepared-state/base SHA as `prepared.json`; distinct run/workspace IDs and private workspace paths for the mixed batch. Save worker attempts, consent/profile digest and capture metadata. No checkout edits or ignored dependency loss; source `tracked.txt` stays `original\n`, source has no model-created files, prepared `tracked.txt` is unchanged and prepared `.deps/ready` stays `ready`.
3. A registered complete manifest, prompt snapshot, bounded logs/events, nonempty `tracked.patch` and `untracked.tar`; verify **every** inventory entry's `size_bytes` and `sha256` against its root-relative `relative_path` under the managed root (e.g. `shasum -a 256 "$FABRIC_ROOT/$RELATIVE_PATH"`). Validate manifest run/source/profile provenance too. Inspect actual native/normalized output and requested file bytes, not just a model's assertion. Empty/truncated/unparseable needed output is insufficient evidence.
4. Restore each result independently into a **new separate** trusted checkout of the recorded source SHA: `git clone --no-local --no-hardlinks "$SOURCE" "$RECOVERY"`, `git -C "$RECOVERY" checkout --detach "$SOURCE_SHA"`, then `git -C "$RECOVERY" apply --binary "$TRACKED_PATCH"` and `tar -xpf "$UNTRACKED_TAR" -C "$RECOVERY"`. Supply a fresh absolute `RECOVERY` and the actual inventory paths (`tracked_patch`, `untracked_archive`); never restore into the source or READY base. Require `tracked.txt` and the task's named note to contain exactly its task ID plus newline. Ignored dependencies are not archived; check them in the immutable base, not the restored archive.
5. Workspace `RECLAIMED`, old workspace absent, artifacts still readable/checksummed afterward. Record actual `cleanup` evidence and storage report; a cleanup refusal is not reclaimed space. Preserve evidence outside the managed root, including request, versions, invocation, approval scope (no secrets), timing, IDs, SHAs, digests, actual model/output and cleanup result. `SMOKE_DIR` contains sibling source/evidence/storage directories; keep it private rather than deleting it automatically.

Recovery on an interrupted/failed attempt is separate from successful live acceptance:

```sh
"$FABRIC_BINARY" --root "$FABRIC_ROOT" recover "$RUN_ID" --json
# Only after reviewing the inspection and explicitly approving mutation:
"$FABRIC_BINARY" --root "$FABRIC_ROOT" recover "$RUN_ID" --apply --json
```

Inspection is the default. Apply needs **positive inactive authority**, exact managed identity and complete durable capture. Schema v12 records immutable profile/artifact-limit context before execution. Complete interrupted capture may be sealed/registered before safe reclaim; recovery never reruns the worker or invents success. Partial, unknown, corrupt or legacy-insufficient evidence remains retained with an explanation. `storage status` reports run IDs, actions and logical bytes (unknown is not zero), not APFS-exclusive storage or a hard quota. There is no recovery-data TTL deletion or artifact-history purge. Never kill a stored PID or delete a guessed path to make a smoke pass.

## Existing Codex-only foreground smoke

The existing helper remains a separate, smaller output-only proof, not the mixed/async or edited-source test above:

```sh
# Safe default: prints NOT_VERIFIED; no provider invocation.
sh scripts/smoke-codex.sh
```

For an authorized live attempt, supply **your real absolute nonsecret `CODEX_SMOKE_PROFILE` path** binding `harness_id:codex`, `harness_version:codex-cli 0.171.0`, and reviewed native-config/connection references (or an explicitly authorized foreground env connection). See [explicit profiles](../README.md#explicit-profiles). Its selected native/default model must be explicitly reviewed; env-only mode has no Fabric-enforced model selection. Do not invent a sample credential or assume connect generates this file. This profile is an independent authorization path, not implicitly granted by connect:

```sh
: "${CODEX_SMOKE_PROFILE:?supply the actual absolute profile JSON path}"
test "${FABRIC_LIVE_APPROVED:-}" = yes && \
  FABRIC_BINARY="$FABRIC_BINARY" sh scripts/smoke-codex.sh --run --profile "$CODEX_SMOKE_PROFILE"
```

The helper requires the pinned version, runs a 60-second foreground task, verifies `FABRIC_SMOKE_OK`, checksums and reclaim, and reports `evidence_root` on PASS or `recovery_root` on failure. It retains its source/evidence. A zero helper exit with `NOT_VERIFIED` is **not PASS**. It does not establish explicit model identity, mixed-harness independence, source-edit restoration or model-selected pstack routing.

## Explicit local pstack live smoke (separate controller authorization)

Require an operator-supplied absolute generated `PSTACK_PLUGIN_DIR`, the initialized `live-smoke` project, selected OMP context/model and authorization for a controller turn **plus four workers**. First verify actual skill discovery using the provider-free host procedure in [control surfaces](CONTROL_SURFACES.md); do not send an unknown slash command as a nonbilling probe. Then, only under the live authorization:

```sh
: "${PSTACK_PLUGIN_DIR:?supply the actual generated om-pstack omp directory}"
test "${FABRIC_LIVE_APPROVED:-}" = yes && \
  omp --plugin-dir "$PSTACK_PLUGIN_DIR" -e "$PWD/integrations/omp/fabric.ts"
```

Generate the literal skill command from the recorded state, then send its output in that approved OMP session:

```sh
python3 - <<'PY'
import json, os
from pathlib import Path
state = json.loads((Path(os.environ['SMOKE_DIR']) / 'prepared.json').read_text())['state_id']
print('/skill:swarm --fabric-local --fabric-project live-smoke --fabric-harness omp '
      'Run four independent bounded smoke workers named smoke-1 through smoke-4. '
      f'Each must use source_ref={{"kind":"prepared","state_id":"{state}"}}. '
      'Each must read .deps/ready as ready, then write its own task ID plus newline '
      'to tracked.txt and a distinct task-ID.txt note; change nothing else. '
      'Use per-worker 120-second, 1048576-byte log and 10485760-byte copy/artifact limits. '
      'Return durable handles promptly, continue independent work, then use bounded '
      'fabric_wait and fabric_results. Verify actual outputs, artifacts, source and reclaim. '
      'Report gaps as BLOCKED or ISSUES; no remote placement or native-task fallback.')
PY
```

Provide reviewed model/authorization details in the controller context before sending; `prepared.json` is operator evidence, not assumed accessible in a worker. Save the controller's actual selected tool calls, early handles and ordinary-context progress/results; reconnect to saved IDs rather than resubmit. Apply the per-run evidence checks above, including independent restore of edited files. This route is interpreted by the skill/controller, not a new CLI parser; confirm its actual submitted tasks/sources/limits before accepting the proof. No command can guarantee a model follows instructions.

Only an observed authorized model-selected typed route plus verified live workers can establish that live routing result. Provider-free discovery cannot. Missing native tools/consent/READY state are blockers, not permission for implicit setup or fallback. Ordinary `/skill:swarm` keeps native OMP `task`; Fabric's explicit bridge is **local only**, never cloud/BYOC/security-isolation parity. Keep `LOCAL_WORKER_API_READY`, `PSTACK_LOCAL_SWARM_READY` and `LIVE_HARNESS_VERIFIED` evidence separate in the release record.
