# Fabric Connect: stable authenticated automation identities

Connect uses the existing owner-provisioned Cloudflare Worker at `https://fabric-connect.satansdeer.workers.dev` and its SQLite Durable Object. Issue [#9](https://github.com/satansdeer/fabric/issues/9) adds claimed account/project/automation identities to that service; it does not create another service or deploy this source. `polyglo.tv` is optional future custom-hostname work, not a DNS prerequisite. No `usemont.com` changes are involved. Broad outbound reconnect (#10) and external GitHub setup (#11) remain separate.

## Trust and execution

GitHub signs raw `issues` webhook bodies with a separate, randomly generated secret for each automation. Claimed policy fixes the repository, author, sender and readiness label. Only open, non-PR issues with that exact policy enter the inbox. Supported actions are opened, edited, reopened and addition of the readiness label. SHA-256 of the raw body is the event identity; reusing a GitHub delivery UUID for different bytes is rejected. New events must have a valid issue update timestamp within the preceding 24 hours (at most five minutes in the future). Authenticated exact redeliveries return the retained receipt even after that freshness window.

A label only queues a notification. The local owner selects one issue, reviews its current snapshot and supplies that exact approval SHA. The consumer refetches GitHub and calls the existing local bridge; Fabric owns admission, execution, artifacts and reclaim. No hosted plugin execution, inbound local port, automatic model spending, push or merge. Native harnesses require both existing local consent and `--allow-model-spend`. Existing connector source/tenant/scope/route checks and Core lifecycle invariants remain unchanged.

Account and project claims bind an immutable principal ID; an automation binds that principal, a stable identity, an explicit webhook policy and a worker ID. Authorization checks the principal's account, exact project/automation grant and operation, plus the stored owner. Parent claims do not grant access to descendant inboxes. Moving a private connection and its existing local recovery state to another authorized Mac does not change the public URL. A worker ID is an authenticated consumer binding, never a route key, tunnel ID or lease identity. Copying a token alone is not permission to resubmit a running event.

## Provisioning and claims

The service operator provisions private `FABRIC_CONNECT_PRINCIPALS` JSON: at most 64 credentials, each with `id`, `account`, `token_sha256` and up to 64 exact grants. Only SHA-256 bearer hashes belong on the server. Multiple credentials may share a principal ID within the same account, allowing read-only credentials without granting claim/poll/ack. Tokens must be distinct. IDs are ASCII; account, project and automation segments match `[a-z0-9][a-z0-9_-]{0,62}`.

```json
[
  {
    "id": "owner",
    "account": "team",
    "token_sha256": "SHA256_OF_PRIVATE_BEARER",
    "grants": [
      {"project": null, "automation": null, "permissions": ["claim"]},
      {"project": "fabric", "automation": null, "permissions": ["claim"]},
      {"project": "fabric", "automation": "issues", "permissions": ["claim", "read", "poll", "ack"]}
    ]
  }
]
```

Replace the hash placeholder with a lowercase 64-hex digest of a high-entropy private token; never use example credentials. `null` selects exactly the account/project claim, not a wildcard. Only `claim` is allowed on these parent grants. Provisioning grants authorizes a claim; it does not silently transfer an existing namespace. Owner changes and policy/secret/worker reassignment are not permitted by the claim API.

All operations below use HTTPS. Claims require the explicitly scoped bearer token and durable compare-and-swap success:

| Operation | Path | JSON body |
| --- | --- | --- |
| Claim account | `POST /v1/accounts/team/claim` | `{}` |
| Claim project, after account | `POST /v1/projects/team/fabric/claim` | `{}` |
| Claim automation, after project | `POST /v1/projects/team/fabric/automations/issues/claim` | `{"worker_id":"PRIVATE_WORKER_ID","webhook_secret":"64_LOWERCASE_HEX_RANDOM_SECRET","policy":{"repo":"satansdeer/fabric","author":"satansdeer","sender":"satansdeer","label":"ready"}}` |

Secrets in this example are placeholders, not usable values. Supply a distinct cryptographically random 32-byte hex webhook secret per route and configure the same value with its GitHub webhook. Repository names and policy values are bounded ASCII identifiers. Identical claims are idempotent; ownership/policy conflicts are rejected. A successful automation claim returns its canonical absolute `url`; append `/github` for provider ingress, `/status` for redacted inspection, `/poll` for delivery and `/events/<event_id>/ack` for acknowledgement. Each suffix is scoped independently (`read`, `poll`, `ack`). GitHub ingress uses that route's HMAC secret, never a worker bearer.

### In-place migration of the deployed route

Keep the existing `FABRIC_CONNECT_WORKER_ID`, `FABRIC_CONNECT_WORKER_TOKEN_SHA256` and `FABRIC_CONNECT_WEBHOOK_SECRET` private bindings intact for the first upgraded request, and provision `FABRIC_CONNECT_PRINCIPALS` (an empty array is valid for a legacy-only transition). Do not rename the Durable Object class, migration tag, object name `satansdeer/fabric/issues` or SQLite table. The legacy namespace is reserved against other principals while migration is pending.

The first valid signed delivery or authenticated worker operation claims the existing account/project for principal `satansdeer` and CAS-upgrades the same route-state key `satansdeer/fabric/issues` to schema 2. Queued receipts, rate history, delivery leases, running handles and acknowledgement/tombstone history are preserved. Parent ownership writes are idempotent; interruption before the route CAS leaves the v1 record intact and the next request resumes migration. Invalid or unauthorized requests do not migrate or replace the inbox.

The existing webhook URL remains `https://fabric-connect.satansdeer.workers.dev/v1/projects/satansdeer/fabric/automations/issues/github`. It is the canonical claimed route, not an alias or second service. Its previous worker token hash is persisted as an exact-route credential with only read/poll/ack, never claim or another project's access. Its webhook secret/policy and worker binding are also persisted; changing or removing bootstrap bindings after successful migration does not reassign the route or invalidate acknowledged events. Confirm schema 2 and retained receipts through operator-owned SQLite inspection before removing bootstrap bindings. There is no public raw-state endpoint.

## Local use

The connection file must be an owner-owned, nonsymlink, mode-0600 JSON file:

```json
{"schema_version":2,"endpoint":"https://fabric-connect.satansdeer.workers.dev","identity":{"account":"satansdeer","project":"fabric","automation":"issues"},"worker_id":"OWNER_PROVISIONED_ID","token":"OWNER_PROVISIONED_TOKEN"}
```

Use the existing owner-provisioned connection at `~/.config/fabric/connect/cloudflare.json`; do not copy credentials into the checkout. A private schema-1 connection is atomically upgraded to the explicit legacy identity after validation; the endpoint, worker ID and token are unchanged. Existing legacy journals are migrated in place without changing their Core idempotency keys. Other identities have separate local journal/lock namespaces, so equal event hashes cannot reuse another route's mapping. Status does not admit work:

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

`status` supports claimed identities and reports the stable URL, policy and lifecycle counts. The supervised `once` consumer still uses the existing approved `satansdeer/fabric` local issue workflow; it rejects a different remote policy before leasing anything. Generic external GitHub setup belongs to #11. A claimed identity can select that existing policy without changing the normal Fabric worker lifecycle or adding hosted execution.

Worker resource flags are shared with direct issue dispatch: `--timeout-seconds 7200 --max-log-bytes 67108864 --max-artifact-bytes 134217728 --copy-limit-bytes 104857600` explicitly selects a two-hour emergency deadline, 64 MiB combined raw/events, 128 MiB artifacts and 100 MiB copy fallback. Defaults, bounds and approval requirements are in the [operator workflow](ISSUE_WORKFLOW.md#dispatch-one-bounded-worker). Limits are validated before polling, reported locally, and persisted in the event configuration and request/idempotency identity before admission. Reconnect with identical limits; changing them on a bound event is refused, not a new run. The 30-second consumer observation window never replaces the selected worker deadline.

Watch the actual worker using [local metadata or opt-in raw logs](../README.md#watch-a-running-worker-locally); live and retained reads stay on the Mac. Connect receives only existing sanitized handles/state, never raw harness logs or the new activity payload. Two inspections can inform an explicit existing `cancel`; there is no inactivity kill or automatic restart. Trusted-local native execution and environment fidelity are unchanged, not a sandbox claim.

## Provider-independent development

The [local issue bridge](ISSUE_WORKFLOW.md) reads GitHub directly and needs no public URL. It remains a fallback for explicitly approved issue-driven work independent of hosted ingress.

For signed-webhook development, a [Cloudflare Quick Tunnel](https://developers.cloudflare.com/tunnel/get-started/quick-tunnels/) forwards a temporary HTTPS address to a loopback receiver without an account or domain. Keep Core private; expose only the bounded, signature-checked Connect handler, with separate disposable webhook and worker credentials. The local file-backed `test-server.mjs` is a single-process integration fixture, not a production service. The address changes on restart and delivery is unavailable when the Mac or tunnel is offline; the local file store does not make ingress always-on.

In [#20](https://github.com/satansdeer/fabric/issues/20), real GitHub opened/ready-label events survived receiver restart, reused one actual completed Core admission, and produced 14 artifacts whose bytes, sizes and SHA-256 digests were verified. GitHub's exact signed redelivery returned `completed` with `duplicate: true`; a subsequent consumer invocation was idle. Unsigned ingress and unauthenticated worker requests returned 401. The temporary hook, tunnel and receiver were removed/stopped after verification. No model calls were made.

The permanent adapter is in [`integrations/connect/cloudflare/`](../integrations/connect/cloudflare/): [Workers on `workers.dev`](https://developers.cloudflare.com/workers/configuration/routing/workers-dev/) forwards all claimed routes to the same existing [SQLite-backed Durable Object](https://developers.cloudflare.com/durable-objects/platform/pricing/). Each automation has a separate CAS row; account/project ownership has immutable separate rows. Conditional writes use synchronous SQLite transactions. Bounded route JSON is SQL TEXT, not a 128 KiB Durable Object KV value. No custom domain, Netlify account, provider dependency, scheduler or event bus is added.

## Live deployment evidence

Historical controller-supplied production observations for [#21](https://github.com/satansdeer/fabric/issues/21), before the issue-9 source migration:

- Workers and SQLite Durable Object deployment succeeded without a paid upgrade or DNS changes.
- Public `GET /health` returned 200; unsigned ingress and unauthenticated worker access each returned 401.
- Repository webhook `715419509` is active, issues-only, with SSL verification enabled, at `https://fabric-connect.satansdeer.workers.dev/v1/projects/satansdeer/fabric/automations/issues/github`.
- A real signed ready-label delivery for #21 queued while the local consumer was offline, survived production redeploy, and was subsequently consumed through real Core/fake-harness execution.

This proves permanent ingress/redeploy durability and provider-free consumption, not model-backed implementation. [#23](https://github.com/satansdeer/fabric/issues/23) and [#25](https://github.com/satansdeer/fabric/issues/25) separately supply native-worker evidence. Ingress queues unattended; execution remains supervised and outbound-only through the existing `status`/`once` consumer, not a new daemon.

Those observations concern the original deployment, not acceptance of this source. The upgraded server stores only scoped principal hashes and route-specific webhook secrets; no hosted GitHub token, issue body/title, raw local harness logs or arbitrary plugin code. Authenticated status returns allowlisted identifiers, lifecycle counts, safe failure codes, attempt/backoff timestamps and worker heartbeat. Responses never expose token hashes, webhook secrets or internal acknowledgement records; errors withhold raw exceptions. Liveness is not execution evidence.

## Historical Netlify/domain fallback

The reserved Netlify site `fabric-connect-polyglo-satansdeer` (ID `6400a187-1d3a-49a4-aef1-0e4510571f1c`) was blocked by exhausted account credits and is not deployed. Its fallback address is `https://fabric-connect-polyglo-satansdeer.netlify.app`; no webhook was created against that undeployed endpoint. The proposed `fabric-connect.polyglo.tv` domain returned NXDOMAIN and registry WHOIS “No Data Found” and was detached. Existing DNS and unrelated sites were left unchanged.

These limitations do not gate the live Cloudflare path. The Netlify adapter uses site-wide Blobs, strong reads and conditional writes, but still lacks production/redeploy verification. Revisit only if the owner explicitly selects that fallback; no credit purchase, domain registration or authorization renewal is required to use the current Cloudflare connection.

## Bounds and recovery

Each route is bounded: 128 KiB request bodies, 64 active events, 256 retained records, 60 new events/minute persisted across restart, 24-hour queued/delivering expiry and seven days of terminal receipt retention. Schema 2 permits at most 514 KiB route state, reserving 2 KiB above the original 512 KiB cap so an otherwise full v1 inbox can migrate without dropping events. A further 600 requests/minute per active Durable Object handler bounds work on invalid signatures, claims and replays; this defensive counter resets on cold start, unlike the durable new-event limit. All non-health requests consume it. Capacity returns 429 rather than evicting required history; bounded CAS exhaustion returns 503 rather than acknowledging unpersisted work.

`queued` means durably accepted, not executed. `delivering` has a renewable 120-second lease, not confirmed Core admission. `running` requires a matching lease and accepted batch/run handles. Only subsequent acknowledgement with the same handles can record `completed` or `failed`; only explicit `approval_mismatch`/`issue_rejected` can fail before admission without handles. Terminal acknowledgement is idempotent; a stale lease or cross-route/principal acknowledgement cannot update another receipt.

Expired delivery leases become queued with 1/2/4/8-second backoff; the fifth failed lease becomes `failed` (`delivery_exhausted`). Offline queued/delivering work becomes `expired` after 24 hours, irrespective of retries. Maintenance runs lazily on valid route activity, not via a new scheduler. Running work is retained, never blindly requeued or expired merely because a worker disconnects: only its existing local Core evidence may settle it. A full inbox with unresolved running work rejects new input rather than duplicating admissions.

Offline workers leave events queued. Run `status` after reconnect, then explicitly approve and consume the chosen issue. Do not delete local journals to force a retry: retained Core handles and evidence are the recovery authority. The hosted inbox is not an artifact store; inspect and verify actual artifacts through Fabric locally.

## Provider-free verification

Issue-9 worker checks: **NOT RUN BY WORKER** (source-only resource boundary: no builds, lint, tests, formatters or dependency installation). Regression sources cover claim/read/poll/ack isolation, invalid signatures/bodies/policies, delivery UUID conflicts and authenticated replays, stable offline URLs, bounded expiry/backoff, in-place credential/event migration and actual on-disk SQLite runtime restart. The existing local fixture also persists multi-key ownership/inbox state across process restart and migrates its previous single-record file format.

Manager-owned commands, using only already provisioned dependencies and binaries:

```sh
node --test integrations/connect/service.test.mjs
(cd integrations/connect/cloudflare && npm test)
python3 scripts/test-fabric-connect-request.py -v
python3 scripts/test-fabric-connect.py --fabric "$PWD/target/debug/fabric"
```

Run the Python commands from the repository root (the Cloudflare command runs in its own directory). The real-adapter test bundles a test-only subclass of the production Durable Object, runs the actual service through Miniflare, disposes the runtime and reopens its persisted SQLite directory. Test-only clock/seed/read/CAS endpoints are not part of the deployed adapter. Production deployment, GitHub setup, exact-head/main CI, release and independent consumer acceptance remain manager-owned. Historical fixture/live observations above do not certify this new migration or a model-backed implementation.

The observed [SIGTERM CI failure](https://github.com/satansdeer/fabric/actions/runs/45035376872) was a partial heartbeat PID record (#22); shared tree readiness now requires complete parent/child records. A later [typed-conformance CI failure](https://github.com/satansdeer/fabric/actions/runs/45036747038) exposed a manager startup SQLite handle retained across shutdown (#24); that startup-only handle now closes before readiness. The deterministic readiness regression, actual SIGTERM lifecycle test, binary build and actual typed conformance passed locally after these fixes. The conformance fixture owns/reaps the foreground manager and prints PASS only after successful resource cleanup. Controller-supplied GitHub macOS CI runs [45039604770](https://github.com/satansdeer/fabric/actions/runs/45039604770) and [45039529438](https://github.com/satansdeer/fabric/actions/runs/45039529438) at `c6583cb` completed successfully. These are recorded observations, not checks rerun here; #15 retains the real model-backed issue-to-reviewed-PR acceptance gate.
