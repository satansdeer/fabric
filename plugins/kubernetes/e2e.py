#!/usr/bin/env python3
"""Opt-in REAL installed-target acceptance. Never run implicitly or from CI.

Uses only a separately bootstrapped, labeled namespace and owner-provided private
configuration. No provider calls, OAuth, namespace deletion, credential copying,
or adoption of existing Pod/PVC resources. All child waits use original handles.
"""
import argparse
import fcntl
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import pty
import select
import signal
import socket
import struct
import subprocess
import sys
import tarfile
import termios
import time
import uuid

HERE = pathlib.Path(__file__).resolve().parent
SOURCE = HERE.parent.parent
spec = importlib.util.spec_from_file_location("kubernetes_target", HERE / "target.py")
implementation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(implementation)


def require(value, label):
    if not value:
        raise AssertionError(label)


def wait_until(predicate, seconds, label):
    deadline = time.monotonic() + seconds
    while True:
        value = predicate()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise AssertionError(label)
        time.sleep(0.1)


class Terminal:
    def __init__(self, proof):
        self.proof, self.master = proof, None
        master, slave = pty.openpty()
        self.master = master
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        try:
            self.process = subprocess.Popen(proof.command("shell", proof.environment, "--owner", proof.owner,
                                                        "--target", "kubernetes"),
                                            stdin=slave, stdout=slave, stderr=slave, env=proof.env,
                                            start_new_session=True)
        except BaseException:
            os.close(master)
            self.master = None
            raise
        finally:
            os.close(slave)
        proof.children.append(self.process)
        self.buffer = bytearray()
        try:
            self.send(b"stty -echo; printf '\\122\\105\\101\\104\\131\\n'\n")
            self.expect(b"READY\r\n")
        except BaseException:
            self.close("disconnect")
            raise

    def send(self, data):
        os.write(self.master, data)

    def expect(self, needle, seconds=60):
        deadline = time.monotonic() + seconds
        while needle not in self.buffer:
            remaining = deadline - time.monotonic()
            require(remaining > 0 and select.select([self.master], [], [], remaining)[0], "terminal output deadline")
            try:
                data = os.read(self.master, 4096)
            except OSError:
                raise AssertionError("terminal closed before expected bytes") from None
            require(data, "terminal transport EOF")
            self.buffer.extend(data)
            require(len(self.buffer) <= 65536, "terminal fixture buffer bound")
        end = self.buffer.index(needle) + len(needle)
        result = bytes(self.buffer[:end])
        del self.buffer[:end]
        return result

    def close(self, how="exit"):
        try:
            if self.process.poll() is None:
                if how == "exit":
                    self.send(b"exit\n")
                else:
                    self.process.send_signal(signal.SIGTERM if how == "cancel" else signal.SIGKILL)
                self.process.wait(timeout=30)
            if how == "exit":
                require(self.process.returncode == 0, "interactive exit status")
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
            if self.master is not None:
                os.close(self.master)
                self.master = None


class Proof:
    def __init__(self, args):
        self.args = args
        self.directory = pathlib.Path(args.work).resolve()
        require(not self.directory.exists(), "work directory must be NEW")
        self.directory.mkdir(mode=0o700)
        self.root = self.directory / "storage"
        self.registry = self.directory / "registry"
        self.registry.mkdir(mode=0o700)
        self.home = self.directory / "controller-home"
        self.home.mkdir(mode=0o700)
        self.env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(self.home),
                    "FABRIC_PLUGIN_REGISTRY": str(self.registry)}
        self.owner = "issue33"
        self.environment = "k33-" + uuid.uuid4().hex[:16]
        self.canary = "pty-only-" + uuid.uuid4().hex
        self.marker = b"provider-free-pvc-marker\n"
        self.target = None
        self.settings = None
        self.binding = None
        self.children = []

    def command(self, *args):
        return [self.args.fabric, "--root", str(self.root), *map(str, args)]

    def call(self, *args, ok=True):
        result = subprocess.run(self.command(*args), env=self.env, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=240)
        require((result.returncode == 0) == ok, "CLI outcome: " + " ".join(map(str, args[:2])))
        return json.loads(result.stdout) if ok else result

    def install(self, configuration, upgrade=False):
        digest = hashlib.sha256(implementation.private_bytes(configuration, 65536)).hexdigest()
        settings = implementation.config(configuration, digest)
        if self.target is not None:
            self.target.close()
        self.target = implementation.Target(settings, digest)
        self.settings = settings
        self.namespace_uid = self.target.namespace()
        # This independently supplied namespace must not already contain our IDs.
        incoming = dict(environment_id=self.environment, owner=self.owner, resource=None)
        name = self.target.names(incoming)
        require(self.target.get("pod", name) is None and self.target.get("pvc", name) is None, "never adopt resources")
        package = self.directory / ("package-" + self.environment)
        generated = subprocess.run([self.args.node, str(HERE / "package.mjs"), str(package),
                                    str(pathlib.Path(sys.executable).resolve()), configuration],
                                   env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        require(generated.returncode == 0, "package generation")
        pin = json.loads(generated.stdout)
        install = [self.args.node, str(SOURCE / "sdk/typescript/cli.mjs"), "install", str(package),
                   "--registry", str(self.registry), "--fabric", self.args.fabric, "--version", "0.1.0",
                   "--sha256", pin["sha256"], "--trust"]
        if upgrade:
            install.append("--upgrade")
        result = subprocess.run(install, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        require(result.returncode == 0, "explicit content-pinned installation")
        self.binding = incoming

    def identity_args(self):
        return [self.environment, "--owner", self.owner, "--target", "kubernetes"]

    def ensure(self, ok=True):
        return self.call("environments", "ensure", *self.identity_args(), "--interactive-exec-pty", "--json", ok=ok)

    def observe(self):
        name = self.target.names(self.binding)
        pvc = self.target.get("pvc", name)
        require(pvc is not None, "owned PVC exists")
        uid = self.target.owned(pvc, self.binding, name)
        resource = dict(backend="remote.v1", resource_id="k8s:" + self.namespace_uid + ":" + uid,
                        namespace_uid=self.namespace_uid, pvc_uid=uid, name=name,
                        config_sha256=self.target.digest)
        self.binding["resource"] = resource
        pod = self.target.get("pod", name)
        require(pod is not None, "owned Pod exists")
        self.target.validate_pod(pod, name, self.binding)
        return pod

    def destroy(self, ok=True, confirmation=None):
        return self.call("environments", "destroy", *self.identity_args(), "--confirm",
                         confirmation or self.environment, "--json", ok=ok)

    def exec_bytes(self, *argv):
        pod = self.observe()
        process = self.target.spawn(["exec", pod["metadata"]["name"], "-c", "environment", "--", *argv],
                                    stdout=subprocess.PIPE)
        try:
            data, _ = process.communicate(timeout=15)
            require(process.returncode == 0 and len(data) <= 65536, "actual Pod exec")
            return data
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            self.target.forget(process)

    def prepare(self):
        repo = self.directory / "repo"
        repo.mkdir(mode=0o700)
        (repo / "tracked").write_bytes(b"source bytes\n")
        git_env = dict(self.env, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
                       GIT_AUTHOR_NAME="Fixture", GIT_AUTHOR_EMAIL="fixture@example.invalid",
                       GIT_COMMITTER_NAME="Fixture", GIT_COMMITTER_EMAIL="fixture@example.invalid")
        for args in (["init", "--quiet"], ["add", "tracked"], ["commit", "--quiet", "-m", "provider-free fixture"]):
            result = subprocess.run(["/usr/bin/git", "-C", str(repo), *args], env=git_env,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
            require(result.returncode == 0, "isolated source fixture")
        recipe = self.directory / "recipe.toml"
        recipe.write_text('version=1\n[[steps]]\nargv=["/usr/bin/true"]\n')
        self.call("project", "add", "--name", "proof", "--path", repo, "--json")
        self.call("project", "prepare", "proof", "--recipe", recipe, "--min-free-bytes", "1", "--json")

    def run_args(self, prompt):
        return ["run", "proof", "--prompt-file", str(prompt), "--harness", "fake", "--placement", "kubernetes",
                "--environment", self.environment, "--environment-owner", self.owner, "--interactive-exec-pty",
                "--copy-limit-bytes", "134217728", "--min-free-bytes", "1", "--json"]

    def prompt(self, name, value):
        path = self.directory / name
        path.write_text(json.dumps(value))
        return path

    def verify_artifacts(self, run):
        require(run["state"] == "SUCCEEDED" and run["exit_code"] == 0, "actual successful fake worker")
        artifacts = self.call("runs", "artifacts", run["run_id"], "--json")
        receipt = artifacts["launch_receipt"]
        remote = receipt["remote"]
        require("workspace_device" not in receipt and "workspace_inode" not in receipt, "no invented remote inode")
        pod = self.observe()
        require(remote["os"] == "linux" and remote["instance"] == pod["metadata"]["uid"]
                and remote["hostname"] == pod["metadata"]["name"]
                and remote["hostname"] != socket.gethostname(), "distinct actual execution identity")
        content = {}
        for artifact in artifacts["artifacts"]:
            path = pathlib.PurePosixPath(artifact["relative_path"])
            require(not path.is_absolute() and ".." not in path.parts, "artifact path")
            data = (self.root / path).read_bytes()
            require(len(data) == artifact["size_bytes"] and hashlib.sha256(data).hexdigest() == artifact["sha256"], "sealed artifact bytes")
            require(self.canary.encode() not in data, "PTY content must not enter artifacts")
            content[artifact["kind"]] = data
        with tarfile.open(fileobj=io.BytesIO(content["untracked_archive"]), mode="r:") as archive:
            member = archive.getmember("identity.json")
            require(member.isfile() and member.size <= 4096, "real worker artifact")
            identity = json.loads(archive.extractfile(member).read())
        require(bytes(identity["marker"]) == self.marker and identity["os"] == "linux"
                and identity["hostname"] == remote["hostname"] and identity["instance"] == remote["instance"], "worker observed persistent HOME")
        require(run["cleanup"]["action"] == "reclaimed", "disposable workspace reclaimed")
        resource = remote["resource"]
        require(self.exec_bytes("/bin/sh", "-c", 'test ! -e "/work/fabric/$1"; printf gone', "sh", resource["run_id"]) == b"gone", "remote workspace actually absent")
        return identity

    def terminal_process(self, how):
        terminal = Terminal(self)
        try:
            self.destroy(ok=False)
            # The shell supplies its ACTUAL PID and Linux start time, not a guessed PID.
            terminal.send(b"printf '%s ' \"$$\" > \"$HOME/session-process\"; cut -d ' ' -f 22 /proc/$$/stat >> \"$HOME/session-process\"; sleep 120\n")
            home = "/home/fabric/fabric/environments/" + self.environment + "/tree/"
            def witness():
                value = self.exec_bytes("/bin/sh", "-c", 'test ! -f "$1" || cat "$1"', "sh", home + "session-process").split()
                return value if len(value) == 2 else None
            pid, start = wait_until(witness, 15, "actual shell PID witness")
            require(pid.isdigit() and start.isdigit(), "Linux process witness")
            terminal.close(how)
            def gone():
                state = self.exec_bytes("/bin/sh", "-c", 'test ! -e "/proc/$1/stat" || cat "/proc/$1/stat"', "sh", pid.decode()).split()
                return not state or state[2] == b"Z" or state[21] != start
            wait_until(gone, 30, "ordinary shell process stopped after " + how)
            self.exec_bytes("/bin/rm", home + "session-process")
        finally:
            terminal.close("disconnect")

    def scenario(self):
        self.install(self.args.config)
        self.ensure()
        original = self.observe()
        pvc_uid = self.binding["resource"]["pvc_uid"]
        terminal = Terminal(self)
        try:
            self.destroy(ok=False)
            terminal.send(("umask 077; printf 'provider-free-pvc-marker\\n' > \"$HOME/persistence-marker\"; printf '%s\\n' '" + self.canary + "'; printf '\\377\\376\\n'\n").encode())
            terminal.expect(self.canary.encode())
            terminal.expect(b"\xff\xfe\r\n")
            fcntl.ioctl(terminal.master, termios.TIOCSWINSZ, struct.pack("HHHH", 37, 101, 0, 0))
            terminal.process.send_signal(signal.SIGWINCH)
            terminal.send(b"stty size\n")
            terminal.expect(b"37 101\r\n")
            terminal.send(b"sleep 120\n")
            time.sleep(0.2)
            terminal.send(b"\x03")
            terminal.send(b"printf '\\103\\117\\116\\124\\122\\117\\114\\n'\n")
            terminal.expect(b"CONTROL\r\n")
            terminal.close()
        finally:
            terminal.close("disconnect")
        self.prepare()
        prompt = self.prompt("probe.json", {"required_files": ["tracked"], "environment_probe": {"home_file": "persistence-marker", "artifact": "identity.json"}})
        first = self.verify_artifacts(self.call(*self.run_args(prompt)))
        # Delete ONLY the validated owned Pod with an API-server UID precondition.
        self.target.delete_uid("pod", original["metadata"]["name"], original["metadata"]["uid"])
        self.call("environments", "inspect", *self.identity_args(), "--interactive-exec-pty", "--json")
        recreated = self.observe()
        require(recreated["metadata"]["uid"] != original["metadata"]["uid"] and self.binding["resource"]["pvc_uid"] == pvc_uid, "same PVC, new actual Pod")
        second = self.verify_artifacts(self.call(*self.run_args(prompt)))
        require(second["marker"] == first["marker"] and second["instance"] != first["instance"], "persistent bytes across real recreation")
        cancel = self.prompt("cancel.json", {"process_tree": {"ignore_child_term": True}})
        child = subprocess.Popen(self.command(*self.run_args(cancel)), env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.children.append(child)
        def active():
            runs = self.call("runs", "list", "--json")
            return next((r for r in runs if r["state"] == "RUNNING"), None)
        running = wait_until(active, 60, "actual remote run started")
        self.destroy(ok=False)
        # Observe both actual remote OS processes before cancellation; retain
        # start times so PID reuse cannot masquerade as an unreaped fixture.
        remote_tree = "/work/fabric/" + running["run_id"] + "/tree"
        witnesses = []
        for filename in ("parent.heartbeat", "child.heartbeat"):
            def witness():
                data = self.exec_bytes("/bin/sh", "-c", 'test ! -f "$1" || head -n 1 "$1"', "sh", remote_tree + "/" + filename).strip()
                if not data.startswith(b"pid="):
                    return None
                pid = data[4:]
                require(pid.isdigit(), "actual worker PID")
                stat = self.exec_bytes("/bin/cat", "/proc/" + pid.decode() + "/stat").split()
                return (pid.decode(), stat[21])
            witnesses.append(wait_until(witness, 15, "actual heartbeat process"))
        child.send_signal(signal.SIGTERM)
        output, _ = child.communicate(timeout=60)
        require(child.returncode != 0, "cancelled foreground exit")
        cancelled = self.call("runs", "show", running["run_id"], "--json")
        require(cancelled["run"]["state"] == "CANCELLED", "durable cancelled run")
        require(cancelled["workspace"]["state"] == "RECLAIMED", "cancel reclaims only disposable workspace")
        self.terminal_process("cancel")
        self.terminal_process("disconnect")
        for pid, start in witnesses:
            def stopped():
                state = self.exec_bytes("/bin/sh", "-c", 'test ! -e "/proc/$1/stat" || cat "/proc/$1/stat"', "sh", pid).split()
                return not state or state[2] == b"Z" or state[21] != start
            wait_until(stopped, 15, "owned worker process stopped")
        self.destroy(ok=False, confirmation="wrong")
        self.destroy()
        name = self.target.names(self.binding)
        require(self.target.get("pod", name) is None and self.target.get("pvc", name) is None
                and self.target.namespace() == self.namespace_uid, "explicit destroy only owned resources")
        # No transcript/default capture: scan only our disposable controller root.
        for path in self.root.rglob("*"):
            if path.is_file() and not path.is_symlink():
                require(self.canary.encode() not in path.read_bytes(), "no persisted PTY transcript")
        # A SECOND explicit private config selects a CPU request the owner has
        # independently established cannot schedule in this test namespace.
        self.environment = "k33-pending-" + uuid.uuid4().hex[:12]
        self.install(self.args.scheduling_config, upgrade=True)
        started = time.monotonic()
        self.ensure(ok=False)
        pending = self.observe()
        evidence = implementation.status_evidence(pending)
        require(evidence.get("reason") == "Unschedulable" and time.monotonic() - started < self.settings["scheduling_timeout_seconds"] + 45, "bounded real scheduling failure")
        self.destroy()
        require(self.target.get("pvc", self.target.names(self.binding)) is None, "owned Pending resources cleaned")
        print(json.dumps({"scenario": "installed-kubernetes-cli", "persistence": "VERIFIED", "pty": "VERIFIED",
                          "cancel_reclaim": "VERIFIED", "scheduling": "VERIFIED", "real_oom": "NOT_VERIFIED",
                          "oauth": "NOT_AUTHORIZED", "namespace_deleted": False}))

    def close(self):
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.wait()
        if self.target is not None:
            self.target.close()
        # Failure retains exact owned resources for explicit CLI destruction;
        # never guess cleanup from a partially completed test.


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-kubernetes", action="store_true", required=True)
    parser.add_argument("--fabric", required=True)
    parser.add_argument("--node", required=True)
    parser.add_argument("--config", required=True)
    parser.add_argument("--scheduling-config", required=True)
    parser.add_argument("--work", required=True)
    args = parser.parse_args()
    for key in ("fabric", "node", "config", "scheduling_config", "work"):
        require(os.path.isabs(getattr(args, key)), "explicit absolute inputs")
    proof = Proof(args)
    try:
        proof.scenario()
    finally:
        proof.close()


if __name__ == "__main__":
    main()
