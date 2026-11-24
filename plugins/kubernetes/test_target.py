"""Provider-free boundary tests. No kubectl invocation or Kubernetes emulation."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("kubernetes_target", Path(__file__).with_name("target.py"))
plugin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(plugin)


class Boundaries(unittest.TestCase):
    def test_oom_requires_actual_terminated_status_not_arbitrary_message(self):
        pod = {"metadata": {"uid": "pod-1"}, "status": {"phase": "Running", "message": "OOMKilled secret",
               "containerStatuses": [{"name": "environment", "state": {"running": {}}}]}}
        self.assertNotIn("reason", plugin.status_evidence(pod))
        pod["status"]["containerStatuses"][0]["state"] = {"terminated": {"reason": "OOMKilled", "exitCode": 137, "signal": 9, "message": "private"}}
        self.assertEqual(plugin.status_evidence(pod), {"instance": "pod-1", "phase": "Running", "reason": "OOMKilled", "exit_code": 137, "signal": 9})
        pod["status"]["containerStatuses"][0]["state"]["terminated"]["reason"] = "Error OOMKilled"
        self.assertEqual(plugin.status_evidence(pod)["reason"], "ContainerTerminated")

    def test_scheduling_evidence_omits_private_messages(self):
        result = plugin.status_evidence({"metadata": {"uid": "p"}, "status": {"phase": "Pending", "conditions": [
            {"type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": "private node detail"}]}})
        self.assertEqual(result, {"instance": "p", "phase": "Pending", "reason": "Unschedulable"})

    def test_foreign_ownership_and_relaxed_security_are_rejected(self):
        settings = dict(namespace="bounded-test", installation="proof", uid=1000, gid=1000,
                        image="example.invalid/image@sha256:" + "1" * 64, cpu="100m", memory="128Mi", workspace_size="256Mi")
        target = plugin.Target(settings, "a" * 64)
        environment = dict(owner="owner", environment_id="test", resource=None)
        name = target.names(environment)
        pod = target.pod_spec(name, environment)
        pod["metadata"]["uid"] = "owned-uid"
        target.validate_pod(pod, name, environment)
        for mutate in (lambda p: p["metadata"]["annotations"].update({"fabric.sh/owner": "foreign"}),
                       lambda p: p["spec"]["containers"][0]["securityContext"].update({"privileged": True}),
                       lambda p: p["spec"]["volumes"].append({"name": "escape", "hostPath": {"path": "/"}}),
                       lambda p: p["spec"].update({"hostNetwork": True})):
            changed = copy.deepcopy(pod)
            mutate(changed)
            with self.assertRaises(plugin.Failure):
                target.validate_pod(changed, name, environment)

    def test_missing_or_changed_pvc_and_unbound_existing_claim_fail_closed(self):
        # Read-only API object injection, not Kubernetes execution or an E2E mock.
        settings = dict(namespace="bounded-test", installation="proof", storage_class="local-path", storage_size="256Mi")
        target = plugin.Target(settings, "a" * 64)
        target.namespace = lambda: "namespace-uid"
        environment = dict(owner="owner", environment_id="test", resource=None)
        name = target.names(environment)
        resource = dict(backend="remote.v1", resource_id="k8s:namespace-uid:pvc-original",
                        namespace_uid="namespace-uid", pvc_uid="pvc-original", name=name, config_sha256="a" * 64)
        saved = {**environment, "resource": resource}
        target.get = lambda kind, name: None
        with self.assertRaises(plugin.Failure):
            target.environment(saved)
        pvc = dict(metadata={**target.metadata(name, environment), "uid": "replacement"},
                   spec=dict(storageClassName="local-path", accessModes=["ReadWriteOnce"],
                             resources={"requests": {"storage": "256Mi"}}))
        target.get = lambda kind, name: pvc if kind == "pvc" else None
        with self.assertRaises(plugin.Failure):
            target.environment(saved)
        pvc["metadata"]["uid"] = "pvc-original"
        with self.assertRaises(plugin.Failure):
            target.environment(environment, create=True)

    def test_private_input_does_not_follow_links_or_accept_world_readable_files(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config"
            path.write_bytes(b"private")
            path.chmod(0o600)
            self.assertEqual(plugin.private_bytes(str(path), 7), b"private")
            with self.assertRaises(plugin.Failure):
                plugin.private_bytes(str(path), 6)
            path.chmod(0o644)
            with self.assertRaises(plugin.Failure):
                plugin.private_bytes(str(path), 7)
            link = Path(directory) / "link"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                plugin.private_bytes(str(link), 7)

    def test_invalid_namespace_and_missing_context_fail_before_platform_execution(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config"
            config = dict(kubectl="/absent", kubectl_sha256="0" * 64, kubeconfig="/absent", kubeconfig_sha256="0" * 64,
                          context="explicit", namespace="../foreign", installation="test", image="image@sha256:" + "0" * 64,
                          uid=1000, gid=1000, storage_class="local-path", storage_size="256Mi", workspace_size="256Mi",
                          memory="128Mi", cpu="100m", scheduling_timeout_seconds=5)
            for change in ({}, {"namespace": "bounded-test", "context": ""}, {"namespace": "bounded-test", "uid": 0}):
                value = {**config, **change}
                raw = json.dumps(value).encode()
                path.write_bytes(raw)
                path.chmod(0o600)
                with self.assertRaises(plugin.Failure):
                    plugin.config(str(path), hashlib.sha256(raw).hexdigest())


if __name__ == "__main__":
    unittest.main()
