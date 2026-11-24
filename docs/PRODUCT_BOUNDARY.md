# Product boundary

Fabric is a macOS-first, single-user, trusted-local coding-agent worker layer. It owns immutable prepared bases, independent writable workspaces, durable run identities, bounded evidence and verified cleanup. Controllers and worker harnesses are independent. OMP and Codex own their native authentication, model defaults, skills, extensions, plugins and MCP configuration.

## Supported boundary

- Local filesystem execution only. No cloud, BYOC, remote isolation or multi-tenant service. Unsupported placement must fail before allocating work; local is never a fallback for a remote requirement.
- A private HOME or explicit environment allowlist is scoping, not an OS credential/process sandbox. Trusted native configuration can execute configured commands and access credentials as the local user.
- Fabric never logs in to providers, copies native auth stores, parses secrets or builds a credential vault. Native-context access requires explicit local consent; advanced explicit profiles retain their enforcement.
- A durable run is not a process. Results/evidence outlive controller processes. An uncertain interrupted run must not be reported successful or silently rerun.
- A workspace is disposable only after ownership, leases, capture and cleanup prerequisites pass. Prepared bases and preserved evidence are not disposable run workspaces. Uncertainty means retain and explain, never guess a deletion path.
- Environments are durable owner/UID/target-bound private homes, separate from disposable run/workspace identity. Local primitives support ensure, inspect, explicit destroy and disposable workspace creation. The separate optional `pty.v1` extension supports owner-only `fabric shell`; unsupported targets, infrastructure diagnostics and start/stop fail typed unsupported, without a local fallback.
- Environment HOME/XDG state survives target/worker recreation and normal run cancel/recovery/reclaim. No implicit native auth/config export; unsafe native contexts cannot bind the home. Default artifact capture excludes it, but secrets deliberately written to cwd/stdout/stderr are captured like other run data.
- Environment destroy requires exact ID confirmation, immutable owner/target and descriptor-verified resource ownership plus inactive native run leases. Active shells hold the same namespace shared lease through target-side child reaping. Symlink/replaced/unowned roots and stale/unknown leases fail closed. Owner names are labels, not credentials or multi-user authentication; same-UID/root interference remains outside the boundary.
- Private permissions are not encryption. Environment secrets remain ordinary local plaintext; Fabric implements no at-rest encryption, sandbox or vault. Independently managed filesystem/storage encryption is an operator responsibility.
- Interactive shells have bounded byte queues, idle/max duration, resize and explicit close/cancel. They do not create supervised run output/artifacts, Connect inbox content or GitHub comments. Terminal state is restored on normal/error exits while the terminal exists, not after SIGKILL/host loss. Reconnect is a new shell. Trusted commands/plugins and explicitly user-owned recordings are not universally redacted.
- The provider-free PTY relay fixture mutually authenticates fresh TCP connections with a private fixture HMAC key, then relays to a separate native target. It uses loopback and shared owner-private filesystem resources; data is not encrypted. This proves a target transport/session contract, not remote run isolation, production ingress, cloud placement or provider login.
- Storage admission is best effort and measures logical bytes; APFS physical/exclusive usage and host disk exhaustion are not guaranteed. Adapter request budgets are not hard billing caps.
- Plugin timeout metadata observes only the session's actual owned direct child before teardown: bounded PID/group/wait/budget/RPC counters and buffered byte count, never payload, argv or environment. Non-reaping exit observation preserves the process-group PID anchor; it is not descendant supervision, a scheduler trace, killer attribution or proof that a missing response's cause is known. Existing trusted-local, capture, lifecycle and authorization limits remain unchanged.

## Corrective milestone status

Baseline: `8e926f934f6508d114579e87d53e60a6d9ebdaf6`. At this baseline, batch execution and the OMP extension are foreground only. Durable asynchronous submission, guided initialization and native om-pstack local swarm routing are corrective deliverables, not pre-existing capabilities.

Provider-free fake-harness tests prove lifecycle/control behavior, not provider authentication or model-backed execution. Live OMP/Codex acceptance requires explicit spending authorization. `LOCAL_WORKER_API_READY`, `PSTACK_LOCAL_SWARM_READY` and `LIVE_HARNESS_VERIFIED` are separate evidence gates; no `PSTACK_CLOUD_READY` claim is valid without a real remote backend.

## Out of scope

Cloud infrastructure, distributed scheduling, generic workflows/DAGs, credential brokerage, a custom MCP transport, native session resume, automatic side-effecting retries, Linux porting and cosmetic crate restructuring.
