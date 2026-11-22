# Supervised GitHub Connect bootstrap

Tracked in [#18](https://github.com/satansdeer/fabric/issues/18), stacked on the [local issue bridge](ISSUE_WORKFLOW.md). Implementation passes provider-free checks; **the hosted service is not deployed**. Netlify rejects deployment because account credits are exhausted. The reserved fallback is `https://fabric-connect-polyglo-satansdeer.netlify.app`; the desired custom domain is `fabric-connect.polyglo.tv`, but public DNS returns NXDOMAIN and registry WHOIS returns “No Data Found”. Restore deployment allowance and domain registration/delegation before launch. This is a single owner-provisioned route, not self-service authentication or a multi-tenant service. Full identity, transport and GitHub connector issues remain open.

## Trust and execution

GitHub signs `issues` webhooks with a separate webhook secret. Only open, non-PR issues in `satansdeer/fabric`, authored and changed by `satansdeer`, with `ready`, can enter the inbox. Supported actions are opened, edited, reopened and ready-label addition. Signature validation covers the raw body; the event identity is its SHA-256. An event contains safe identifiers, not issue title/body or credentials.

A label only queues a notification. The local owner selects one issue, reviews its current snapshot and supplies that exact approval SHA. The consumer refetches GitHub and calls the existing local bridge; Fabric owns admission, execution, artifacts and reclaim. No inbound local port, automatic model spending, push or merge. Native harnesses require both existing local consent and `--allow-model-spend`.

The hosted route binds one provisioned worker identity and bearer-token hash. Rotating a token requires coordinated local/server configuration. Switching machines is not an automatic reassignment path. Keep the private connection file outside the checkout; never paste it into an issue or commit it.

## Local use

The connection file must be an owner-owned, nonsymlink, mode-0600 JSON file:

```json
{"schema_version":1,"endpoint":"https://fabric-connect-polyglo-satansdeer.netlify.app","worker_id":"OWNER_PROVISIONED_ID","token":"OWNER_PROVISIONED_TOKEN"}
```

The initial owner connection is provisioned at `~/.config/fabric/connect/polyglo.json`, outside the checkout. It is not usable until deployment succeeds. After launch, status does not admit work:

```sh
python3 scripts/fabric-connect.py status \
  --connection "$HOME/.config/fabric/connect/polyglo.json"
```

Prepare/register the trusted source using the [local workflow](ISSUE_WORKFLOW.md), then preview the live issue to obtain its approval SHA. Select the same source/project/prepared state when consuming:

```sh
python3 scripts/fabric-issue.py preview \
  --repo satansdeer/fabric --issue 18 \
  --allow-author satansdeer --allow-label ready

python3 scripts/fabric-connect.py once \
  --connection "$HOME/.config/fabric/connect/polyglo.json" \
  --issue 18 --approve "$REVIEWED_SHA" \
  --fabric "$PWD/target/debug/fabric" --root "$FABRIC_ROOT" \
  --project fabric --prepared-state "$READY_ID" \
  --harness fake --fake-binary "$TRUSTED_FAKE_BINARY" \
  --allow-author satansdeer --allow-label ready
```

The fake harness is for deterministic proof, not code generation. For an explicitly authorized real OMP run, select `--harness omp --allow-model-spend` and omit `--fake-binary`; native OMP must already be connected. `--post` explicitly opts into sanitized GitHub status comments.

`once` polls only the selected issue, journals the immutable event-to-local-request mapping before admission, acknowledges accepted Core handles, then observes for at most 30 seconds. Repeat the identical command to reconcile unfinished work. Accepted work is observed without requiring the issue still to be eligible. Uncertain admission is retained for reconciliation, never silently declared complete or blindly replaced. Replays use the existing bridge/Core identity instead of creating a second run. A terminal hosted status requires an observed local terminal outcome.

## Deployment

The isolated Netlify site is `fabric-connect-polyglo-satansdeer` (ID `6400a187-1d3a-49a4-aef1-0e4510571f1c`), reserved but not deployed. Its `netlify.app` hostname is the fallback public address after deployment allowance is restored. The unavailable custom domain is detached; no new DNS record remains. The two original `polyglo.tv` DNS records and unrelated sites/DNS, especially `usemont.com`, remain unchanged. Deployment is manual; no repository build hook or untrusted preview receives credentials.

After the owner restores deployment allowance, run `npm ci` then `npx netlify-cli deploy --no-build --prod --site 6400a187-1d3a-49a4-aef1-0e4510571f1c` from `integrations/connect` with an authenticated Netlify CLI. Do not put credentials in command arguments or source. The modern function uses site-wide Netlify Blobs, strong reads and conditional writes to preserve data across redeploys; this still needs production verification. Production environment variables:

- `FABRIC_CONNECT_WEBHOOK_SECRET`: random shared GitHub HMAC secret.
- `FABRIC_CONNECT_WORKER_TOKEN_SHA256`: SHA-256 of the private local bearer token, never the raw token.
- `FABRIC_CONNECT_WORKER_ID`: the explicitly provisioned worker identity.

Variables are site-specific and production-context only. The current plan does not support function-only scopes or Netlify secret-value flags; account administrators can read these values. No paid-plan upgrade, credit purchase or domain registration was performed. No GitHub token is stored on Netlify.

After health succeeds, create the GitHub repository webhook at `https://fabric-connect-polyglo-satansdeer.netlify.app/v1/projects/satansdeer/fabric/automations/issues/github`, content type JSON, SSL verification enabled, only `issues` events. Configure the shared secret privately. **No webhook has been created against the undeployed endpoint.** Public `GET /health` checks the service; authenticated `/status`, `/poll` and `/events/<id>/ack` are scoped to that fixed route. No status payload includes raw issue content or tokens.

## Bounds and recovery

The single route blob is intentionally bounded: 64 active events, 256 retained records, 60 new events/minute, 24-hour unstarted expiry, seven-day replay retention, 120-second delivery leases and at most five delivery attempts. Capacity returns an explicit rejection rather than evicting required history. Running work is not reassigned merely because the worker disconnects. Repeated compare-and-swap conflicts return a retryable service error rather than acknowledging an unpersisted update.

Offline workers leave events queued. Run `status` after reconnect, then explicitly approve and consume the chosen issue. Do not delete local journals to force a retry: retained Core handles and evidence are the recovery authority. The hosted inbox is not an artifact store; inspect and verify actual artifacts through Fabric locally.

## Provider-free verification

```sh
node --test integrations/connect/service.test.mjs
python3 scripts/test-fabric-connect.py --fabric "$PWD/target/debug/fabric"
```

These checks use signed fixture events, a durable local test-server store and real Fabric/fake-harness processes. All 19 ingress tests and the outbound real-Core/fake-harness check pass, including 14 verified artifacts, restart recovery and duplicate-admission prevention. `netlify functions:build --src netlify/functions --functions .netlify/functions` also successfully bundles the production adapter. They do not prove deployed runtime behavior, actual GitHub delivery, production redeploy durability or model-backed issue-to-PR dogfooding. Launch blockers and remaining live acceptance gates are recorded in #18; #15 remains the real model-backed acceptance gate.
