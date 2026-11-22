# Supervised GitHub Connect bootstrap

Tracked in [#18](https://github.com/satansdeer/fabric/issues/18), stacked on the [local issue bridge](ISSUE_WORKFLOW.md). **The permanent owner-provisioned Cloudflare service is live at `https://fabric-connect.satansdeer.workers.dev`**, backed by a SQLite Durable Object. The owner connection is `~/.config/fabric/connect/cloudflare.json`, outside the checkout. No paid upgrade or DNS changes were needed. Netlify and the custom domain are historical fallbacks, not active prerequisites. This is a single owner-provisioned route, not self-service authentication or a multi-tenant service. Full identity, transport and GitHub connector issues remain open.

## Trust and execution

GitHub signs `issues` webhooks with a separate webhook secret. Only open, non-PR issues in `satansdeer/fabric`, authored and changed by `satansdeer`, with `ready`, can enter the inbox. Supported actions are opened, edited, reopened and ready-label addition. Signature validation covers the raw body; the event identity is its SHA-256. An event contains safe identifiers, not issue title/body or credentials.

A label only queues a notification. The local owner selects one issue, reviews its current snapshot and supplies that exact approval SHA. The consumer refetches GitHub and calls the existing local bridge; Fabric owns admission, execution, artifacts and reclaim. No inbound local port, automatic model spending, push or merge. Native harnesses require both existing local consent and `--allow-model-spend`.

The hosted route binds one provisioned worker identity and bearer-token hash. Rotating a token requires coordinated local/server configuration. Switching machines is not an automatic reassignment path. Keep the private connection file outside the checkout; never paste it into an issue or commit it.

## Local use

The connection file must be an owner-owned, nonsymlink, mode-0600 JSON file:

```json
{"schema_version":1,"endpoint":"https://fabric-connect.satansdeer.workers.dev","worker_id":"OWNER_PROVISIONED_ID","token":"OWNER_PROVISIONED_TOKEN"}
```

Use the existing owner-provisioned connection at `~/.config/fabric/connect/cloudflare.json`; do not copy credentials into the checkout. Status does not admit work:

```sh
python3 scripts/fabric-connect.py status \
  --connection "$HOME/.config/fabric/connect/cloudflare.json"
```

Prepare/register the trusted source using the [local workflow](ISSUE_WORKFLOW.md), then preview the live issue to obtain its approval SHA. Select the same source/project/prepared state when consuming:

```sh
python3 scripts/fabric-issue.py preview \
  --repo satansdeer/fabric --issue 18 \
  --allow-author satansdeer --allow-label ready

python3 scripts/fabric-connect.py once \
  --connection "$HOME/.config/fabric/connect/cloudflare.json" \
  --issue 18 --approve "$REVIEWED_SHA" \
  --fabric "$PWD/target/debug/fabric" --root "$FABRIC_ROOT" \
  --project fabric --prepared-state "$READY_ID" \
  --harness fake --fake-binary "$TRUSTED_FAKE_BINARY" \
  --allow-author satansdeer --allow-label ready
```

The fake harness is for deterministic proof, not code generation. For an explicitly authorized real OMP run, select `--harness omp --allow-model-spend` and omit `--fake-binary`; native OMP must already be connected. `--post` explicitly opts into sanitized GitHub status comments.

`once` polls only the selected issue, journals the immutable event-to-local-request mapping before admission, acknowledges accepted Core handles, then observes for at most 30 seconds. Repeat the identical command to reconcile unfinished work. Accepted work is observed without requiring the issue still to be eligible. Uncertain admission is retained for reconciliation, never silently declared complete or blindly replaced. Replays use the existing bridge/Core identity instead of creating a second run. A terminal hosted status requires an observed local terminal outcome.

## Provider-independent development

The [local issue bridge](ISSUE_WORKFLOW.md) reads GitHub directly and needs no public URL. It remains a fallback for explicitly approved issue-driven work independent of hosted ingress.

For signed-webhook development, a [Cloudflare Quick Tunnel](https://developers.cloudflare.com/tunnel/get-started/quick-tunnels/) forwards a temporary HTTPS address to a loopback receiver without an account or domain. Keep Core private; expose only the bounded, signature-checked Connect handler, with separate disposable webhook and worker credentials. The local file-backed `test-server.mjs` is a single-process integration fixture, not a production service. The address changes on restart and delivery is unavailable when the Mac or tunnel is offline; the local file store does not make ingress always-on.

In [#20](https://github.com/satansdeer/fabric/issues/20), real GitHub opened/ready-label events survived receiver restart, reused one actual completed Core admission, and produced 14 artifacts whose bytes, sizes and SHA-256 digests were verified. GitHub's exact signed redelivery returned `completed` with `duplicate: true`; a subsequent consumer invocation was idle. Unsigned ingress and unauthenticated worker requests returned 401. The temporary hook, tunnel and receiver were removed/stopped after verification. No model calls were made.

The permanent Cloudflare adapter is in [`integrations/connect/cloudflare/`](../integrations/connect/cloudflare/): [Workers on `workers.dev`](https://developers.cloudflare.com/workers/configuration/routing/workers-dev/) forwards the single configured route to one [SQLite-backed Durable Object](https://developers.cloudflare.com/durable-objects/platform/pricing/). Conditional writes use synchronous SQLite transactions; the route's bounded 512 KiB JSON is SQL TEXT, not a single 128 KiB Durable Object KV value. Authentication, leases/replay and explicit local approval/spend gates reuse the existing service. No custom domain, Netlify account or paid-plan upgrade is required.

## Live deployment evidence

Controller-supplied production observations for [#21](https://github.com/satansdeer/fabric/issues/21):

- Workers and SQLite Durable Object deployment succeeded without a paid upgrade or DNS changes.
- Public `GET /health` returned 200; unsigned ingress and unauthenticated worker access each returned 401.
- Repository webhook `715419509` is active, issues-only, with SSL verification enabled, at `https://fabric-connect.satansdeer.workers.dev/v1/projects/satansdeer/fabric/automations/issues/github`.
- A real signed ready-label delivery for #21 queued while the local consumer was offline, survived production redeploy, and was subsequently consumed through real Core/fake-harness execution.

This proves permanent ingress/redeploy durability and provider-free consumption, not model-backed implementation. [#23](https://github.com/satansdeer/fabric/issues/23) and [#25](https://github.com/satansdeer/fabric/issues/25) separately supply native-worker evidence. Ingress queues unattended; execution remains supervised and outbound-only through the existing `status`/`once` consumer, not a new daemon.

The deployed route uses the privately provisioned `FABRIC_CONNECT_WEBHOOK_SECRET`, `FABRIC_CONNECT_WORKER_TOKEN_SHA256` and `FABRIC_CONNECT_WORKER_ID` bindings. Only the bearer token's SHA-256 digest belongs on the server. Keep the raw token and owner connection outside the checkout. Public health and authenticated `/status`, `/poll` and `/events/<id>/ack` remain scoped to the fixed route; status payloads contain neither raw issue content nor tokens.

## Historical Netlify/domain fallback

The reserved Netlify site `fabric-connect-polyglo-satansdeer` (ID `6400a187-1d3a-49a4-aef1-0e4510571f1c`) was blocked by exhausted account credits and is not deployed. Its fallback address is `https://fabric-connect-polyglo-satansdeer.netlify.app`; no webhook was created against that undeployed endpoint. The proposed `fabric-connect.polyglo.tv` domain returned NXDOMAIN and registry WHOIS “No Data Found” and was detached. Existing DNS and unrelated sites were left unchanged.

These limitations do not gate the live Cloudflare path. The Netlify adapter uses site-wide Blobs, strong reads and conditional writes, but still lacks production/redeploy verification. Revisit only if the owner explicitly selects that fallback; no credit purchase, domain registration or authorization renewal is required to use the current Cloudflare connection.

## Bounds and recovery

The single route blob is intentionally bounded: 64 active events, 256 retained records, 60 new events/minute, 24-hour unstarted expiry, seven-day replay retention, 120-second delivery leases and at most five delivery attempts. Capacity returns an explicit rejection rather than evicting required history. Running work is not reassigned merely because the worker disconnects. Repeated compare-and-swap conflicts return a retryable service error rather than acknowledging an unpersisted update.

Offline workers leave events queued. Run `status` after reconnect, then explicitly approve and consume the chosen issue. Do not delete local journals to force a retry: retained Core handles and evidence are the recovery authority. The hosted inbox is not an artifact store; inspect and verify actual artifacts through Fabric locally.

## Provider-free verification

The consumer sends `User-Agent: Fabric-Connect (+https://github.com/satansdeer/fabric)`. Check actual GET/POST headers, Bearer authorization and JSON content type with disposable loopback HTTP and fake credentials only:

```sh
python3 scripts/test-fabric-connect-request.py -v
```

```sh
node --test integrations/connect/service.test.mjs
python3 scripts/test-fabric-connect.py --fabric "$PWD/target/debug/fabric"
```

These checks use signed fixture events, a durable local test-server store and real Fabric/fake-harness processes. All 19 ingress tests and the outbound real-Core/fake-harness check passed locally, including 14 verified artifacts, restart recovery and duplicate-admission prevention. `netlify functions:build --src netlify/functions --functions .netlify/functions` also successfully bundled the Netlify adapter. Those fixture checks and the temporary tunnel proof in #20 do not themselves establish permanent hosting or model-backed execution; the separate production and native-worker evidence is listed above.

The observed [SIGTERM CI failure](https://github.com/satansdeer/fabric/actions/runs/45035376872) was a partial heartbeat PID record (#22); shared tree readiness now requires complete parent/child records. A later [typed-conformance CI failure](https://github.com/satansdeer/fabric/actions/runs/45036747038) exposed a manager startup SQLite handle retained across shutdown (#24); that startup-only handle now closes before readiness. The deterministic readiness regression, actual SIGTERM lifecycle test, binary build and actual typed conformance passed locally after these fixes. The conformance fixture owns/reaps the foreground manager and prints PASS only after successful resource cleanup. Controller-supplied GitHub macOS CI runs [45039604770](https://github.com/satansdeer/fabric/actions/runs/45039604770) and [45039529438](https://github.com/satansdeer/fabric/actions/runs/45039529438) at `c6583cb` completed successfully. These are recorded observations, not checks rerun here; #15 retains the real model-backed issue-to-reviewed-PR acceptance gate.
