#!/usr/bin/env python3
"""Explicitly installed Kubernetes target. Stdlib only; stdout is protocol only."""
import argparse
import hashlib
import http.client
import json
import os
import pathlib
import re
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid

MAX_FRAME = 65536
OPERATIONS = ["prepare", "start", "inspect", "cancel", "collect", "reclaim"]
OPTIONAL = ["environment.v1", "pty.v1", "remote.v1"]
CAPABILITIES = dict(version=1, persistent_home=True, interactive_exec_pty=True,
                    infrastructure_diagnostics=True,
                    operations=["ensure", "inspect", "destroy", "workspace"])
REMOTE_OPERATIONS = {"environment.ensure", "environment.inspect", "environment.destroy",
                     "harness.probe", "harness.plan", "pty.open", "pty.exchange", "pty.close",
                     "workspace.create", "source.write", "source.seal", "worker.start",
                     "worker.poll", "worker.cancel", "workspace.export", "workspace.read",
                     "workspace.reclaim"}


class Failure(Exception):
    def __init__(self, scope="target", kind="output", evidence=None):
        self.scope, self.kind, self.evidence = scope, kind, evidence


def require(ok):
    if not ok:
        raise Failure()


def encoded(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=True).encode()


def identifier(value):
    require(isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_-]{1,128}", value))
    return value


def private_bytes(path, maximum):
    require(isinstance(path, str) and os.path.isabs(path))
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        meta = os.fstat(fd)
        require(stat.S_ISREG(meta.st_mode) and meta.st_uid == os.geteuid()
                and meta.st_nlink == 1 and not meta.st_mode & 0o077 and meta.st_size <= maximum)
        with os.fdopen(fd, "rb", closefd=False) as source:
            data = source.read(maximum + 1)
        require(len(data) <= maximum)
        return data
    finally:
        os.close(fd)


def config(path, digest):
    raw = private_bytes(path, MAX_FRAME)
    require(hashlib.sha256(raw).hexdigest() == digest)
    value = json.loads(raw)
    require(set(value) == {"kubectl", "kubectl_sha256", "kubeconfig", "kubeconfig_sha256",
                           "context", "namespace", "installation", "image", "uid", "gid",
                           "storage_class", "storage_size", "workspace_size", "memory", "cpu",
                           "scheduling_timeout_seconds"})
    for key in ("namespace", "storage_class"):
        require(isinstance(value[key], str) and re.fullmatch(r"[a-z][a-z0-9-]{0,61}[a-z0-9]", value[key]))
    identifier(value["installation"])
    require(isinstance(value["context"], str) and 0 < len(value["context"]) <= 253
            and not any(ord(c) < 33 for c in value["context"]))
    require(isinstance(value["image"], str) and re.fullmatch(r"[A-Za-z0-9./:_-]+@sha256:[0-9a-f]{64}", value["image"]))
    for key in ("uid", "gid"):
        require(type(value[key]) is int and 1 <= value[key] <= 2147483647)
    for key in ("storage_size", "workspace_size", "memory"):
        require(isinstance(value[key], str) and re.fullmatch(r"[1-9][0-9]{0,3}Mi", value[key]))
    require(isinstance(value["cpu"], str) and re.fullmatch(r"[1-9][0-9]{0,3}m", value["cpu"]))
    require(type(value["scheduling_timeout_seconds"]) is int and 1 <= value["scheduling_timeout_seconds"] <= 600)
    require(hashlib.sha256(private_bytes(value["kubeconfig"], 1048576)).hexdigest() == value["kubeconfig_sha256"])
    binary = pathlib.Path(value["kubectl"])
    require(binary.is_absolute() and not binary.is_symlink())
    with binary.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    require(actual == value["kubectl_sha256"] and os.access(binary, os.X_OK))
    return value


def status_evidence(pod):
    """Only structured observed API status; never classify harness stderr."""
    meta, status = pod.get("metadata", {}), pod.get("status", {})
    evidence = {"instance": meta.get("uid"), "phase": status.get("phase")}
    for item in status.get("containerStatuses", []):
        if item.get("name") != "environment":
            continue
        terminated = item.get("state", {}).get("terminated")
        if terminated:
            reason = terminated.get("reason")
            evidence.update(reason="OOMKilled" if reason == "OOMKilled" else "ContainerTerminated",
                            exit_code=terminated.get("exitCode"), signal=terminated.get("signal"))
            return evidence
    for condition in status.get("conditions", []):
        if condition.get("type") == "PodScheduled" and condition.get("status") == "False":
            evidence["reason"] = "Unschedulable" if condition.get("reason") == "Unschedulable" else "SchedulingFailed"
            return evidence
    if status.get("phase") in ("Failed", "Succeeded"):
        evidence["reason"] = "PodTerminated"
    return evidence

def quantity(value):
    """Exact integer quantities; Kubernetes may canonicalize 1024Mi to 1Gi."""
    match = re.fullmatch(r"([0-9]+)(m|Ki|Mi|Gi|Ti)?", str(value))
    require(match is not None)
    number, suffix = match.groups()
    return int(number) * {None: 1000, "m": 1, "Ki": 1024_000,
                          "Mi": 1048576_000, "Gi": 1073741824_000, "Ti": 1099511627776_000}[suffix]


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=10)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


class Agent:
    def __init__(self, target, pod, binding, initialize=False):
        self.target, self.binding, self.instance = target, binding, pod["metadata"]["uid"]
        self.lock, self.closed, self.next_id = threading.Lock(), threading.Event(), 0
        argv = ["exec", "-i", pod["metadata"]["name"], "-c", "environment", "--",
                "/opt/fabric/fabric-remote-agent", "/home/fabric/fabric", "/work/fabric"]
        if initialize:
            argv.append("--initialize-home")
        self.process = target.spawn(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.process.stdin.write(encoded(binding) + b"\n")
        self.process.stdin.flush()
        self.heartbeat = threading.Thread(target=self.keepalive, daemon=True)
        self.heartbeat.start()

    def call(self, operation, value):
        with self.lock:
            require(not self.closed.is_set() and self.process.poll() is None)
            self.next_id += 1
            message = encoded(dict(id=self.next_id, operation=operation, input=value)) + b"\n"
            require(len(message) <= MAX_FRAME)
            self.process.stdin.write(message)
            self.process.stdin.flush()
            line = self.process.stdout.readline(MAX_FRAME + 1)
            require(0 < len(line) <= MAX_FRAME and line.endswith(b"\n"))
            reply = json.loads(line)
            require(reply.get("id") == self.next_id)
            if "error" in reply:
                kind = reply["error"].get("kind")
                require(isinstance(kind, str) and len(kind) <= 64)
                raise Failure("harness" if operation.startswith("harness.") or operation.startswith("worker.") else "target", kind)
            require(set(reply) == {"id", "result"})
            return reply["result"]

    def keepalive(self):
        while not self.closed.wait(2):
            if self.lock.acquire(blocking=False):
                self.lock.release()
                try:
                    self.call("heartbeat", {})
                except Exception:
                    self.close()
                    return

    def close(self):
        if self.closed.is_set():
            return
        self.closed.set()
        # Closing stdin delivers EOF to the in-Pod supervisor: cancel, TERM/KILL, reap.
        try:
            self.process.stdin.close()
        except (BrokenPipeError, OSError):
            pass
        try:
            self.process.wait(timeout=0.6)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.process.stdout.close()
        self.target.forget(self.process)


class Target:
    def __init__(self, settings, digest):
        self.c, self.digest = settings, digest
        self.agents, self.children = {}, set()
        self.children_lock = threading.Lock()
        self.stopping = threading.Event()

    def command(self, args):
        return [self.c["kubectl"], "--kubeconfig", self.c["kubeconfig"], "--context", self.c["context"],
                "--namespace", self.c["namespace"], "--request-timeout=10s", *args]

    def spawn(self, args, **kwargs):
        require(not self.stopping.is_set())
        process = subprocess.Popen(self.command(args), stderr=subprocess.DEVNULL,
                                   env={"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "LANG": "C"}, **kwargs)
        with self.children_lock:
            self.children.add(process)
        return process

    def forget(self, process):
        with self.children_lock:
            self.children.discard(process)

    def kubectl(self, args, value=None):
        process = self.spawn(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        deadline = threading.Timer(15, lambda: process.kill() if process.poll() is None else None)
        deadline.start()
        try:
            # API object output is bounded before buffering; no diagnostics are retained.
            data = encoded(value) if value is not None else b""
            require(len(data) <= MAX_FRAME)
            process.stdin.write(data)
            process.stdin.close()
            output = process.stdout.read(1048577)
            require(len(output) <= 1048576)
            require(process.wait(timeout=15) == 0)
            return json.loads(output) if output else None
        finally:
            deadline.cancel()
            if process.poll() is None:
                process.kill()
            process.wait()
            process.stdout.close()
            self.forget(process)

    def get(self, kind, name):
        return self.kubectl(["get", kind, name, "--ignore-not-found", "-o", "json"])

    def namespace(self):
        namespace = self.get("namespace", self.c["namespace"])
        require(namespace and namespace["metadata"].get("labels", {}).get("fabric.sh/installation") == self.c["installation"]
                and not namespace["metadata"].get("deletionTimestamp"))
        return namespace["metadata"]["uid"]

    def names(self, environment):
        key = hashlib.sha256(encoded([self.c["installation"], identifier(environment["owner"]), identifier(environment["environment_id"])] )).hexdigest()[:32]
        return "fabric-" + key

    def metadata(self, name, environment):
        return dict(name=name, namespace=self.c["namespace"],
                    labels={"fabric.sh/installation": self.c["installation"]},
                    annotations={"fabric.sh/config": self.digest,
                                 "fabric.sh/environment": environment["environment_id"],
                                 "fabric.sh/owner": environment["owner"]})

    def owned(self, obj, environment, name):
        require(obj and obj["metadata"].get("name") == name and obj["metadata"].get("namespace") == self.c["namespace"]
                and not obj["metadata"].get("deletionTimestamp"))
        expected = self.metadata(name, environment)
        for category in ("labels", "annotations"):
            require(all(obj["metadata"].get(category, {}).get(k) == v for k, v in expected[category].items()))
        return obj["metadata"]["uid"]

    def pod_spec(self, name, environment):
        return dict(apiVersion="v1", kind="Pod", metadata=self.metadata(name, environment), spec={
            "restartPolicy": "Never", "automountServiceAccountToken": False,
            "enableServiceLinks": False, "hostNetwork": False, "hostPID": False, "hostIPC": False,
            "securityContext": {"runAsNonRoot": True, "runAsUser": self.c["uid"], "runAsGroup": self.c["gid"],
                                "fsGroup": self.c["gid"], "fsGroupChangePolicy": "OnRootMismatch",
                                "seccompProfile": {"type": "RuntimeDefault"}},
            "containers": [{"name": "environment", "image": self.c["image"], "imagePullPolicy": "IfNotPresent",
                            "command": ["/opt/fabric/fabric-remote-agent", "--init"],
                            "env": [{"name": "FABRIC_TARGET_INSTANCE", "valueFrom": {"fieldRef": {"apiVersion": "v1", "fieldPath": "metadata.uid"}}},
                                    {"name": "FABRIC_PLUGIN_REGISTRY", "value": "/home/fabric/fabric/environments/" + identifier(environment["environment_id"]) + "/tree/.fabric/plugins"}],
                            "securityContext": {"allowPrivilegeEscalation": False, "privileged": False,
                                                "readOnlyRootFilesystem": True, "capabilities": {"drop": ["ALL"]}},
                            "resources": {"requests": {"cpu": self.c["cpu"], "memory": self.c["memory"]},
                                          "limits": {"cpu": self.c["cpu"], "memory": self.c["memory"], "ephemeral-storage": self.c["workspace_size"]}},
                            "volumeMounts": [{"name": "home", "mountPath": "/home/fabric"},
                                             {"name": "work", "mountPath": "/work"}, {"name": "tmp", "mountPath": "/tmp"}]}],
            "volumes": [{"name": "home", "persistentVolumeClaim": {"claimName": name}},
                        {"name": "work", "emptyDir": {"sizeLimit": self.c["workspace_size"]}},
                        {"name": "tmp", "emptyDir": {"sizeLimit": "64Mi"}}]})

    def validate_pod(self, pod, name, environment):
        self.owned(pod, environment, name)
        expected = self.pod_spec(name, environment)["spec"]
        actual = pod["spec"]
        # Admission may add defaults, never relax the requested isolation or add mounts.
        for key in ("restartPolicy", "automountServiceAccountToken", "enableServiceLinks", "hostNetwork", "hostPID", "hostIPC"):
            require(actual.get(key, False) == expected[key])
        require(len(actual["containers"]) == 1 and not actual.get("initContainers") and not actual.get("ephemeralContainers"))
        container = actual["containers"][0]
        require(set(container) <= set(expected["containers"][0]) | {"terminationMessagePath", "terminationMessagePolicy"})
        for key, value in expected["containers"][0].items():
            if key == "resources":
                for group in ("requests", "limits"):
                    for resource, amount in value[group].items():
                        require(quantity(container.get(key, {}).get(group, {}).get(resource)) == quantity(amount))
            else:
                require(container.get(key) == value)
        require(actual["securityContext"] == expected["securityContext"])
        require(len(actual["volumes"]) == len(expected["volumes"]))
        for observed, wanted in zip(actual["volumes"], expected["volumes"]):
            if "emptyDir" in wanted:
                require(observed.get("name") == wanted["name"] and set(observed) == set(wanted)
                        and set(observed["emptyDir"]) == {"sizeLimit"}
                        and quantity(observed["emptyDir"]["sizeLimit"]) == quantity(wanted["emptyDir"]["sizeLimit"]))
            else:
                require(observed == wanted)

    def environment(self, incoming, create=False):
        require(set(incoming) == {"environment_id", "owner", "resource"})
        name, ns_uid = self.names(incoming), self.namespace()
        saved = incoming["resource"]
        pvc = self.get("pvc", name)
        if saved is None:
            # No saved UID means no authority to adopt even matching annotations.
            # A lost create reply leaves an orphan for explicit owner inspection.
            require(create and pvc is None and self.get("pod", name) is None)
            metadata = self.metadata(name, incoming)
            metadata["annotations"]["fabric.sh/home-state"] = "new"
            pvc = self.kubectl(["create", "-f", "-", "-o", "json"], dict(apiVersion="v1", kind="PersistentVolumeClaim",
                metadata=metadata, spec={"accessModes": ["ReadWriteOnce"], "storageClassName": self.c["storage_class"],
                                       "resources": {"requests": {"storage": self.c["storage_size"]}}}))
        require(pvc is not None)
        pvc_uid = self.owned(pvc, incoming, name)
        require(pvc["spec"]["storageClassName"] == self.c["storage_class"] and pvc["spec"]["accessModes"] == ["ReadWriteOnce"]
                and quantity(pvc["spec"]["resources"]["requests"]["storage"]) == quantity(self.c["storage_size"]))
        resource = dict(backend="remote.v1", resource_id="k8s:" + ns_uid + ":" + pvc_uid,
                        namespace_uid=ns_uid, pvc_uid=pvc_uid, name=name, config_sha256=self.digest)
        require(saved is None or saved == resource)
        binding = dict(environment_id=incoming["environment_id"], owner=incoming["owner"], resource=resource)
        # Bind the API-server PVC UID before readiness can fail. Core persists
        # this identity before inspect waits for scheduling; destroy can then
        # clean an owned Pending Pod/PVC without adopting anything.
        if saved is None:
            return binding, None
        pod = self.get("pod", name)
        if pod is None:
            pod = self.kubectl(["create", "-f", "-", "-o", "json"], self.pod_spec(name, binding))
        self.validate_pod(pod, name, binding)
        instance = pod["metadata"]["uid"]
        deadline = time.monotonic() + self.c["scheduling_timeout_seconds"]
        while True:
            require(not self.stopping.is_set())
            pod = self.get("pod", name)
            require(pod and pod["metadata"]["uid"] == instance)
            self.validate_pod(pod, name, binding)
            evidence = status_evidence(pod)
            if "reason" in evidence and evidence["reason"] != "Unschedulable":
                raise Failure(evidence=evidence)
            pvc = self.get("pvc", name)
            require(pvc and self.owned(pvc, binding, name) == pvc_uid)
            if pod.get("status", {}).get("phase") == "Running" and pvc.get("status", {}).get("phase") == "Bound" and any(
                    c.get("type") == "Ready" and c.get("status") == "True" for c in pod.get("status", {}).get("conditions", [])):
                return binding, pod
            if time.monotonic() >= deadline:
                evidence.setdefault("reason", "SchedulingDeadline")
                raise Failure(evidence=evidence)
            self.stopping.wait(0.1)

    def agent(self, binding, pod):
        key = binding["resource"]["resource_id"]
        agent = self.agents.get(key)
        if agent is not None:
            require(agent.instance == pod["metadata"]["uid"] and agent.binding == binding)
        else:
            require(not self.agents)
            pvc = self.get("pvc", binding["resource"]["name"])
            require(pvc and self.owned(pvc, binding, binding["resource"]["name"]) == binding["resource"]["pvc_uid"])
            state = pvc["metadata"].get("annotations", {}).get("fabric.sh/home-state")
            require(state in ("new", "ready"))
            initialize = state == "new"
            if initialize:
                # resourceVersion makes this a single, non-retryable creation
                # claim. A lost/failing claim never grants restart adoption.
                require(pvc["metadata"].get("resourceVersion"))
                pvc["metadata"]["annotations"]["fabric.sh/home-state"] = "initializing"
                pvc = self.kubectl(["replace", "-f", "-", "-o", "json"], pvc)
                require(self.owned(pvc, binding, binding["resource"]["name"]) == binding["resource"]["pvc_uid"]
                        and pvc["metadata"]["annotations"]["fabric.sh/home-state"] == "initializing")
            agent = Agent(self, pod, binding, initialize=initialize)
            try:
                identity = agent.call("agent.identity", {})
                require(identity["instance"] == pod["metadata"]["uid"] and identity["os"] == "linux"
                        and identity["environment"] == binding)
                if initialize:
                    pvc["metadata"]["annotations"]["fabric.sh/home-state"] = "ready"
                    saved = self.kubectl(["replace", "-f", "-", "-o", "json"], pvc)
                    require(self.owned(saved, binding, binding["resource"]["name"]) == binding["resource"]["pvc_uid"]
                            and saved["metadata"]["annotations"]["fabric.sh/home-state"] == "ready")
                self.agents[key] = agent
            except Exception:
                agent.close()
                raise
        return agent

    def delete_uid(self, kind, name, uid, resource_version=None):
        """DELETE exact UID; failed-start deletion also fences observed Pod state."""
        plural = {"pod": "pods", "pvc": "persistentvolumeclaims"}[kind]
        path = "/api/v1/namespaces/" + self.c["namespace"] + "/" + plural + "/" + name
        preconditions = {"uid": uid}
        if resource_version is not None:
            require(isinstance(resource_version, str) and resource_version)
            preconditions["resourceVersion"] = resource_version
        with tempfile.TemporaryDirectory(prefix="fabric-k8s-") as directory:
            endpoint = os.path.join(directory, "api.sock")
            proxy = self.spawn(["proxy", "--unix-socket=" + endpoint, "--accept-hosts=^localhost$", "--accept-paths=^/api/v1/namespaces/" + self.c["namespace"] + "/"], stdout=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 5
                while not os.path.exists(endpoint):
                    require(proxy.poll() is None and time.monotonic() < deadline and not self.stopping.is_set())
                    time.sleep(0.01)
                connection = UnixHTTP(endpoint)
                try:
                    connection.request("DELETE", path, encoded({"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": preconditions}),
                                       {"Content-Type": "application/json", "Host": "localhost"})
                    response = connection.getresponse()
                    require(response.status in (200, 202, 404))
                    require(len(response.read(1048577)) <= 1048576)
                finally:
                    connection.close()
            finally:
                proxy.terminate()
                try:
                    proxy.wait(timeout=0.3)
                except subprocess.TimeoutExpired:
                    proxy.kill()
                    proxy.wait()
                self.forget(proxy)
        deadline = time.monotonic() + self.c["scheduling_timeout_seconds"]
        while True:
            observed = self.get(kind, name)
            if observed is None:
                return
            require(observed["metadata"]["uid"] == uid and time.monotonic() < deadline)
            self.stopping.wait(0.1)

    def destroy(self, binding):
        # Never create resources while destroying. Namespace/PVC identity must still match.
        require(self.namespace() == binding["resource"]["namespace_uid"])
        name = self.names(binding)
        require(binding["resource"]["name"] == name and binding["resource"]["config_sha256"] == self.digest)
        pvc, pod = self.get("pvc", name), self.get("pod", name)
        if pvc is None:
            require(pod is None)
            return {"absent": True}
        require(self.owned(pvc, binding, name) == binding["resource"]["pvc_uid"])
        if pod is not None:
            self.validate_pod(pod, name, binding)
            phase = pod.get("status", {}).get("phase")
            if phase in ("Failed", "Succeeded", "Pending"):
                statuses = pod.get("status", {}).get("containerStatuses", [])
                require(isinstance(statuses, list))
                # Presence, not truthiness: running:{} is an active container.
                for item in statuses:
                    require(isinstance(item, dict) and item.get("name") == "environment")
                    state = item.get("state")
                    require(isinstance(state, dict) and len(state) == 1
                            and set(state) <= {"waiting", "terminated"}
                            and all(isinstance(value, dict) for value in state.values()))
                    kind, detail = next(iter(state.items()))
                    if kind == "waiting":
                        require(set(detail) <= {"reason", "message"}
                                and all(isinstance(value, str) for value in detail.values()))
                    else:
                        require(set(detail) <= {"exitCode", "signal", "reason", "message", "startedAt", "finishedAt", "containerID"}
                                and type(detail.get("exitCode")) is int
                                and -(2 ** 31) <= detail["exitCode"] < 2 ** 31
                                and ("signal" not in detail or (type(detail["signal"]) is int and 0 <= detail["signal"] < 2 ** 31))
                                and all(isinstance(value, str) for key, value in detail.items() if key not in ("exitCode", "signal")))
                require(len(statuses) <= 1)
                # An already-owned exec stream precludes the failed-start shortcut.
                require(binding["resource"]["resource_id"] not in self.agents)
                version = pod["metadata"].get("resourceVersion")
                require(isinstance(version, str) and version)
                self.delete_uid("pod", name, pod["metadata"]["uid"], version)
            else:
                agent = self.agent(binding, pod)
                require(agent.call("environment.prepare_destroy", binding) == {"exclusive": True})
                self.delete_uid("pod", name, agent.instance)
                agent.close()
                self.agents.pop(binding["resource"]["resource_id"], None)
        self.delete_uid("pvc", name, binding["resource"]["pvc_uid"])
        return {"absent": True}

    def call(self, operation, value):
        require(operation in REMOTE_OPERATIONS and isinstance(value, dict))
        if operation == "environment.destroy":
            return self.destroy(value)
        if operation in ("environment.ensure", "environment.inspect"):
            binding, pod = self.environment(value, create=operation == "environment.ensure")
            if pod is None:
                return {"available": False, "resource": binding["resource"]}
            result = self.agent(binding, pod).call(operation, binding)
            require(result == {"available": True, "resource": binding["resource"]})
            return result
        if operation.startswith("pty.") and operation != "pty.open":
            require(len(self.agents) == 1)
            return next(iter(self.agents.values())).call(operation, value)
        binding = value.get("environment", value.get("resource", {}).get("environment"))
        require(isinstance(binding, dict))
        key = binding["resource"]["resource_id"]
        existing = self.agents.get(key)
        if existing is not None:
            require(existing.binding == binding)
            # The owned exec stream is tied to the actual Pod UID by agent.identity.
            # Do not repeat API calls per 8 KiB transfer chunk.
            try:
                return existing.call(operation, value)
            except Exception as failure:
                observed = self.get("pod", self.names(binding))
                require(observed and observed["metadata"]["uid"] == existing.instance)
                evidence = status_evidence(observed)
                if "reason" in evidence:
                    return {"state":"target_failed", "evidence":evidence}
                raise failure
        binding, pod = self.environment(binding)
        resource = value.get("resource")
        if resource is not None:
            require(resource.get("instance") == pod["metadata"]["uid"])
        agent = self.agent(binding, pod)
        try:
            return agent.call(operation, value)
        except Exception as failure:
            observed = self.get("pod", pod["metadata"]["name"])
            require(observed and observed["metadata"]["uid"] == agent.instance)
            evidence = status_evidence(observed)
            if "reason" in evidence:
                if operation in ("worker.poll", "worker.cancel"):
                    return {"state": "target_failed", "evidence": evidence}
                raise Failure(evidence=evidence) from None
            raise failure

    def close(self):
        self.stopping.set()
        for agent in list(self.agents.values()):
            agent.close()
        with self.children_lock:
            children = list(self.children)
        for child in children:
            if child.poll() is None:
                child.kill()
            child.wait()


def serve(target):
    next_id, ticket, result, thread = 1, None, None, None
    guard = threading.Lock()
    remote = False

    def invoke(operation, value):
        nonlocal result
        try:
            reply = {"state": "complete", "result": target.call(operation, value)}
        except Failure as failure:
            reply = {"state": "failed", "scope": failure.scope, "kind": failure.kind}
            if failure.evidence is not None:
                # Evidence is returned as data, not an unstructured error message.
                reply = {"state": "complete", "result": {"state": "target_failed", "evidence": failure.evidence}}
        except Exception:
            reply = {"state": "failed", "scope": "target", "kind": "output"}
        with guard:
            result = reply

    try:
        while True:
            line = sys.stdin.buffer.readline(MAX_FRAME + 1)
            if not line:
                return
            require(len(line) <= MAX_FRAME and line.endswith(b"\n"))
            request = json.loads(line)
            require(set(request) == {"jsonrpc", "id", "method", "params"} and request["jsonrpc"] == "2.0" and request["id"] == next_id)
            method, params = request["method"], request["params"]
            if next_id == 1:
                require(method == "initialize" and params == {"protocol": 1})
                reply = {"protocol": 1, "id": "kubernetes", "kind": "target", "version": "0.1.0"}
            elif next_id == 2:
                require(method == "health" and params == {})
                reply = {"ready": True}
            elif method == "target.capabilities":
                require(params == {})
                reply = {"placements": ["kubernetes"], "operations": OPERATIONS, "optional": OPTIONAL}
            elif method == "target.placement":
                require(params == {"placement": "kubernetes", "backends": ["remote.v1"]})
                remote, reply = True, {"backend": "remote.v1"}
            elif method == "target.environment.capabilities":
                require(remote and params == {"version": 1})
                reply = CAPABILITIES
            elif method == "target.remote.invoke":
                require(remote and ticket is None and set(params) == {"version", "operation", "input"}
                        and params["version"] == 1 and params["operation"] in REMOTE_OPERATIONS)
                ticket = uuid.uuid4().hex
                result = None
                thread = threading.Thread(target=invoke, args=(params["operation"], params["input"]))
                thread.start()
                reply = {"ticket": ticket}
            elif method == "target.remote.poll":
                require(ticket is not None and params == {"ticket": ticket})
                with guard:
                    reply = result or {"state": "pending"}
                    if result is not None:
                        ticket, result = None, None
            elif method == "shutdown":
                require(params == {} and ticket is None)
                target.close()
                reply = {"ready": True}
            else:
                raise Failure()
            data = encoded({"jsonrpc": "2.0", "id": next_id, "result": reply}) + b"\n"
            require(len(data) <= MAX_FRAME)
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
            next_id += 1
            if method == "shutdown":
                return
    finally:
        target.close()
        if thread is not None:
            thread.join()


def main():
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--config", required=True)
    parser.add_argument("--config-sha256", required=True)
    parser.add_argument("--fabric-plugin-stdio", action="store_true", required=True)
    args = parser.parse_args()
    require(re.fullmatch(r"[0-9a-f]{64}", args.config_sha256))
    serve(Target(config(args.config, args.config_sha256), args.config_sha256))


if __name__ == "__main__":
    try:
        main()
    except Exception:
        sys.stderr.write("Kubernetes target failed closed.\n")
        sys.exit(1)
