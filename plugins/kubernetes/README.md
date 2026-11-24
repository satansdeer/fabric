# External Kubernetes target

Issue #33 implementation; worker delivery **checks=NOT_RUN_BY_WORKER**. Manager-observed provider-free verification is recorded below. No native OAuth result, version bump, or provider/model/effort/auth default change is claimed.

This explicitly installed target uses the public [`remote.v1` contract](../../docs/PLUGIN_PROTOCOL.md#generic-remote-target-backend-v1). Core and Runner contain no Kubernetes dispatch. `fabric-remote-agent` is a generic target-side executable: actual Linux process supervision, PTY allocation, HOME/workspace leases, source verification and capture happen inside the Pod. Controller directories are bounded transfer staging, **not** a mirror advertised as remote HOME. Acknowledging local grants cannot select this backend.

## Prerequisites and trust

* Owner-reviewed Python **3.11+**, Node **22.18+**, a compatible Fabric controller and an explicitly supplied `kubectl`. No pip/npm dependencies, hooks, context activation or daemon setup are needed by this plugin.
* A separately bootstrapped, bounded namespace and storage class. Normal operation cannot create/delete namespaces, RBAC, nodes, storage classes or volumes directly. Never point the plugin at existing manager fixture resources.
* An owner-supplied private kubeconfig and **explicit context**. There is no ambient current-context, `KUBECONFIG`, home discovery or in-cluster credential fallback. Every kubectl command supplies kubeconfig, context and namespace; its environment is cleared. Kubeconfig, configuration and kubectl bytes are SHA-256 pinned. Owner-chosen kubeconfig authentication helpers and referenced key files remain part of the owner's trusted kubectl configuration, not something Fabric imports into a Pod.
* Installed code, Python/system libraries, controller/cluster administrators, image contents and same-UID code are trusted. Content pins are not a sandbox, runtime signing or defense against a malicious trusted user replacing binaries/files between validation and execution. No credential bytes belong in packages, source, artifacts or bug reports. Keep config/package paths private too.

The manager-reported private fixture used Colima 0.10.3/VZ arm64 on macOS, kubectl v1.35.10 and k3s v1.35.10+k3s1 with Rancher local-path. Those are provenance, **not worker-observed execution or authority to access that fixture**. An independently owned kind/minikube/k3d cluster is also suitable with the image/storage/RBAC contract below.

## Image and storage contract

Supply an explicit Linux image **digest**, for the node architecture, with these executable paths:

* `/opt/fabric/fabric-remote-agent`, built from the same compatible reviewed source, including Linux PID1 `--init` and explicit initial HOME creation;
* `/opt/fabric/fabric` and `/opt/fabric/fabric-fake-harness` for provider-free proof and optional native setup; put them on `/usr/local/bin` through ordinary image installation if desired;
* `/bin/sh`, `/bin/sleep`, `/usr/bin/git`, and ordinary POSIX tools (`stty`, `cat`, `cut`, `head`, `rm`, `test`). Linux procfs and Unix PTYs must be available. The fake harness must be a sibling of the agent unless its explicit registry configuration selects another reviewed binary.

Build/publish/review this image separately; this plugin neither builds images nor downloads tools. For an authorized native harness, include its supported CLI/runtime and reviewed Fabric adapter, or install/configure it manually inside the Pod. Do not copy controller credentials, SSH agents, kubeconfig or user mounts into the image/Pod.

The sole pinned Pod command is exactly `["/opt/fabric/fabric-remote-agent","--init"]`, with no arguments, fallback or assumed image init. The already-required agent acts as Linux PID1, repeatedly reaping adopted exited ordinary children; TERM/INT/HUP exits without cross-session signaling. Changed command/image/mount/isolation fails closed. It does not replace owned worker/session cancellation or contain hostile detached code. The external package pins `target.py` and launcher; the separately reviewed image digest binds this generic binary. Image build/digest validation and actual PID1 adoption/reaping proof belong to manager QA, not source-implementation admission.

The Pod uses explicit positive UID/GID, `runAsNonRoot`, `fsGroup`, `OnRootMismatch`, RuntimeDefault seccomp, dropped capabilities, read-only root filesystem, no privilege escalation, no privileged container, no hostPath/hostNetwork/hostPID/hostIPC, no init/ephemeral containers and no service-account token. `/home/fabric` is a single ReadWriteOnce PVC; `/work` and `/tmp` are separate bounded emptyDir volumes. The agent creates private owner-only child directories; the storage driver must support the requested fsGroup/access semantics. It fails closed instead of chmodding/adopting foreign HOME contents.

Persistent HOME is `/home/fabric/fabric/environments/ENV/tree`; HOME, XDG roots and their private `.local` parent and native configuration/consent live there. Each run has fresh `/work/fabric/RUN` source/tree/log state. Ordinary run/session cleanup never deletes HOME or PVC. Pod recreation with the same proven PVC retains HOME but loses disposable emptyDir data. Loss of a Pod during unfinished capture is not successful artifact recovery.

First agent startup claims `fabric.sh/home-state:new` on the proven PVC with resourceVersion-conditional replacement, persists `initializing` before explicit `--initialize-home`, then `ready` after verified agent identity. No automatic retry: lost/failed claims stay refused for owner inspection. All subsequent startup/inspect/ensure/probe/plan/PTY/worker/destroy validation is non-creating. A private complete owner record binds environment, container, HOME and every XDG root by device/inode/birth time, independent of Pod UID and disposable workspace. Missing/replaced/symlinked/unsafe roots, old records or unavailable identity evidence fail closed without reading credentials. A new Pod on the same independently verified storage uses existing roots only. Filesystems without birth time or remounts/device changes that cannot preserve/prove identities refuse continuity; this is not a portable storage identity or backup guarantee.

`storage_size`, `workspace_size` and `memory` are explicit positive `Mi` quantities; CPU uses positive millicores. Requests/limits and emptyDir limits are Kubernetes best-effort enforcement, not an application sandbox/hard dollar or aggregate disk cap. Core's existing source/log/artifact/storage bounds also apply. HOME usage is persistent and owner-managed; do not fill the PVC with disposable source. Provision enough node ephemeral storage for transfer/archive copies as well as the expanded tree.

Rancher local-path is node-local: `WaitForFirstConsumer` may keep a PVC Pending until a consumer schedules; reclaim policy `Delete` may irreversibly remove its backing directory when the PVC is explicitly destroyed. This is not HA, backup, cloud durability or multi-node portability. Review the actual storage class and CSI/fsGroup/reclaim behavior; bootstrap/provisioner permissions are separate from the normal Fabric identity.

The Pod never requests host networking. Network egress is governed by owner-installed CNI/NetworkPolicy, not by Core. Permit only separately authorized DNS/provider/package endpoints; provider-free proof needs no model endpoint. Namespace quotas/admission/image policy are owner responsibilities. Admission may add harmless Pod defaults, but injected containers/mounts or relaxed security are rejected.

## Least-privilege bootstrap

A namespace administrator provisions the namespace, labels it `fabric.sh/installation: OWNER_INSTALLATION`, configures restricted admission/storage/network policy, and grants the owner identity only the following namespaced Role. Substitute a dedicated namespace and identity; do not apply these examples against manager-owned fixtures.

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: fabric-target
  namespace: OWNER_NAMESPACE
rules:
- apiGroups: [""]
  resources: [pods]
  verbs: [get, create, delete]
- apiGroups: [""]
  resources: [persistentvolumeclaims]
  verbs: [get, create, update, delete]
- apiGroups: [""]
  resources: [pods/exec]
  verbs: [get, create]
```

Bind this Role only to the kubeconfig's intended user/group via a namespaced RoleBinding. Modern kubectl transports can use GET or POST for exec; both verbs are explicit. Only PVC `update` is required for the one-time resourceVersion-conditional HOME claim; no list/watch/patch, Pod update, secrets, deployments, jobs, node, PV or cluster-admin access is required by normal operation. The plugin addresses individual deterministic resource names and uses API-server UID preconditions for deletion, never namespace sweeping.

Namespace identity checking needs one separately bootstrapped ClusterRole/ClusterRoleBinding restricted to `get` on **that exact namespace name**:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: fabric-owner-namespace-read
rules:
- apiGroups: [""]
  resources: [namespaces]
  resourceNames: [OWNER_NAMESPACE]
  verbs: [get]
```

The bootstrap administrator creates that binding; the normal user cannot grant itself permissions. Standard authenticated API discovery permission may also be needed by kubectl. Restrict its authentication identity to this namespace and avoid kubeconfigs with unrelated credentials. Cluster administrators/provisioners still have broader authority; Fabric does not claim to constrain them. Namespace deletion and any provisioner leftovers are explicit administrator actions outside ordinary cleanup.

## Explicit private configuration and installation

Create a canonical absolute, owner-only regular config file (0600, one link) outside source trees. Kubeconfig must also be a canonical owner-private regular file; never paste it into this document. Supply every key; placeholders below are **not usable defaults**:

```json
{
  "kubectl": "/canonical/private/tools/kubectl",
  "kubectl_sha256": "REVIEWED_KUBECTL_SHA256",
  "kubeconfig": "/canonical/private/kubeconfig",
  "kubeconfig_sha256": "REVIEWED_KUBECONFIG_SHA256",
  "context": "EXPLICIT_OWNER_CONTEXT",
  "namespace": "owner-fabric-proof",
  "installation": "owner-proof-installation",
  "image": "owner.example/fabric-linux@sha256:REVIEWED_IMAGE_DIGEST",
  "uid": 1000,
  "gid": 1000,
  "storage_class": "local-path",
  "storage_size": "256Mi",
  "workspace_size": "512Mi",
  "memory": "256Mi",
  "cpu": "250m",
  "scheduling_timeout_seconds": 30
}
```

Namespace/storage-class names are bounded DNS labels (2–63 characters); installation/environment/owner identifiers are bounded ASCII identifiers. The target deterministically names each Pod/PVC from installation + owner + environment, and records installation/config/owner/environment annotations. Namespace UID, PVC UID and configuration digest become an immutable opaque resource binding. Resource IDs are **not host paths or deletion capabilities**. Initial provisioning persists the PVC UID before readiness waiting, so a scheduling failure can still be explicitly destroyed. Missing PVC, changed namespace/PVC/config/target pin, foreign annotations/security, absent tools or an unavailable API fail closed. Inspect may recreate only a missing owned Pod with the same verified PVC; it does not recreate/adopt a missing saved PVC.

After reviewing this source/config/runtime, generate a new private package. These are proposed operator commands, not worker-executed commands:

```sh
node plugins/kubernetes/package.mjs /canonical/private/packages/kubernetes \
  /canonical/path/to/python3 /canonical/private/kubernetes.json
# Review the generated content and printed content-tree SHA256 before trust:
node sdk/typescript/cli.mjs install /canonical/private/packages/kubernetes \
  --registry /canonical/private/fabric-plugins --fabric /canonical/path/to/fabric \
  --version 0.1.0 --sha256 REVIEWED_PACKAGE_CONTENT_SHA256 --trust
export FABRIC_PLUGIN_REGISTRY=/canonical/private/fabric-plugins
fabric --root /canonical/private/fabric-root environments ensure proof-env \
  --owner proof-owner --target kubernetes --interactive-exec-pty --json
fabric --root /canonical/private/fabric-root shell proof-env \
  --owner proof-owner --target kubernetes
```

Packaging copies only the implementation and a launcher containing explicit Python/config path + config digest, not kubeconfig/config bytes. It uses the existing SDK content hash and installer; the target is **not installed by default**. No dependency resolution, automatic trust or remote publication. Config changes require a newly reviewed package pin. Do not upgrade an active binding to a different config: drain runs/shells, explicitly destroy old environments with their original pinned package/config, then install the new package and create new environments. Core refuses changed target pins; there is no compatibility alias or automatic adoption.

Inside the actual Pod shell, configure/install native software with its native commands only when separately authorized. For an installed native Fabric adapter, explicitly use the Pod registry:

```sh
export FABRIC_PLUGIN_REGISTRY="$HOME/.fabric/plugins"
# Install the separately reviewed adapter using the public SDK inside the Pod.
# Then, if separately authorized:
/opt/fabric/fabric init --harness omp
```

The Pod uses its own private HOME/consent, not controller native onboarding or credential files. Preserve native provider/model/effort/auth defaults. Real OAuth requires separate opt-in authorization and manual native device-code/browser login with a test account; never automate credential transfer. A private non-secret marker/fake auth file demonstrates persistence only, never OAuth validity. No OAuth is attempted by the acceptance script.

PTY bytes/control/resize flow through kubectl exec's binary NDJSON channel to an **actual in-Pod PTY**; kubectl itself must not use `-t`, which would corrupt the control channel. There is no host-local PTY bypass or shell transcript/default artifact capture. Terminal output can still be seen by the human/terminal; never paste it into diagnostics. Closing, cancellation or transport loss cancels and reaps ordinary owned process groups, with a 10-second remote heartbeat deadline if transport EOF is lost. Deliberately detached same-UID processes and abrupt node loss remain trusted-local best-effort ceilings, not containment guarantees.

## Runs, failures and cleanup

Use normal `fabric run` with explicit `--harness fake --placement kubernetes --environment proof-env --environment-owner proof-owner --interactive-exec-pty`. Configure explicit source/storage budgets appropriate to the small test PVC. No target failure silently executes the harness on the controller.

The controller uploads a bounded traversal/link-safe archive and verifies observed portable source/prompt hashes before launch. The prelaunch receipt records actual Linux/architecture/hostname/Pod UID plus the exact target/environment/workspace binding; it invents no remote inode/device witness. Actual remote tree/log bytes return through bounded size/SHA-verified transfer and existing artifact sealing. Ordinary cancellation waits for actual owned group termination before collecting/reclaiming disposable storage. Concurrent worker/shell shared HOME leases exclude explicit destruction, including independent controller processes via the remote lease.

After a controller crash between remote result sync and local capture, recovery validates the complete original receipt and positive remote inactivity, downloads retained tree/log bytes with bounded digest verification, then seals them without rerunning. Durable completion includes wait status, stop reason, harness-failure state and capture metadata; changed completion refuses even when capture is already recorded or artifacts await database association. Older records are checked against all existing durable wait/attempt/capture state. Inspect stages privately and removes staging without changing durable local/remote evidence.

Before any local output rename, apply commits an immutable installation receipt containing the private staging and original/returned filesystem identities, original/returned tree digests and the durable completion binding. A restart can finish the exact recorded original-tree gap, returned-tree/log prefix, or completed installation awaiting capture metadata. Matching bytes or directory names alone never authorize adopting partial/foreign output; changed identities, bytes, missing or duplicate objects retain evidence. The original tree and verified staging remain until authorized workspace cleanup. This is trusted-local process-crash continuity, not protection against a hostile same-UID actor or filesystem identity reuse; inode/device identities use creation time when the filesystem provides it.

Apply records actual wait/capture metadata and refreshes the run before capture. Target-supplied malformed archives, unsafe/duplicate paths, special links and missing/wrong tree/log types are `target_protocol`, preserving decoder/validation detail. Local file I/O, store or artifact-cap/capture failure after verified transfer remains local `artifact_capture`, not a target failure.

Kubernetes API scheduling/termination status is separate from harness status. `Unschedulable` waits only the explicit provisioning budget. `OOMKilled` is reported only when an actual terminated environment container has that exact structured reason; exit 137 or arbitrary harness stderr does not prove OOM. Pod identity evidence is bounded and omits raw Kubernetes messages, credentials and harness stderr. Failure before durable capture or changed/lost resources retains unknown state for inspection rather than inventing successful recovery. No automatic retry/replay or healthy-run global timeout was added; issue #76 wire budgets and cancellation/reaping remain.

Explicit irreversible destruction requires the exact environment confirmation, immutable binding, proven owned resources and no active run/shell:

```sh
fabric --root /canonical/private/fabric-root environments destroy proof-env \
  --owner proof-owner --target kubernetes --confirm proof-env --json
```

Only that environment's proven Pod/PVC are deleted with UID preconditions; the namespace is never deleted. Normal shell/task cleanup and Pod-only recreation retain HOME/PVC. Failed partial provisioning remains explicitly destroyable once ownership is persisted. If PVC creation succeeds but its reply is lost before Core persists the UID binding, the resource is retained for owner inspection: a new unbound request never adopts an existing claim, even with matching annotations. If API ownership or original pins cannot be verified, stop: restore the original reviewed configuration/target or have the owner inspect the retained resources. Do not sweep or relabel foreign resources.

Confirmed-owned Pending failed starts permit absent statuses or exclusively valid waiting/terminated container states (including ImagePullBackOff/CreateContainerConfigError); malformed/unknown/mixed/running states, including `running:{}`, refuse. The phase itself grants no lease: Core run/shell exclusion, namespace/PVC/config/owner identity, exact Pod security and absence of owned exec streams still apply. Failed-start Pod deletion requires both the original UID and the resourceVersion of that exact validated status snapshot; a scheduling/status race conflicts and retains the PVC. Running environments require the existing exclusive agent destroy proof. Deletion is original Pod first, then original PVC UID, using API-server preconditions; replacements are never adopted.

## Manager-owned narrow QA (not run by worker)

Provider-free deterministic checks; no cluster/provider emulation is represented as E2E:

```sh
node --test sdk/typescript/test/remote.test.mjs
python3 -m unittest discover -s plugins/kubernetes -p test_target.py
cargo test --locked -p fabric-core remote::
cargo test --locked -p fabric-core remote_agent::tests
cargo test --locked -p fabric-cli --bin fabric-remote-agent
# Build the required CLI/reference sibling binaries once before CLI integration tests:
cargo build --locked -p fabric-cli -p fabric-reference-harnesses --bins
cargo test --locked -p fabric-cli --test run remote_recovery::
```

Also run existing Runner target/plugin and CLI environment/shell ownership tests after building the required CLI/reference siblings once. The sealed worker summary lists exact proposed commands. The Python tests inject API status objects solely to test normalization and reject security/ownership/config boundaries before any kubectl execution. Real OOM remains **NOT_VERIFIED** unless an authorized owner separately obtains reliable actual container termination evidence; a memory-limit kill is not universally reproducible.

The explicitly opt-in real scenario uses installed target + actual CLI/Core/Pod exec + PTY + provider-free fake workers. Supply two separately reviewed private configs: the normal one above, and one for the **same owned namespace/installation/image** with an owner-established unschedulable CPU request (for example `9999m` only if known impossible on that cluster), and a short scheduling budget (for example 10 seconds). It destroys the first environment before explicitly upgrading to the second pin. Its work directory must not exist and must be outside Git checkouts. Do not use an existing manager fixture namespace/resources.

```sh
/canonical/path/to/python3 plugins/kubernetes/e2e.py --allow-kubernetes \
  --fabric /canonical/path/to/fabric --node /canonical/path/to/node \
  --config /canonical/private/kubernetes.json \
  --scheduling-config /canonical/private/kubernetes-unschedulable.json \
  --work /canonical/private/NEW-issue33-proof
```

The script exercises real shell binary bytes/control/resize/exit/cancel/disconnect, rejects active destruction, writes a non-secret private HOME marker, executes a first fake worker, UID-deletes only its owned Pod, recreates it with the same PVC, executes a second worker observing identical marker bytes and a different actual Pod identity, verifies all returned artifact sizes/SHA256 and remote workspace disappearance, cancels an actual parent/child worker group using observed PID/start-time witnesses, and explicitly destroys owned Pod/PVC. It checks bounded real scheduling failure with the second config and leaves the namespace untouched. It never invokes a model/provider/OAuth flow. Success output is emitted only after actual assertions; failures retain exact owned resources for inspection/explicit confirmed destruction, never guessed cleanup. Private fixture files/output must not be uploaded as public artifacts.

Process success requires the original observed PID/start-time identity to disappear, not merely become `Z`; matching zombies fail while present. `/proc/PID/stat` parsing handles spaces/parentheses in comm, rejects malformed/unreadable evidence and treats only a changed start time as a replacement (never signaled/adopted). Ordinary descendants are observed before terminal cancel, disconnect and worker cancel. Two explicit ordinary orphan batches demonstrate actual adoption by PID1 and eventual disappearance, with finite waits and immediate terminal-error checks. These scenarios remain manager-only, not a unit fixture or universal containment proof.

**Worker source-only delivery: NOT_RUN_BY_WORKER. Its source handoff alone established no Kubernetes E2E, real OOM, native OAuth or release/runtime compatibility result; separate manager observations follow.**

## Manager-observed private Kubernetes proof

On 2026-11-24, the actual opt-in `e2e.py --allow-kubernetes` scenario passed in 47.62 seconds on the private Colima/VZ arm64, kubectl v1.35.10, k3s v1.35.10+k3s1, local-path fixture described above. It used a newly owned restricted namespace and installation, not existing environment resources.

- `persistence`, `pty`, `cancel_reclaim`, and `scheduling`: **VERIFIED**. The scenario includes actual CLI → installed SDK package → external target → Linux agent execution, same-PVC Pod recreation, two ordinary orphan batches adopted/reaped by PID1, terminal resize/control-C/disconnect, cancellation and owned disposable-workspace reclamation.
- Exact image source: `f818d0595f1a2299fc1ba66c26e0c8c0a2b2df51`; Linux arm64 image manifest: `sha256:ef505a0168e35091ebb2644dbb4ef5f50d392b4bfbcf4b88f9fff9b9daa83dde`. It was built and registered only in the private fixture's external containerd `k8s.io` namespace, not published as a supported registry image. Subsequent source formatting does not change this image's provenance.
- After the scenario exited successfully, a separate namespaced installation-label query found zero owned Pods/PVCs. The owned namespace was retained; no unrelated resources or namespaces were deleted.
- All six actual CLI/installed-target remote recovery regressions and four core transfer regressions passed. These include immutable returned-tree preservation after capture commit and before manifest association, with no worker rerun.
- Real cgroup OOM remains **NOT_VERIFIED**; injectable terminated-status normalization is not real OOM evidence. Native OAuth remains **NOT_AUTHORIZED**. No paid model execution, multi-node/high-availability storage, hostile-code sandbox, or full consumer parity is claimed.


The manager repeated the installed Kubernetes scenario after isolating `kubectl`
discovery state in a private temporary client HOME: all 15 Python boundary checks
passed, then the real scenario passed in 45.57 seconds with the same explicit OOM
and OAuth limits above. The client HOME is removed only after its owned agents
and subprocesses stop; discovery caches do not enter the caller's workspace.
