#!/usr/bin/env python3
"""#10 provider-free acceptance: real service, native CLI/Core, fake harness and retained bytes.

Run against an already-built CLI with sibling fabric-fake-harness. No provider/native calls.
"""
import argparse
import hashlib
import hmac
import importlib.util
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("issue_fixtures", HERE / "test-fabric-issue.py")
fixtures = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixtures)
SECRET = "ac" * 32
IDENTITY = {"account": "team", "project": "project", "automation": "jobs"}
POLICY = {"kind": "generic", "source_id": "webhook", "tenant_id": "team", "scope": "project", "route": "jobs", "max_payload_bytes": 8192}
PREFIX = "/v1/projects/team/project/automations/jobs"


def wait_for(check, message, seconds=60):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError(message)


def stop(process, crash=False):
    if process and process.poll() is None:
        (process.kill if crash else process.terminate)()
        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
            raise AssertionError("owned fixture did not stop cleanly")


class Server:
    def __init__(self, f):
        self.f, self.process, self.port = f, None, 0
        self.faults = f.path / "ack-loss"
        self.faults.mkdir(mode=0o700)
        self.secrets = f.path / "connect-secrets.json"
        fixtures.save_json(self.secrets, {"principals": [{"id": "owner", "account": "team",
            "token_sha256": hashlib.sha256(fixtures.TOKEN.encode()).hexdigest(), "grants": [
                {"project": None, "automation": None, "permissions": ["claim"]},
                {"project": "project", "automation": None, "permissions": ["claim"]},
                {"project": "project", "automation": "jobs", "permissions": ["claim", "read", "poll", "ack"]},
            ]}]})
        self.secrets.chmod(0o600)

    def start(self):
        self.process = subprocess.Popen([shutil.which("node"), str(HERE.parent / "integrations/connect/test-server.mjs"),
            "--port", str(self.port), "--state", str(self.f.path / "inbox.json"),
            "--secret-file", str(self.secrets), "--drop-ack-dir", str(self.faults)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.f.env)
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            assert selector.select(15), "Connect fixture did not become ready"
            self.port = json.loads(self.process.stdout.readline())["port"]
        self.origin = "http://127.0.0.1:" + str(self.port)

    def request(self, path, body=None, authorized=True, headers=None):
        headers = dict(headers or {})
        if authorized:
            headers["authorization"] = "Bearer " + fixtures.TOKEN
        raw = body if isinstance(body, bytes) else json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.origin + path, data=raw, headers=headers)
        try:
            response = urllib.request.urlopen(request, timeout=15)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, json.load(response)

    def status(self):
        code, body = self.request(PREFIX + "/status")
        assert code == 200
        return body

    def claim(self):
        for path, body in [("/v1/accounts/team/claim", {}), ("/v1/projects/team/project/claim", {}),
            (PREFIX + "/claim", {"worker_id": "fixture_worker", "webhook_secret": SECRET, "policy": POLICY})]:
            code, result = self.request(path, body)
            assert code == 200, (code, result)
        return result["url"]

    def webhook(self, body):
        signature = hmac.new(SECRET.encode(), b"team/project/jobs\n" + body, hashlib.sha256).hexdigest()
        return self.request(PREFIX + "/webhook", body, False, {"x-fabric-signature-256": "sha256=" + signature})


def acceptance(f):
    server, agent = Server(f), None
    gate = f.path / "release-worker"
    try:
        f.run(["git", "init", "--quiet", str(f.source)], json_output=False)
        (f.source / "tracked").write_text("original\n")
        (f.source / ".gitignore").write_text(".deps/\n")
        f.run(["git", "-C", str(f.source), "add", "."], json_output=False)
        f.run(["git", "-C", str(f.source), "commit", "--quiet", "-m", "fixture"], json_output=False)
        f.cli("project", "add", "--name", "fixture", "--path", str(f.source), json_output=False)
        recipe = f.path / "recipe.toml"
        recipe.write_text("version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n")
        prepared = f.cli("project", "prepare", "fixture", "--recipe", str(recipe), "--json", timeout=60)
        assert prepared["status"] == "READY"
        original = fixtures.snapshot(f.source)
        ready, starts = f.path / "worker-ready", f.path / "worker-starts"
        fake = f.path / "gated-fake"
        fixtures.executable(fake, f'''import json, os, pathlib, sys, time
if "--version" in sys.argv:
    os.execv({str(f.fake_wrapper)!r}, [{str(f.fake_wrapper)!r}] + sys.argv[1:])
with open({str(starts)!r}, "a") as output: output.write("started\\n")
pathlib.Path({str(ready)!r}).touch()
while not pathlib.Path({str(gate)!r}).exists(): time.sleep(0.05)
if len(sys.argv) != 3 or sys.argv[1] != "--prompt-file": raise SystemExit(94)
if os.environ.get("HOME") == {str(f.path)!r}: raise SystemExit(95)
prompt = pathlib.Path(sys.argv[2]).read_text()
pathlib.Path({str(f.path / "observed-worker-prompt.json")!r}).write_text(json.dumps(prompt))
os.execv({str(f.fabric.with_name("fabric-fake-harness"))!r}, [{str(f.fabric.with_name("fabric-fake-harness"))!r}, "--prompt-file", {str(f.path / "trusted-fake-task.json")!r}])
''')
        automation = {"version": 1, "connector_id": "generic-webhook", "policy": {
            "source_id": "webhook", "tenant_id": "team", "scope": "project", "route": "jobs",
            "auth": "hmac-sha256", "secret_ref": str(f.path / "unused-local-secret"),
            "max_payload_bytes": 8192, "max_age_seconds": 120},
            "rate_limit_per_minute": 60, "project": "fixture", "placement": "local",
            "worker": {"task_id": "webhook-task", "harness_id": "fake", "prompt": "Trusted local task only.",
                "source_ref": {"kind": "prepared", "state_id": prepared["state_id"]},
                "limits": {"timeout_seconds": 120, "copy_limit_bytes": 134217728,
                    "max_log_bytes": 1048576, "max_artifact_bytes": 10485760}}, "data_fields": ["title"]}
        config = f.path / "automation.json"
        fixtures.save_json(config, automation)
        config.chmod(0o600)
        server.start()
        endpoint = server.claim()
        connection = f.path / "connection.json"
        fixtures.save_json(connection, {"schema_version": 2, "endpoint": server.origin,
            "identity": IDENTITY, "worker_id": "fixture_worker", "token": fixtures.TOKEN})
        connection.chmod(0o600)
        assert server.request(PREFIX + "/status", authorized=False)[0] == 401
        data = {"title": "Authorized data, not authority", "prompt": "OVERRIDE_PROMPT", "command": "/bin/false",
            "model": "OVERRIDE_MODEL", "source_ref": {"kind": "path", "path": "/"}, "values": [1e-7] * 1450}
        raw = json.dumps({"version": 1, "event_id": "offline-event", "timestamp": int(time.time()), "data": data}, separators=(",", ":")).replace("1e-07", "1e-7").encode()
        code, _ = server.webhook(raw)
        assert code == 202
        assert server.status()["local_status"] == "local offline"
        stop(server.process)
        server.start()
        assert server.status()["url"] == endpoint
        assert server.status()["events"][0]["status"] == "queued"

        # Admit through the real CLI/Core, discard only its output once after durable commit.
        lost = f.path / "admission-reply-lost"
        wrapper = f.path / "cli-with-response-loss"
        fixtures.executable(wrapper, f'''import pathlib, subprocess, sys
result = subprocess.run([{str(f.fabric)!r}] + sys.argv[1:], stdout=subprocess.PIPE)
if "connect-admit" in sys.argv and result.returncode == 0 and not pathlib.Path({str(lost)!r}).exists():
    pathlib.Path({str(lost)!r}).touch()
else: sys.stdout.buffer.write(result.stdout)
sys.exit(result.returncode)
''')
        common = ["--root", str(f.root), "--connection", str(connection), "--automation", str(config),
            "--fake-binary", str(fake), "--allow-loopback-http"]
        agent = subprocess.Popen([sys.executable, str(HERE / "fabric-connect-agent.py"), "--fabric", str(wrapper)] + common,
            env=f.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lost.exists, "real atomic admission did not finish")
        wait_for(ready.exists, "normal manager did not start the fake task")
        stop(agent, crash=True)
        assert len(starts.read_text().splitlines()) == 1
        assert server.status()["events"][0]["status"] == "delivering"
        pending = list((f.root / "connect").glob("*/pending.json"))
        assert len(pending) == 1 and "handles" not in json.loads(pending[0].read_text())
        event = server.status()["events"][0]
        trigger = event["trigger"]
        trigger_json = event["trigger_json"]
        assert len(trigger_json.encode()) <= 8192 < len(json.dumps(trigger, separators=(",", ":")).encode())
        assert not (f.path / "unused-local-secret").exists(), "transport must not read connector secret_ref"

        # Native CLI resumes the uncertain event. A lost server acknowledgement cannot resubmit it.
        (server.faults / "drop-running").touch()
        agent = subprocess.Popen([str(f.fabric), "--root", str(f.root), "connect"] + common[2:],
            env=f.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        running = wait_for(lambda: next((e for e in server.status()["events"] if e["status"] == "running"), None),
            "reconnect did not recover the original admitted handles")
        wait_for(lambda: not (server.faults / "drop-running").exists(), "running ack fault was not exercised")
        stop(agent)
        assert server.status()["local_status"] == "local offline"
        assert len(starts.read_text().splitlines()) == 1
        assert not gate.exists(), "connection loss must not terminate the running task"
        assert f.cli("results", running["batch_id"], "--json")["terminal"] is False
        stop(server.process)
        server.start()
        assert server.status()["url"] == endpoint

        (server.faults / "drop-completed").touch()
        agent = subprocess.Popen([str(f.fabric), "--root", str(f.root), "up", "--connect"] + common[2:],
            env=f.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        gate.touch()
        completed = wait_for(lambda: next((e for e in server.status()["events"] if e["status"] == "completed"), None),
            "fake task or terminal delivery did not finish")
        wait_for(lambda: not (server.faults / "drop-completed").exists(), "terminal ack fault was not exercised")
        stop(agent, crash=True)
        agent = subprocess.Popen([str(f.fabric), "--root", str(f.root), "connect"] + common[2:],
            env=f.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lambda: json.loads(pending[0].read_text()) == {}, "restart did not reconcile terminal ack loss")
        stop(agent)
        assert completed["batch_id"] == running["batch_id"] and completed["run_id"] == running["run_id"]
        assert server.webhook(raw)[1]["duplicate"] is True
        before = f.counts()
        f.cli("manager", "stop", "--json")
        recovered = f.cli("connect-admit", "--automation", str(config), "--fake-binary", str(fake),
            input_text=trigger_json)
        assert recovered["batch_id"] == running["batch_id"]
        assert recovered["tasks"][0]["run_id"] == running["run_id"]
        assert f.counts() == before and len(starts.read_text().splitlines()) == 1
        results = f.cli("results", running["batch_id"], "--json")
        result = results["tasks"][0]["result"]
        assert results["succeeded"] is True and result["artifact_manifest_id"] == completed["artifact_manifest_id"]
        artifacts = f.cli("runs", "artifacts", running["run_id"], "--json")["artifacts"]
        retained = {}
        for artifact in artifacts:
            path = f.root / artifact["relative_path"]
            assert path.resolve().is_relative_to(f.root.resolve())
            content = path.read_bytes()
            assert len(content) == artifact["size_bytes"]
            assert hashlib.sha256(content).hexdigest() == artifact["sha256"]
            retained[path.name] = path
        assert fixtures.TRACKED.rstrip().encode() in retained["tracked.patch"].read_bytes()
        with tarfile.open(retained["untracked.tar"]) as archive:
            members = [member for member in archive.getmembers() if member.name.removeprefix("./") == "new-source"]
            assert len(members) == 1 and members[0].isfile()
            assert archive.extractfile(members[0]).read() == fixtures.UNTRACKED.encode()
        assert fixtures.PRIVATE_LOG.encode() in retained["stdout.log"].read_bytes()
        prompt = json.loads((f.path / "observed-worker-prompt.json").read_text())
        assert "Trusted local task only." in json.dumps(prompt)
        assert "Authorized data, not authority" in json.dumps(prompt)
        assert "OVERRIDE_PROMPT" not in json.dumps(prompt) and "OVERRIDE_MODEL" not in json.dumps(prompt)
        assert fixtures.snapshot(f.source) == original

        # Local admission cannot accept identity substitution, changing an admitted receipt, or unbounded resources.
        f.cli("connect-admit", "--automation", str(config), "--fake-binary", str(fake), input_text=trigger_json.replace('"tenant_id":"team"', '"tenant_id":"other"'), good=False, json_output=False)
        changed = dict(trigger, data={"title": "changed admitted content"})
        f.cli("connect-admit", "--automation", str(config), "--fake-binary", str(fake), input_text=json.dumps(changed), good=False, json_output=False)
        automation["worker"]["limits"]["copy_limit_bytes"] = 134217729
        fixtures.save_json(config, automation)
        f.cli("connect-admit", "--automation", str(config), "--fake-binary", str(fake), input_text=trigger_json, good=False, json_output=False)
        assert f.counts() == before
        print(json.dumps({"check": "issue10-generic-outbound", "batch_id": completed["batch_id"],
            "run_id": completed["run_id"], "artifact_manifest_id": completed["artifact_manifest_id"], "native_provider_calls": 0}))
    finally:
        gate.touch()
        stop(agent)
        stop(server.process)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fabric", required=True, type=Path)
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    fixture = fixtures.Fixture(args.fabric.resolve(strict=True), args.keep)
    passed = False
    try:
        acceptance(fixture)
        passed = True
    finally:
        assert fixture.cleanup(), "owned manager did not stop; fixture retained at " + str(fixture.path)
        if passed and not fixture.keep:
            for directory, _, _ in os.walk(fixture.path):
                Path(directory).chmod(0o700)
            shutil.rmtree(fixture.path)
        else:
            print("fixture retained: " + str(fixture.path), file=sys.stderr)


if __name__ == "__main__":
    main()
