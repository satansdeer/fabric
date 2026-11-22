# Issue-controlled development: local first

[Roadmap #1](https://github.com/satansdeer/fabric/issues/1) tracks the imported plugin/Connect/cloud plan. [Bootstrap #16](https://github.com/satansdeer/fabric/issues/16) starts the local issue loop on the existing macOS runtime; [dogfood #15](https://github.com/satansdeer/fabric/issues/15) retains the full real-agent → reviewed-PR acceptance. Hosted Connect, a plugin rewrite, Linux and release packaging do not gate this first loop.

## Execution order

1. Prove the small local issue controller with the existing manager and provider-free workers.
2. Explicitly authorize one native-context/model-backed worker for one approved issue; inspect its evidence and open a PR for human review. Until observed, real agent dogfooding is **NOT_VERIFIED**.
3. Continue one approved issue/PR at a time: protocol/SDK (#2–#3), reference plugins (#4–#6), easier installation/local setup (#7–#8), Connect/GitHub (#9–#11), then remote/cloud/BYOC (#12–#14).

The local controller is `scripts/fabric-issue.py`, using Python 3, `gh` and the existing Fabric CLI. GitHub-specific code stays outside Rust Core. It is a manually triggered bridge, not a webhook service, continuous issue watcher or new execution engine.

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
- A human reviews before merge. Only then select the next ready issue. No unbounded retry/Ralph loop.

The first model-backed issue/PR remains tracked separately from provider-free controller verification. Do not close #15 or mark its live acceptance passed based on a fake worker.

## Provider-free integration gate

```sh
python3 scripts/test-fabric-issue.py --fabric "$PWD/target/debug/fabric"
```

This check uses a disposable Git source, fake GitHub transport, a real Fabric manager and separate fake harness subprocesses. It requires neither GitHub authentication/network nor native harness consent/provider spending. It exercises approved issue admission, rejected input, durable idempotency, actual captured mutations/evidence and safe cleanup. It is not a real-agent or hosted-trigger proof.

Observed bootstrap proof: [provider-free acceptance evidence](https://github.com/satansdeer/fabric/issues/16#issuecomment-7080579649). The gate passed with real manager admission, stable retry handles, 14 verified artifacts, immutable source trees, reclaimed workspace and stopped owned manager. Live GitHub issue preview also passed. This does not verify a native/model-backed worker, automatic issue delivery or a hosted callback URL.
