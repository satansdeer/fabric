"""Provider-free boundary tests. No kubectl invocation or Kubernetes emulation."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location("kubernetes_target", Path(__file__).with_name("target.py"))
plugin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(plugin)
e2e_spec = importlib.util.spec_from_file_location("kubernetes_evidence", Path(__file__).with_name("e2e.py"))
evidence = importlib.util.module_from_spec(e2e_spec)
e2e_spec.loader.exec_module(evidence)


class Boundaries(unittest.TestCase):
    def test_original_identity_requires_disappearance_not_zombie(self):
        def proc(state=b"S", start=b"987"):
            return b"123 (comm with ) spaces (and)) " + b" ".join([state, b"1"] + [b"0"] * 17 + [start]) + b"\n"
        for state in (b"S", b"R", b"Z"):
            self.assertFalse(evidence.original_gone(proc(state), (123, 987)))
        self.assertTrue(evidence.original_gone(b"ABSENT\n", (123, 987)))
        self.assertTrue(evidence.original_gone(proc(start=b"988"), (123, 987)))
        for malformed in (b"", b"permission denied", b"123 (x) S", proc(start=b"bad")):
            with self.assertRaises(AssertionError):
                evidence.original_gone(malformed, (123, 987))
        proof = object.__new__(evidence.Proof)
        proof.exec_bytes = lambda *args: (_ for _ in ()).throw(AssertionError("unreadable"))
        with self.assertRaises(AssertionError):
            proof.process_witness(123)

    def destroy_fixture(self):
        settings = dict(namespace="bounded-test", installation="proof", uid=1000, gid=1000,
                        image="example.invalid/image@sha256:" + "1" * 64, cpu="100m", memory="128Mi", workspace_size="256Mi")
        target = plugin.Target(settings, "a" * 64)
        binding = dict(owner="owner", environment_id="test", resource=None)
        name = target.names(binding)
        binding["resource"] = dict(backend="remote.v1", resource_id="k8s:ns:pvc", namespace_uid="ns",
                                  pvc_uid="pvc", name=name, config_sha256=target.digest)
        pvc = dict(metadata={**target.metadata(name, binding), "uid": "pvc"})
        pod = target.pod_spec(name, binding)
        pod["metadata"].update(uid="pod", resourceVersion="1")
        pod["status"] = {"phase": "Pending"}
        objects, deleted = {"pvc": pvc, "pod": pod}, []
        target.namespace = lambda: "ns"
        target.get = lambda kind, observed: objects[kind]
        def delete(kind, observed, uid, resource_version=None):
            self.assertEqual(observed, name)
            self.assertEqual(uid, objects[kind]["metadata"]["uid"])
            if resource_version is not None:
                self.assertEqual(resource_version, objects[kind]["metadata"]["resourceVersion"])
            deleted.append((kind, uid))
            objects[kind] = None
        target.delete_uid = delete
        target.agent = lambda *args: (_ for _ in ()).throw(plugin.Failure())
        return target, binding, objects, deleted

    def test_failed_start_deletes_exact_owned_pod_before_pvc(self):
        for state in (None, {"waiting": {"reason": "ImagePullBackOff"}},
                      {"waiting": {"reason": "CreateContainerConfigError"}}, {"terminated": {"exitCode": 1}}):
            target, binding, objects, deleted = self.destroy_fixture()
            if state is not None:
                objects["pod"]["status"]["containerStatuses"] = [{"name": "environment", "state": state}]
            self.assertEqual(target.destroy(binding), {"absent": True})
            self.assertEqual(deleted, [("pod", "pod"), ("pvc", "pvc")])
            self.assertEqual(objects, {"pod": None, "pvc": None})

    def test_destroy_refuses_running_ambiguous_or_foreign_without_deletion(self):
        mutations = [(lambda value: lambda t,b,o: o["pod"]["status"].update(containerStatuses=value))(value)
                     for value in (None, {}, [None], [{"name": "environment", "state": {}}],
                                   [{"name": "environment", "state": {"running": {}}}],
                                   [{"name": "environment", "state": {"waiting": {}, "running": {}}}],
                                   [{"name": "environment", "state": {"unknown": {}}}],
                                   [{"name": "foreign", "state": {"waiting": {}}}],
                                   [{"name": "environment", "state": {"terminated": {}}}],
                                   [{"name": "environment", "state": {"terminated": {"exitCode": True}}}],
                                   [{"name": "environment", "state": {"waiting": {"reason": 3}}}],
                                   [{"name": "environment", "state": {"waiting": {"foreign": ""}}}],
                                   [{"name": "environment", "state": {"waiting": None}}])]
        for key in ("fabric.sh/owner", "fabric.sh/config"):
            mutations.append(lambda t,b,o,key=key: o["pod"]["metadata"]["annotations"].update({key:"foreign"}))
        mutations.extend([
            lambda t,b,o: o["pvc"]["metadata"]["labels"].update({"fabric.sh/installation":"foreign"}),
            lambda t,b,o: o["pvc"]["metadata"].update(uid="replacement"),
            lambda t,b,o: b["resource"].update(namespace_uid="replacement"),
            lambda t,b,o: o["pod"]["metadata"].pop("resourceVersion"),
            lambda t,b,o: o["pod"]["metadata"].update(resourceVersion=""),
            lambda t,b,o: o["pod"]["metadata"].update(resourceVersion=3),
            lambda t,b,o: o["pod"]["spec"]["containers"][0]["securityContext"].update(privileged=True),
            lambda t,b,o: o["pod"]["spec"]["containers"][0].update(command=["/bin/sleep","infinity"]),
            lambda t,b,o: o["pod"]["spec"]["containers"][0].update(image="foreign@sha256:"+"0"*64),
            lambda t,b,o: t.agents.update({b["resource"]["resource_id"]:object()}),
            lambda t,b,o: o["pod"]["status"].update(phase="Running"),
        ])
        for mutate in mutations:
            target, binding, objects, deleted = self.destroy_fixture()
            mutate(target, binding, objects)
            with self.assertRaises(plugin.Failure):
                target.destroy(binding)
            self.assertEqual(deleted, [])

    def test_running_destroy_requires_exclusive_agent_and_keeps_delete_order(self):
        for exclusive in (False, True):
            target, binding, objects, deleted = self.destroy_fixture()
            objects["pod"]["status"] = {"phase": "Running"}
            agent = Mock(instance="pod")
            agent.call.return_value = {"exclusive": exclusive}
            target.agent = lambda *args: agent
            if exclusive:
                self.assertEqual(target.destroy(binding), {"absent": True})
                self.assertEqual(deleted, [("pod", "pod"), ("pvc", "pvc")])
                agent.close.assert_called_once_with()
            else:
                with self.assertRaises(plugin.Failure):
                    target.destroy(binding)
                self.assertEqual(deleted, [])
                agent.close.assert_not_called()
            agent.call.assert_called_once_with("environment.prepare_destroy", binding)

    def test_api_deletion_fences_failed_start_state_and_rejects_replacements(self):
        for conflict in (None, "uid", "running"):
            target, binding, objects, _ = self.destroy_fixture()
            target.c["scheduling_timeout_seconds"] = 1
            calls = []
            def spawn(argv, **kwargs):
                endpoint = next(arg.split("=", 1)[1] for arg in argv if arg.startswith("--unix-socket="))
                Path(endpoint).touch()  # readiness only; no process or cluster
                proxy = Mock()
                proxy.poll.return_value = None
                return proxy
            target.spawn = spawn
            target.forget = lambda child: None
            def connection(endpoint):
                http = Mock()
                def request(method, path, body, headers):
                    kind = "pvc" if "/persistentvolumeclaims/" in path else "pod"
                    options = json.loads(body)
                    self.assertEqual(method, "DELETE")
                    self.assertEqual(path, "/api/v1/namespaces/bounded-test/" + ("pods/" if kind == "pod" else "persistentvolumeclaims/") + binding["resource"]["name"])
                    preconditions = {"uid": kind}
                    if kind == "pod":
                        preconditions["resourceVersion"] = "1"
                    self.assertEqual(options, {"apiVersion": "v1", "kind": "DeleteOptions", "preconditions": preconditions})
                    calls.append((kind, options["preconditions"]["uid"]))
                    response = Mock(status=409 if conflict is not None else 200)
                    response.read.return_value = b""
                    http.getresponse.return_value = response
                    if conflict == "uid":
                        objects[kind]["metadata"]["uid"] = "replacement"
                    elif conflict == "running":
                        objects[kind]["metadata"]["resourceVersion"] = "2"
                        objects[kind]["status"] = {"phase": "Running", "containerStatuses": [{"name": "environment", "state": {"running": {}}}]}
                    else:
                        objects[kind] = None
                http.request.side_effect = request
                return http
            target.delete_uid = lambda *args: plugin.Target.delete_uid(target, *args)
            with patch.object(plugin, "UnixHTTP", side_effect=connection):
                if conflict is not None:
                    with self.assertRaises(plugin.Failure):
                        target.destroy(binding)
                    self.assertEqual(calls, [("pod", "pod")])
                    self.assertIsNotNone(objects["pvc"])
                    self.assertEqual(objects["pod"]["metadata"]["uid"], "replacement" if conflict == "uid" else "pod")
                    if conflict == "running":
                        self.assertEqual(objects["pod"]["metadata"]["resourceVersion"], "2")
                        self.assertEqual(objects["pod"]["status"]["phase"], "Running")
                else:
                    self.assertEqual(target.destroy(binding), {"absent": True})
                    self.assertEqual(calls, [("pod", "pod"), ("pvc", "pvc")])
                    self.assertEqual(objects, {"pod": None, "pvc": None})

    def test_pinned_init_command_is_required_with_image_mount_and_isolation(self):
        target, binding, objects, _ = self.destroy_fixture()
        pod = objects["pod"]
        self.assertEqual(pod["spec"]["containers"][0]["command"], ["/opt/fabric/fabric-remote-agent", "--init"])
        target.validate_pod(pod, binding["resource"]["name"], binding)
        for command in (["/bin/sleep", "infinity"], ["/opt/fabric/fabric-remote-agent"],
                        ["/opt/fabric/fabric-remote-agent", "--init", "foreign"]):
            changed = copy.deepcopy(pod)
            changed["spec"]["containers"][0]["command"] = command
            with self.assertRaises(plugin.Failure):
                target.validate_pod(changed, binding["resource"]["name"], binding)

    def test_home_initialization_is_claimed_once_and_pod_recreation_only_validates(self):
        for state in ("new", "ready", "initializing", None):
            target, binding, objects, _ = self.destroy_fixture()
            pvc = objects["pvc"]
            pvc["metadata"]["resourceVersion"] = "1"
            if state is not None:
                pvc["metadata"]["annotations"]["fabric.sh/home-state"] = state
            updates, initialized = [], []
            def replace(argv, value):
                self.assertEqual(argv, ["replace", "-f", "-", "-o", "json"])
                self.assertEqual(value["metadata"]["uid"], "pvc")
                self.assertEqual(value["metadata"]["resourceVersion"], str(len(updates) + 1))
                updates.append(value["metadata"]["annotations"]["fabric.sh/home-state"])
                saved = copy.deepcopy(value)
                saved["metadata"]["resourceVersion"] = str(len(updates) + 1)
                return saved
            target.kubectl = replace
            def start(t, pod, observed, initialize=False):
                initialized.append(initialize)
                agent = Mock(instance=pod["metadata"]["uid"], binding=observed)
                agent.call.return_value = {"instance": agent.instance, "os": "linux", "environment": observed}
                return agent
            with patch.object(plugin, "Agent", side_effect=start):
                if state in ("new", "ready"):
                    agent = plugin.Target.agent(target, binding, objects["pod"])
                    self.assertEqual(initialized, [state == "new"])
                    self.assertEqual(updates, ["initializing", "ready"] if state == "new" else [])
                    # Same proven PVC, distinct actual Pod: never initialize again.
                    objects["pvc"]["metadata"]["annotations"]["fabric.sh/home-state"] = "ready"
                    target.agents.clear()
                    objects["pod"]["metadata"]["uid"] = "replacement-pod"
                    replacement = plugin.Target.agent(target, binding, objects["pod"])
                    self.assertEqual(replacement.instance, "replacement-pod")
                    self.assertEqual(initialized[-1], False)
                    self.assertNotEqual(agent.instance, replacement.instance)
                else:
                    with self.assertRaises(plugin.Failure):
                        plugin.Target.agent(target, binding, objects["pod"])
                    self.assertEqual(initialized, [])
                    self.assertEqual(updates, [])

    def test_failed_initial_home_claim_is_not_retried_or_adopted(self):
        target, binding, objects, _ = self.destroy_fixture()
        pvc = objects["pvc"]
        pvc["metadata"].update(resourceVersion="1")
        pvc["metadata"]["annotations"]["fabric.sh/home-state"] = "new"
        def replace(argv, value):
            objects["pvc"] = copy.deepcopy(value)
            return objects["pvc"]
        target.kubectl = replace
        agent = Mock()
        agent.call.side_effect = plugin.Failure()
        with patch.object(plugin, "Agent", return_value=agent) as constructor:
            with self.assertRaises(plugin.Failure):
                plugin.Target.agent(target, binding, objects["pod"])
            self.assertEqual(objects["pvc"]["metadata"]["annotations"]["fabric.sh/home-state"], "initializing")
            with self.assertRaises(plugin.Failure):
                plugin.Target.agent(target, binding, objects["pod"])
            self.assertEqual(constructor.call_count, 1)
            agent.close.assert_called_once_with()

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
