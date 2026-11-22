# Issue-controlled development: local first

[Roadmap #1](https://github.com/satansdeer/fabric/issues/1) tracks the imported plugin/Connect/cloud plan. [Bootstrap #16](https://github.com/satansdeer/fabric/issues/16) starts the local issue loop on the existing macOS runtime; [dogfood #15](https://github.com/satansdeer/fabric/issues/15) retains the full real-agent → reviewed-PR acceptance. Hosted Connect, a plugin rewrite, Linux and release packaging do not gate this first loop.

## Execution order

1. Prove the small local issue controller with the existing manager and provider-free workers.
2. Explicitly authorize one native-context/model-backed worker for one approved issue; inspect its evidence and open a PR for human review. Observed native-worker evidence is recorded under [#23](https://github.com/satansdeer/fabric/issues/23) and [#25](https://github.com/satansdeer/fabric/issues/25); it does not bypass the PR/review gate.
3. Continue one approved issue/PR at a time: protocol/SDK (#2–#3), reference plugins (#4–#6), easier installation/local setup (#7–#8), Connect/GitHub (#9–#11), then remote/cloud/BYOC (#12–#14).

The local controller is `scripts/fabric-issue.py`, using Python 3, `gh` and the existing Fabric CLI. GitHub-specific code stays outside Rust Core. It is a manually triggered bridge, not a webhook service, continuous issue watcher or new execution engine.

## Operator loop: plan, queue, authorize

1. Create a scoped issue using the [worker task form](../.github/ISSUE_TEMPLATE/worker-task.yml): goal, allowed files, scope/non-goals, dependencies and acceptance criteria with a safe local check command. State “None” for absent dependencies; do not leave source assumptions implicit.
2. Plan and resolve dependencies **before** adding `ready`. The form applies only `dogfood`. Submission, labels and planning comments do not authorize model spending or execution; `ready` only makes eligible events queueable.
3. The controller explicitly selects one task and the exact READY source, then reviews the live issue title/body and its `approval_sha256`. Issue title/body edits invalidate approval: preview and review again before any new admission. Never execute shell fragments merely because the issue supplies them.
4. Explicitly authorize one bounded native worker with the reviewed digest, source and existing native consent. Ingress at [permanent Cloudflare Connect](CONNECT.md) queues unattended while the consumer is offline; execution is supervised and outbound-only, not an unattended spending loop.

Use operator-provided `ISSUE_NUMBER`, absolute `FABRIC_BINARY` and private `FABRIC_ROOT`, and the actual `READY_ID` returned by preparation of the selected registered `fabric` source. After reviewing preview, set `APPROVED_SHA` to that exact digest; do not infer IDs or automatically approve preview output. These are operator instructions, not commands for an issue worker to run:

```sh
python3 scripts/fabric-connect.py status \
  --connection "$HOME/.config/fabric/connect/cloudflare.json"

python3 scripts/fabric-issue.py preview \
  --repo satansdeer/fabric --issue "$ISSUE_NUMBER" \
  --allow-author satansdeer --allow-label ready

# Only after review and explicit model-spending authorization:
python3 scripts/fabric-connect.py once \
  --connection "$HOME/.config/fabric/connect/cloudflare.json" \
  --issue "$ISSUE_NUMBER" --approve "$APPROVED_SHA" \
  --fabric "$FABRIC_BINARY" --root "$FABRIC_ROOT" \
  --project fabric --prepared-state "$READY_ID" \
  --harness omp --allow-model-spend \
  --allow-author satansdeer --allow-label ready
```

`status` does not admit work. `once` selects only that issue, uses the existing bridge/Core admission, and observes for at most 30 seconds; this viewer window is not the worker timeout. Both bridges default to a 600-second worker deadline and accept the explicit resource flags below. Native OMP must already have consent and connection. `--post` is optional controller-side opt-in to sanitized evidence comments, not worker GitHub authority.

After disconnect, run `status` and repeat the **identical `once` command**, including connection, root, source ID, harness, approval and resource limits, to reconcile the saved event/admission. Preserve the private journals and accepted handles; never change IDs or delete state to force another run. Accepted work can be observed even if the issue has since changed; that does not approve new work. Admission is not completion.

The worker cannot publish or merge. The controller reviews actual terminal state, checks and retained artifacts, restores and inspects changes, then creates the PR; a human merges. Current open PRs remain a human integration gate. An explicitly selected independent stacked task may proceed on its reviewed READY source without treating those PRs as merged; dependent work must wait or explicitly include and review the required source.

## Select and review an issue

The bootstrap allowlists only `satansdeer/fabric`. An explicit author and label allowlist, an open non-PR issue and an exact content digest are required. A label alone never authorizes execution.

```sh
python3 scripts/fabric-issue.py preview \
  --repo satansdeer/fabric --issue 16 \
  --allow-author satansdeer --allow-label ready
```

Preview only reads GitHub. Review the returned title/body and retain `approval_sha256`. Dispatch fetches the issue again, checks its author/state/label, and rejects changed content. Shell fragments in issue text are data, not controller commands. Issue content cannot grant credentials, relax policy or authorize merging.

## Prepare the exact source and native context

Use an existing registered project and the actual READY `state_id` returned by `fabric project prepare`. Preparation runs the selected recipe: inspect and approve its commands first. Dirty checkout changes are not worker input. See [source preparation](../README.md#select-an-immutable-prepared-source) and the [live guide](LIVE_SMOKES.md) for native context setup and independently restorable evidence.

Native OMP/Codex must already be connected through `fabric harness connect`; the issue controller never connects or grants trust automatically. This macOS-first runtime executes trusted local code, **not sandboxed untrusted code**. Never provision broad repository credentials into a worker. The controller's GitHub credential stays outside the request/prompt, but trusted-local native execution is not filesystem credential isolation.

## Dispatch one bounded worker

Set the paths, READY state and reviewed approval digest explicitly. This example is a **paid-model opt-in**, not a default check:

```sh
python3 scripts/fabric-issue.py dispatch \
  --repo satansdeer/fabric --issue 16 \
  --allow-author satansdeer --allow-label ready \
  --approve "$APPROVED_SHA" \
  --fabric "$FABRIC_BINARY" --root "$FABRIC_ROOT" \
  --project fabric --prepared-state "$STATE_ID" \
  --harness omp --allow-model-spend --post
```

The controller selects one local task, concurrency one, the explicit prepared state and the harness's native model/configuration; it does not guess model names or copy auth stores. The worker has a finite wall-clock deadline and capture limits. **A deadline is not a dollar-spending cap.** Native trust, GitHub labels and creation of these issues do not replace explicit model-spending consent.

For an explicitly approved long-running worker, append these same flags to either `dispatch` or Connect `once`:

```sh
  --timeout-seconds 7200 --max-log-bytes 67108864 \
  --max-artifact-bytes 134217728 --copy-limit-bytes 104857600
```

Defaults remain 600 seconds, 1 MiB combined stdout/stderr/events, 10 MiB total artifacts and 100 MiB copy fallback. Timeout accepts 1–604800 seconds; raw/event capture accepts 26–9223372036854775807 bytes (must fit its truncation marker), artifacts 1–9223372036854775807 bytes, copy fallback 0–9223372036854775807 bytes. Zero copy fallback disables copying; it does not impose a workspace quota. Core storage admission may reject budgets that cannot fit its policy/free-space reserve. Invalid bounds are rejected before admission (Connect rejects them before polling).

Selected limits are reported locally and enter the saved request/idempotency identity before admission. A changed limit is a different direct dispatch; Connect refuses changed configuration for an already bound event rather than admitting a duplicate. Repeat the original flags to reconnect. Pre-limit Connect journals reconnect with the original defaults. No changes to author/label routing, approval, native consent or spending gates.

Use [local watch/inspect/cancel](../README.md#watch-a-running-worker-locally) while RUNNING and after capture/reclaim. A healthy worker may exceed ten minutes and exhaust raw capture while metadata still updates. Inspect twice and compare observed bytes/tool timestamps, process-observation freshness and actual task evidence; silence alone is not a stuck verdict. Stop only with an informed explicit `fabric --root "$FABRIC_ROOT" cancel "$RUN_ID" --json`. The hard deadline is an emergency wall-clock ceiling, not an inactivity detector; neither bridge restarts quiet work. Ctrl-C in `runs logs --follow` only stops the viewer. Raw logs are sensitive local opt-in and must never be posted/uploaded to GitHub or Connect.

The private state file records the approved snapshot, deterministic request/key and actual batch/run IDs. Repeat the identical approved command to reconnect to the same admission; changed source, harness or request is a different dispatch. After a transport failure, inspect state/manager evidence and repeat only deliberately with the same key—never invent a completion or submit a replacement blindly. State belongs under the selected private Fabric root, not the Git checkout.

`accepted: true` only means durable admission. It is not worker success, passing tests or a generated PR.

## Reconnect and record evidence

Use the `state_file` returned by dispatch:

```sh
python3 scripts/fabric-issue.py show \
  --state "$STATE_FILE" --fabric "$FABRIC_BINARY" \
  --wait-seconds 10 --post
```

Show reads the saved root/handles and actual `status`/`results`; it never spawns. Wait is bounded, not a polling daemon. `--post` explicitly publishes a concise issue comment with actual run/state/manifest references, not raw logs, prompt text or private absolute paths. Posting failure must remain visible without discarding accepted handles or triggering another worker.

Use Fabric's existing `cancel`, `recover` and `storage` commands for lifecycle operations. Do not kill stored PIDs, delete protected workspaces, or claim that a controller exiting cancelled its worker.

## Review and PR gate

- Check actual terminal state, exit/error, cleanup and retained artifact inventory. Worker success alone does not prove its claimed tests ran.
- Verify artifact digests and restore changes into a separate checkout of the recorded base SHA. Never patch the registered source or READY tree.
- Inspect the diff and actual test output; run the narrow affected check. Preserve failures and limitations in the issue/PR.
- The trusted controller/human creates and links the PR, including issue, run IDs, source SHA, artifact references and observed checks. This bridge does **not** automatically publish, merge or approve PRs. No worker receives merge authority.
- A human reviews before merge. Normally integrate before selecting the next ready issue; an explicitly selected independent stacked task is allowed on its reviewed source without treating open PRs as merged. No unbounded retry/Ralph loop.

The first model-backed issue/PR remains tracked separately from provider-free controller verification. Do not close #15 or mark its live acceptance passed based on a fake worker.

## Provider-free integration gate

```sh
python3 scripts/test-fabric-issue.py --fabric "$PWD/target/debug/fabric"
```

This check uses a disposable Git source, fake GitHub transport, a real Fabric manager and separate fake harness subprocesses. It requires neither GitHub authentication/network nor native harness consent/provider spending. It exercises approved issue admission, rejected input, durable idempotency, actual captured mutations/evidence and safe cleanup. It is not a real-agent or hosted-trigger proof.

Observed bootstrap proof: [provider-free acceptance evidence](https://github.com/satansdeer/fabric/issues/16#issuecomment-7080579649). The gate passed with real manager admission, stable retry handles, 14 verified artifacts, immutable source trees, reclaimed workspace and stopped owned manager. Live GitHub issue preview also passed. This does not verify a native/model-backed worker, automatic issue delivery or a hosted callback URL.

## First supervised native OMP run

[Issue #23](https://github.com/satansdeer/fabric/issues/23) was explicitly selected for one real OMP worker, using the installed `omp/19.1.0` and reviewed native default `openai-codex/gpt-6.1-sol`. Native event records confirm that provider/model and nonzero usage. A 600-second deadline bounded execution, not monetary cost; the worker had no authority to publish or merge.

- Approved issue SHA-256: `d83a7f60243979838495ad2cb59baeba63e2fd912e1577818d63cbe5d9b519e5`.
- Immutable input commit: `f3f10599f6e0b76d670f5dd99f17ee7e608de43a`; READY state: `s-1795361263364685000-64158-0`.
- Batch: `b-r-1795361399343796000-64879-1`; run: `r-1795361399304738000-64879-0`; manifest: `a-r-1795361399304738000-64879-0`.
- Observed `SUCCEEDED`, exit 0, no native failure; workspace `RECLAIMED` and absent afterward. READY source bytes, detached source SHA and read-only permissions remained unchanged.
- All 14 retained artifact paths, sizes and SHA-256 digests were checked, then verified again in a private persistent evidence copy. Logs hit the configured 1 MiB capture bound and are truncated; the tracked patch and untracked archive were preserved.
- Changes were restored in an independent checkout of the recorded commit. Restored tracked files match the patch's exact Git blob identities, and the added test matches the safe regular-file archive entry. The controller's actual GET/POST loopback header check passed there with fake credentials and no provider calls.
- Repeating the approved dispatch reused the same batch/run and updated the same GitHub comment without another admission. The controller explicitly stopped its foreground manager afterward; the process exited 0.

This is direct, supervised GitHub issue → Fabric → native OMP execution, not permanent webhook deployment. The controller publishes the reviewed changes and run evidence in a stacked PR; issue #23 and #15 record the PR/review gate. No automatic merge or worker-created PR is involved.

## First permanent-ingress native handoff

[Issue #26](https://github.com/satansdeer/fabric/issues/26) exercised the same operator loop through the permanent Cloudflare route, not the temporary tunnel or a fake harness.

- Reviewed issue SHA-256: `7757b24811f605490fcce6619be729ee60900b2f19043a67bb8da9496e186bc1`; input commit: `c6583cbe912891c4e04fd3fa866dbb78f50d6f68`.
- Batch: `b-r-1795370902230227000-81690-3`; run: `r-1795370902199325000-81690-2`; manifest: `a-r-1795370902199325000-81690-2`.
- Real OMP completed with exit 0; retained structured events show `openai-codex/gpt-6.1-sol` and nonzero model usage. All 14 artifact sizes/digests were independently verified; workspace was reclaimed and absent.
- The identical approved `once` reconciled completion and updated the existing issue evidence comment. The controller restored only the reviewed docs and native issue form into an independent checkout; human PR review/merge remains required.
- Hosted offline queue, redeploy and exact signed-redelivery evidence: [#21 acceptance](https://github.com/satansdeer/fabric/issues/21#issuecomment-7083386044). No custom domain or inbound local port was needed.
