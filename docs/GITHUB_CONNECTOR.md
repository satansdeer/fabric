# External GitHub issues connector (#11)

Manager-verified provider-free implementation; real GitHub installation/response remains opt-in and unverified. No deployment or live-model execution is claimed.

## Boundary and flow

`integrations/github/plugin.mjs` implements the existing external SDK connector contract, including optional `subscribe` and `reply`. `bridge.mjs` uses the SDK scaffold/content package and actual negotiated external process. Node >=22.18 (including stdlib `node:sqlite`) and an existing Fabric CLI are required; no npm dependencies, Core rebuild or provider/model/auth overrides.

The macOS release includes the three integration modules, this guide and the opt-in response check. Run commands from the extracted release directory; its SDK commands use `sdk/cli.mjs` instead of the checkout's `sdk/typescript/cli.mjs`.

1. GitHub sends an `issues` webhook to operator-owned HTTPS ingress, forwarded without body rewriting to the bridge's loopback `/github` listener.
2. The reviewed external plugin verifies HMAC-SHA256 of the **original UTF-8 raw body**, then checks event/action, repository name/numeric ID, installation ID, sender and issue author login/numeric ID, open non-PR issue URL, readiness label and explicitly approved issue snapshot.
3. It normalizes to the existing generic contract. Business identity hashes repository name/ID, installation ID, issue number and snapshot approval SHA; neither delivery UUID nor action chooses execution identity. Snapshot hashing matches [#16](ISSUE_WORKFLOW.md): sorted, ASCII-escaped compact JSON of repo/issue_number/author/title/body. No event field selects worker configuration.
4. Before forwarding, the listener saves the first signed envelope in a private inbox. It signs the existing generic Connect `/webhook` body with the separately provisioned generic route secret. Core and hosted Connect receive no new GitHub-specific code or policy. Approval time is fixed in private configuration so duplicate/reordered provider events serialize identical generic bytes.
5. Existing `fabric connect` uses the owner-approved private Automation, immutable prepared source and existing Core manager/fake or native harness lifecycle. Hosted admission is **not** a run result or successful execution.
6. Explicit `reply` invokes the SDK outbound hook. It requires the exact signed approved delivery, matching terminal hosted normalized receipt, permanent local SQLite event→batch/run admission mapping, real `fabric results` output, exact run/artifact-manifest identity, and size/SHA256 verification of every retained artifact. It then checks the installation token's single-repository scope and posts/reconciles one scoped issue comment with actual handles and the observed success/failure boolean. It does not publish artifacts, issue text, logs or credentials.

The bridge neither polls for work nor schedules workers. Outbound hooks are explicit operator actions, not automatic Core callbacks. No additional execution grant is created by a webhook, label, plugin install or response check.

## GitHub installation and permissions

Create/review a GitHub App outside Fabric. Install it on **one selected repository**, with **Issues: read and write**, **Metadata: read**, and subscribe to **Issues** webhooks. Set a separate high-entropy webhook secret of 32–256 bytes; keep the exact secret bytes in an owner-private file without an added newline. This implementation supports `opened`, `edited`, `reopened` and `labeled` (only addition of the configured label). Configure no other event types for this receiver; pings/unsupported events are rejected, not enqueued.

Record the actual repository and installation numeric IDs, author/sender numeric IDs and logins, and App bot numeric ID/login used for comments. Issue authors/senders must be ordinary GitHub logins; the response actor may have the `[bot]` suffix. Identity matching is exact, including case. Reinstallation or moving repositories requires operator review, not an implicit migration.

Issue a short-lived **installation access token**, restricted to that repository and Issues write permission, outside this connector. Put only that token's bytes (32–512 printable ASCII, no newline) in a mode-0600 file. Renew it through the operator's existing token tooling; this integration neither stores an App private key nor mints/refreshes tokens. PATs/App JWTs are not the supported outbound credential. No credential belongs in argv, package content, source checkout or logs.

`subscribe` calls GitHub's installation-repositories endpoint and requires exactly the configured repository/name/ID. It reports the configured installation ID and required permissions/events; it **does not** register a webhook or claim to introspect App subscription settings, token installation ID or Issues-write permission. The operator verifies those App settings; signed ingress checks installation ID, and a real response write tests write permission. All production API requests use `https://api.github.com`, reject redirects and have bounded response sizes/timeouts.

References: [signature validation](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries), [installation repository scope](https://docs.github.com/en/rest/apps/installations#list-repositories-accessible-to-the-app-installation), [issue-comment permission](https://docs.github.com/en/rest/issues/comments#create-an-issue-comment).

## Separate generic route and local authority

Provision a **new** generic automation using [Connect's generic claim and Automation setup](CONNECT.md#generic-outbound-local-agent-10). Do not change an existing legacy `/github` URL, claim, policy, webhook secret, queued event, receipt, connection or durable state. The deployed legacy GitHub path remains usable unchanged. This connector's loopback `/github` is a different, operator-provisioned receiver; it does not replace that deployed endpoint.

The claimed generic policy fixes `source_id`, `tenant_id`, `scope`, `route`, and `max_payload_bytes:8192`. Use those exact identities in the plugin binding and local Automation. Generic claim secret: the existing 64-lowercase-hex string. The plugin's `connect.secret_ref` holds that string's exact ASCII bytes, not decoded hex and not the GitHub webhook secret. The schema-2 `connection_ref` contains the existing scoped Connect bearer for read/poll/ack and the exact same HTTPS origin/account/project/automation. These are three independent secrets; the GitHub installation response token is a fourth.

The trusted private Automation selects project, local placement, one prepared state, installed harness, fixed prompt, explicit approved resource bounds and `data_fields:["issue"]`. Treat that projected issue object as untrusted data. Remote `worker`, `model`, `harness_id`, `prompt`, source/config/resource/routing authority fields are rejected by the provider integration, not promoted into configuration. Generic field projection and Core's immutable policy remain authoritative too.

Use the existing foreground agent, for example:

```sh
"$FABRIC" --root "$ROOT" connect --connection "$CONNECTION" \
  --automation "$AUTOMATION" --allow-model-spend
```

This real-harness command requires a **separate explicit operator execution/spending authorization** and existing native consent/configuration; do not run it merely to install or check the connector. The provider-free fixture instead explicitly selects its own real fake executable. No provider/model/effort/auth selection is introduced here.

## Private configuration and approval

All file references must be canonical absolute paths. Config, signed requests and secret/token/connection files must be owner-owned, single-link regular files with no group/other permission bits. Use a canonical owner-private Fabric root and mode-0700 inbox. Secrets are read only by operations that need them. Config schema is exact; unknown configuration fields are rejected.

Example configuration shape (operator supplies actual identities/paths; empty approvals **deny all admission**):

```json
{
  "version": 1,
  "binding": {"source_id":"webhook","tenant_id":"team","scope":"project","route":"jobs"},
  "repo": "example-owner/example-repo", "repository_id": 100, "installation_id": 200,
  "author": {"id":300,"login":"example-author"},
  "sender": {"id":300,"login":"example-author"},
  "label": "ready", "actions": ["opened","edited","reopened","labeled"],
  "approvals": [],
  "webhook_secret_ref": "/private/var/fabric/github-webhook.key",
  "connect": {
    "endpoint":"https://fabric-connect.satansdeer.workers.dev",
    "secret_ref":"/private/var/fabric/generic-route.key",
    "connection_ref":"/private/var/fabric/connection.json",
    "allow_loopback_http":false
  },
  "local": {"fabric":"/private/var/fabric/bin/fabric","root":"/private/var/fabric/root"},
  "response": {
    "api_origin":"https://api.github.com", "token_ref":"/private/var/fabric/github-installation.token",
    "actor":{"id":900,"login":"example-app[bot]"}, "allow_loopback_http":false
  }
}
```

Review the exact issue snapshot before adding an approval `{ "issue_number": NUMBER, "sha256": DIGEST, "timestamp": UNIX_SECONDS }` to the private config. Use the existing #16 preview for `satansdeer/fabric`; other repositories use this bridge's provider-authenticated offline preview. For offline preview, the operator retains the original signed request in this exact private JSON envelope:

- `body`: JSON string of `{event:"issues",delivery:GITHUB_DELIVERY_UUID,raw:ORIGINAL_UTF8_BODY_STRING}`; preserve original raw body, not reserialized provider JSON.
- `authorization`: exact `X-Hub-Signature-256` value.
- `nonce`: delivery UUID; `timestamp` and `expires_at`: integer seconds (the bridge refreshes transport-only freshness for each negotiated verification session).

```sh
node integrations/github/bridge.mjs preview --config "$CONFIG" --request "$REQUEST"
```

Preview verifies signature and allowlists but does not require a preexisting snapshot approval, enqueue or run work. The receiver does **not** retain unapproved incoming events. Obtain/review the snapshot before enabling its label/redelivery, or save the signed request through operator-controlled ingress for offline review. Add only its printed exact digest to approvals, with the current timestamp; publish the private config atomically. Changing title/body/author changes the digest and requires fresh approval; removing an approval revokes future forward/reply authorization but cannot undo existing work. A deliberately approved changed snapshot is a new business event and can execute a new run.

## Package, pin, receive and reply

Set shell variables to canonical absolute paths in operator-owned private storage: `CONFIG`, a **new nonexistent** `PACKAGE` directory, `REGISTRY`, `ROOT`, `INBOX`, and existing `FABRIC`. Review all three integration modules and the vendored SDK before trusting the package.

```sh
node integrations/github/bridge.mjs package --config "$CONFIG" --out "$PACKAGE"
```

Record the returned `sha256` as `PACKAGE_SHA`. This uses existing SDK scaffolding and packages the reviewed modules plus SDK. The package pins the config **path**, not mutable config contents or credentials; that owner-private config is trusted operator authority. The acceptance script generates the custom signed SDK fixture and exercises conformance before installation.

```sh
node sdk/typescript/cli.mjs install "$PACKAGE" --registry "$REGISTRY" \
  --fabric "$FABRIC" --version 1.0.0 --sha256 "$PACKAGE_SHA" --trust
"$FABRIC" --root "$ROOT" plugins --registry "$REGISTRY" check github-issues
PLUGIN="$REGISTRY/packages/$PACKAGE_SHA"
node integrations/github/bridge.mjs subscribe --config "$CONFIG" \
  --plugin "$PLUGIN" --sha256 "$PACKAGE_SHA"
mkdir -m 700 "$INBOX"
node integrations/github/bridge.mjs serve --config "$CONFIG" \
  --plugin "$PLUGIN" --sha256 "$PACKAGE_SHA" --inbox "$INBOX" --port 8788
```

The listener binds only `127.0.0.1`. Use existing operator-owned HTTPS ingress/reverse proxy preserving raw bytes and GitHub headers; this source does not provision TLS/DNS/tunnels/services. Do not expose the manager socket or Core CLI. No public local endpoint is created by `fabric connect` itself. `forward --config ... --plugin ... --sha256 ... --request ...` is the one-shot equivalent for an operator-retained signed envelope; unlike `serve`, it does not save a copy.

The receiver returns 202 for an accepted new generic event, 200 for a retained duplicate, and a fixed rejection on validation/forward errors. A timeout or rejection after uncertain forwarding is **not** proof of no admission: retain the same approved snapshot/timestamp and retry/reconcile the same business identity. Never mint a different approval to retry an uncertain event.

After the existing native agent has produced a terminal local run, select its saved `<business-id>.json` envelope (provider normalization ID, before Connect's additional event-ID hash) and invoke:

```sh
node integrations/github/bridge.mjs reply --config "$CONFIG" \
  --plugin "$PLUGIN" --sha256 "$PACKAGE_SHA" --request "$REQUEST"
```

The caller cannot supply proposed handles, result state or comment content. The hook reads local `fabric.db` read-only, then uses actual CLI results and retained artifacts. Missing/changed mapping, missing artifacts, nonterminal run, receipt mismatch or denied provider permission fails closed. An authenticated forged hosted receipt carrying a different real run's handles cannot pass the permanent local admission mapping check. Keep the original root/database/artifacts through response posting; this hook is intentionally not a portable receipt-only responder.

Comments state terminal status and `succeeded: true/false`, exact batch/run/artifact-manifest handles, and that a worker outcome does not prove checks passed. They never merge or create branches/PRs. Current issue lookup rechecks number/URL/author/non-PR identity, not unchanged title/body/open status: the response explicitly concerns the retained **approved snapshot**, even if GitHub later changes.

Per-event owner-private directory locks serialize responses on the same root. Marker discovery requires the configured numeric bot identity and refuses multiple/forged markers. A lost successful POST is reconciled by listing comments on a later explicit call; no blind write retry. An interrupted process can leave `<ROOT>/github-responses/<host-event-id>.lock`; after verifying no response process is active, the operator may remove that empty stale lock and reconcile. Do not remove live locks or run multiple independent responder roots for one route.

## Limits and compatibility

- At most 8,192 UTF-8 bytes for the wrapped SDK request body **including escaped raw provider JSON and headers**, and independently for the generic event. Real GitHub payloads exceeding this supported small-issue envelope are rejected without truncation/admission. This is not an unlimited GitHub webhook receiver.
- At most 100 approved snapshot entries and 256 saved signed envelopes per inbox. Capacity fails closed; existing saved duplicates remain reusable at capacity. Operator archives old private envelopes only after resolving their work/responses.
- The first new generic event must be within Connect's existing 24-hour/five-minute-future window, using the fixed approval timestamp. Retained exact duplicates preserve that timestamp; changing it is a payload conflict, not a retry. Hosted receipts retain the existing seven-day boundary; absent receipts are not reconstructed for posting. Core's permanent idempotency mapping remains authoritative for execution.
- HTTP calls have 15-second deadlines and 1-MiB response bounds; SDK external normalization has a 10-second process deadline and outbound hooks 240 seconds. One ingress request at a time; additional concurrency returns 503. No scheduler/retry queue was added.
- Response comment discovery is bounded to ten pages of 100 comments. Larger/ambiguous histories reject rather than risk duplicate comments. Total locally verified artifacts must fit 128 MiB. Existing approved task resource ceilings remain in the Automation, not event data.
- Production response API is GitHub.com only, not GitHub Enterprise. Explicit literal-loopback HTTP configuration exists solely for provider-free fixtures. No deployment, token provisioning, automatic token renewal or automatic webhook registration is claimed.
- Generic SDK conformance gained optional fixture `authorization` for third-party signing formats; omitted authorization uses the unchanged generic HMAC path. Generic connector/runtime, deployed legacy URLs/state/GitHub consumer and complete original OMP plugin are unchanged.

## Manager verification

With an already-built CLI and sibling `fabric-fake-harness`, Node >=22.18, Python 3 and git:

```sh
python3 scripts/test-fabric-github.py --fabric "$FABRIC"
node --test sdk/typescript/test/connector.test.mjs integrations/connect/generic.test.mjs integrations/connect/service.test.mjs
```

The source fixture passed in 32.41 seconds with actual successful and failed worker runs, sealed artifacts and scoped HTTP replies; no native provider calls. All 25 focused SDK/generic/service regressions passed with zero skips. To exercise the published modules and SDK rather than checkout modules, add `--bundle /absolute/extracted/release` to the fixture command. Public-release acceptance is recorded separately on issue #11.

The first fixture uses private temporary state, real external SDK/conformance/install/check, real HTTP signed ingress and unchanged generic Connect service, actual CLI/manager/approved prepared fake worker, retained artifact bytes and a local provider HTTP fixture. It covers identity/signature/approval/config/model-spend denials before execution; duplicate/reordered delivery without new work; successful/failed runs; independent repository/write permission denials; a lost committed provider response; forged bot identity and forged authenticated hosted handles; and plain generic delivery through the same route/agent. Only the provider HTTP API and harness task are deterministic fixtures; admission and harness/artifact lifecycle are not mocked. No provider calls or helper workers are required. Failures retain the temporary fixture path; `--keep` retains successful evidence too.

### Explicit opt-in real installation/response check

Only the manager/operator may opt in. Use a real selected-repository App installation token as above, real production HTTPS route, exactly **one** approved issue snapshot, original real signed envelope, and an already authorized terminal native local run with retained artifacts. Send a real supported GitHub issue delivery through the configured HTTPS receiver and observe the existing native agent separately; no worker execution is authorized by the following response command.

```sh
node scripts/check-fabric-github.mjs --allow-github-comment-write \
  --config "$CONFIG" --plugin "$PLUGIN" --sha256 "$PACKAGE_SHA" \
  --request "$REQUEST" --repo "$REPO" --issue "$ISSUE_NUMBER" \
  --approval-sha256 "$APPROVAL_SHA"
```

The script refuses loopback/test endpoints, other repository/issue/digest scope, absent real terminal evidence and denied installation scope. It negotiates actual subscribe/reply hooks and outputs only the observed comment/result handles. This performs real GitHub reads and can create/update one bot comment; it never forwards/adopts a fresh event, grants model spend or launches a worker. Expired tokens or old/missing hosted receipts fail, not simulate success. **Real installation/response: NOT_RUN_BY_WORKER.**
