# Product boundary

Fabric is a macOS-first, single-user, trusted-local coding-agent worker layer. It owns immutable prepared bases, independent writable workspaces, durable run identities, bounded evidence and verified cleanup. Controllers and worker harnesses are independent. OMP and Codex own their native authentication, model defaults, skills, extensions, plugins and MCP configuration.

## Supported boundary

- Local filesystem execution only. No cloud, BYOC, remote isolation or multi-tenant service. Unsupported placement must fail before allocating work; local is never a fallback for a remote requirement.
- A private HOME or explicit environment allowlist is scoping, not an OS credential/process sandbox. Trusted native configuration can execute configured commands and access credentials as the local user.
- Fabric never logs in to providers, copies native auth stores, parses secrets or builds a credential vault. Native-context access requires explicit local consent; advanced explicit profiles retain their enforcement.
- A durable run is not a process. Results/evidence outlive controller processes. An uncertain interrupted run must not be reported successful or silently rerun.
- A workspace is disposable only after ownership, leases, capture and cleanup prerequisites pass. Prepared bases and preserved evidence are not disposable run workspaces. Uncertainty means retain and explain, never guess a deletion path.
- Storage admission is best effort and measures logical bytes; APFS physical/exclusive usage and host disk exhaustion are not guaranteed. Adapter request budgets are not hard billing caps.

## Corrective milestone status

Baseline: `8e926f934f6508d114579e87d53e60a6d9ebdaf6`. At this baseline, batch execution and the OMP extension are foreground only. Durable asynchronous submission, guided initialization and native om-pstack local swarm routing are corrective deliverables, not pre-existing capabilities.

Provider-free fake-harness tests prove lifecycle/control behavior, not provider authentication or model-backed execution. Live OMP/Codex acceptance requires explicit spending authorization. `LOCAL_WORKER_API_READY`, `PSTACK_LOCAL_SWARM_READY` and `LIVE_HARNESS_VERIFIED` are separate evidence gates; no `PSTACK_CLOUD_READY` claim is valid without a real remote backend.

## Out of scope

Cloud infrastructure, distributed scheduling, generic workflows/DAGs, credential brokerage, a custom MCP transport, native session resume, automatic side-effecting retries, Linux porting and cosmetic crate restructuring.
