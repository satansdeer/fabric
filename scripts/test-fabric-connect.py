#!/usr/bin/env python3
"""Provider-free durable inbox/real Fabric fake-worker integration check."""

import argparse
import copy
from datetime import datetime, timezone
import hashlib
import hmac
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import stat
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request
import uuid


spec = importlib.util.spec_from_file_location("issue_fixture", Path(__file__).with_name("test-fabric-issue.py"))
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)
require = fixtures.require
PREFIX = "/v1/projects/satansdeer/fabric/automations/issues"
WORKER = "fixture-owner-worker-18"
TOKEN = "connect-fixture-token-not-a-real-credential-18"
SECRET = hashlib.sha256(b"connect-fixture-webhook-not-a-real-secret-18").hexdigest()


class Server:
    def __init__(self, fixture):
        self.fixture = fixture
        self.process = None
        self.node = shutil.which("node")
        require(self.node is not None, "node is required for the local signed inbox fixture")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            self.port = listener.getsockname()[1]
        self.endpoint = "http://127.0.0.1:" + str(self.port)
        self.state = fixture.path / "hosted-state.json"
        self.secrets = fixture.path / "hosted-secrets.json"
        fixtures.save_json(self.secrets, {"webhook_secret": SECRET,
                                          "worker_token_sha256": hashlib.sha256(TOKEN.encode()).hexdigest(),
                                          "worker_id": WORKER})
        self.secrets.chmod(0o600)
        self.connection = fixture.path / "connection.json"
        fixtures.save_json(self.connection, {"schema_version": 1, "endpoint": self.endpoint,
                                              "worker_id": WORKER, "token": TOKEN})
        self.connection.chmod(0o600)
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(self, path, payload=None, *, token=None, signed=False, signature=None, delivery=None):
        data = None if payload is None else (payload if isinstance(payload, bytes) else json.dumps(payload).encode())
        headers = {"Content-Type": "application/json"}
        if token is not None:
            headers["Authorization"] = "Bearer " + token
        if signed:
            headers.update({"X-GitHub-Event": "issues", "X-GitHub-Delivery": delivery or str(uuid.uuid4()),
                            "X-Hub-Signature-256": signature or "sha256=" + hmac.new(SECRET.encode(), data, hashlib.sha256).hexdigest()})
        request = urllib.request.Request(self.endpoint + path, data=data, headers=headers,
                                         method="GET" if data is None else "POST")
        try:
            response = self.opener.open(request, timeout=5)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            raw = response.read(1024 * 1024 + 1)
            require(len(raw) <= 1024 * 1024, "fixture response is oversized")
            return response.status, json.loads(raw)

    def start(self):
        script = Path(__file__).resolve().parents[1] / "integrations/connect/test-server.mjs"
        self.process = subprocess.Popen(
            [self.node, str(script), "--port", str(self.port), "--state", str(self.state),
             "--secret-file", str(self.secrets)], cwd=self.fixture.path,
            env={"PATH": "/usr/bin:/bin", "HOME": str(self.fixture.path)},
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            require(self.process.poll() is None, "local inbox server exited during startup")
            try:
                code, body = self.request("/health")
                if code == 200 and body == {"service": "fabric-connect", "schema_version": 1}:
                    return
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(0.05)
        raise AssertionError("local inbox server did not become ready")

    def stop(self):
        if self.process is not None:
            if self.process.poll() is None:
                self.process.terminate()  # Only our own live child, never a persisted PID.
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)
            self.process = None

    def view(self):
        code, value = self.request(PREFIX + "/status", token=TOKEN)
        require(code == 200, "authenticated fixture status failed")
        return value

    def webhook(self, fixture, action="edited", **changes):
        issue = copy.deepcopy(fixture.issue)
        issue["updated_at"] = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
        payload = {"action": action, "repository": {"full_name": fixtures.REPO},
                   "sender": {"login": "satansdeer"}, "issue": issue}
        payload.update(changes)
        return json.dumps(payload, sort_keys=True).encode()


def check(f, server, report):
    f.issue["user"] = {"login": "satansdeer"}
    fixtures.save_json(f.issue_path, f.issue)
    issue_args = ["--repo", fixtures.REPO, "--issue", str(fixtures.ISSUE),
                  "--allow-author", "satansdeer", "--allow-label", "ready", "--gh", f.bin / "gh"]
    approval = f.bridge_command("preview", *issue_args)["approval_sha256"]
    f.run(["/usr/bin/git", "-C", f.source, "init", "--quiet"], json_output=False)
    (f.source / "tracked").write_text("original\n")
    (f.source / ".gitignore").write_text(".deps/\n")
    f.run(["/usr/bin/git", "-C", f.source, "add", "."], json_output=False)
    f.run(["/usr/bin/git", "-C", f.source, "commit", "--quiet", "-m", "disposable fixture"], json_output=False)
    f.cli("project", "add", "--name", "fabric", "--path", f.source, json_output=False)
    recipe = f.path / "recipe.toml"
    recipe.write_text("version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n")
    prepared = f.cli("project", "prepare", "fabric", "--recipe", recipe, "--json", timeout=60)
    require(prepared["status"] == "READY", "real preparation did not become READY")
    ready = Path(prepared["prepared_path"])
    source_before, ready_before = fixtures.snapshot(f.source), fixtures.snapshot(ready)
    initial = f.counts()
    require(all(value == 0 for value in initial.values()), "fixture already contains admissions")
    task_path = f.path / "trusted-fake-task.json"
    task = json.loads(task_path.read_text())
    task["absent_environment"] += ["FABRIC_CONNECT_WORKER_TOKEN", "FABRIC_CONNECT_WEBHOOK_SECRET", "CONNECTION_SECRET_CANARY"]
    fixtures.save_json(task_path, task)
    f.env.update(FABRIC_CONNECT_WORKER_TOKEN=TOKEN, FABRIC_CONNECT_WEBHOOK_SECRET=SECRET,
                 CONNECTION_SECRET_CANARY=TOKEN)
    fail_results = f.path / "fail-one-results"
    fail_admission = f.path / "lose-one-admission-response"
    fabric_wrapper = f.bin / "real-fabric-with-lost-evidence"
    fixtures.executable(fabric_wrapper, f"""import json
import os
from pathlib import Path
import sys
import subprocess
args = sys.argv[1:]
if any(key.startswith('FABRIC_CONNECT_') for key in os.environ) or {TOKEN!r} in os.environ.values():
    raise SystemExit(96)
if 'spawn' in args and 'workers' in args:
    mappings = list((Path({str(f.root)!r}) / 'connect-bridge').glob('*.json'))
    if not mappings or not any(json.loads(path.read_text()).get('bridge_state') for path in mappings):
        raise SystemExit(97)
    Path({str(f.path / 'mapping-before-admission')!r}).write_text('durable')
if 'spawn' in args and Path({str(fail_admission)!r}).exists():
    Path({str(fail_admission)!r}).unlink()
    result = subprocess.run([{str(f.fabric)!r}, *args], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if result.returncode:
        raise SystemExit(99)
    raise SystemExit(100)
if 'results' in args and Path({str(fail_results)!r}).exists():
    Path({str(fail_results)!r}).unlink()
    raise SystemExit(98)
os.execv({str(f.fabric)!r}, [{str(f.fabric)!r}, *args])
""")
    consumer = Path(__file__).resolve().with_name("fabric-connect.py")

    def command(name="once", *, good=True, connection=None, **overrides):
        args = [sys.executable, consumer, name, "--connection", connection or server.connection,
                "--allow-loopback-http"]
        if name == "once":
            values = {"issue": fixtures.ISSUE, "approve": approval, "fabric": fabric_wrapper,
                      "root": f.root, "project": "fabric", "prepared-state": prepared["state_id"],
                      "harness": "fake", "allow-author": "satansdeer", "allow-label": "ready",
                      "gh": f.bin / "gh", "fake-binary": f.fake_wrapper}
            values.update(overrides)
            for key, value in values.items():
                if value is True:
                    args.append("--" + key)
                elif value is not None:
                    args += ["--" + key, str(value)]
        result = f.run(args, good=good, timeout=90)
        require(TOKEN not in json.dumps(result) and SECRET not in json.dumps(result), "connection secret leaked in CLI output")
        return result

    server.start()
    require(command("status")["events"] == [] and f.counts() == initial, "status admitted work")
    for path, token, expected in [(PREFIX + "/status", None, 401), (PREFIX + "/status", "invalid", 401),
                                   ("/v1/projects/other/fabric/automations/issues/status", TOKEN, 404)]:
        code, _ = server.request(path, token=token)
        require(code == expected, "invalid auth/route was accepted")
    raw = server.webhook(f)
    require(server.request(PREFIX + "/github", raw, signed=True, signature="sha256=" + "0" * 64)[0] == 401,
            "invalid signature was accepted")
    for changes in ({"repository": {"full_name": "other/fabric"}}, {"sender": {"login": "other"}}):
        code, body = server.request(PREFIX + "/github", server.webhook(f, **changes), signed=True)
        require(code in {200, 400, 403} and body.get("status") != "queued", "unapproved signed identity queued work")
    require(not server.view()["events"] and f.counts() == initial, "invalid ingress admitted work")
    command(good=False, **{"harness": "omp", "fake-binary": None})
    command(good=False, **{"allow-author": "other"})
    require(server.view()["worker"]["last_seen_at"] is None, "spend/route gate polled before rejecting")
    bad_connection = f.path / "bad-connection.json"
    value = json.loads(server.connection.read_text())
    value["token"] = "invalid-credential-but-at-least-32-characters"
    fixtures.save_json(bad_connection, value)
    bad_connection.chmod(0o600)
    command(good=False, connection=bad_connection)
    bad_connection.chmod(0o644)
    command("status", good=False, connection=bad_connection)
    linked = f.path / "linked-connection.json"
    linked.symlink_to(server.connection)
    command("status", good=False, connection=linked)
    value.update(token=TOKEN, endpoint=server.endpoint + "/wrong-route")
    fixtures.save_json(bad_connection, value)
    bad_connection.chmod(0o600)
    command("status", good=False, connection=bad_connection)
    require(f.counts() == initial, "auth/route/spend rejection admitted work")
    report["checks"].append("signature_auth_connection_permissions_route_and_spend_reject_without_admission")

    rejected_raw = server.webhook(f, "opened")
    require(server.request(PREFIX + "/github", rejected_raw, signed=True)[0] == 202, "approval fixture was not queued")
    rejection = command(good=False, approve="0" * 64)
    require(rejection["status"] == "failed" and server.view()["events"][0]["status"] == "failed",
            "approval mismatch was not safely rejected")
    rejected_raw = server.webhook(f, "labeled", label={"name": "ready"})
    require(server.request(PREFIX + "/github", rejected_raw, signed=True)[0] == 202, "issue rejection fixture was not queued")
    closed = copy.deepcopy(f.issue)
    closed.update(state="closed", labels=[])
    fixtures.save_json(f.issue_path, closed)
    command(good=False)
    fixtures.save_json(f.issue_path, f.issue)
    require(f.counts() == initial, "approval or live issue rejection created work")
    report["checks"].append("live_issue_and_explicit_approval_rejection_acknowledged_before_admission")

    raw = server.webhook(f, "reopened")
    event_id = hashlib.sha256(raw).hexdigest()
    code, queued = server.request(PREFIX + "/github", raw, signed=True)
    require(code == 202 and queued["event_id"] == event_id, "offline signed event was not durably queued")
    server.stop()
    server.start()
    offline = command("status")
    queued_event = next(event for event in offline["events"] if event["event_id"] == event_id)
    require(queued_event["status"] == "queued" and f.counts() == initial, "restart lost the offline queue or admitted work")
    fail_admission.touch()
    uncertain = command(good=False)
    require(uncertain.get("admission") == "uncertain" and not uncertain.get("accepted"),
            "lost admission response was not explicitly retained as uncertain")
    require(f.counts()["async_batches"] == 1, "lost response did not preserve the actual Core admission")
    uncertain_remote = next(event for event in server.view()["events"] if event["event_id"] == event_id)
    require(uncertain_remote["status"] == "delivering" and "batch_id" not in uncertain_remote,
            "uncertain admission was falsely acknowledged as running or failed")
    fail_results.touch()
    first = command(good=False, post=True)
    require(first.get("accepted") is True and first["status"] == "running" and first["event_id"] == event_id,
            "lost evidence retrieval did not preserve actual accepted handles")
    batch, run = first["batch_id"], first["run_id"]
    require((f.path / "mapping-before-admission").read_text() == "durable", "event mapping was not saved before Core admission")
    require(f.counts()["async_batches"] == 1 and f.counts()["batch_tasks"] == 1, "once did not admit exactly one real worker")
    remote = next(event for event in server.view()["events"] if event["event_id"] == event_id)
    require(remote["status"] == "running" and remote["batch_id"] == batch and remote["run_id"] == run,
            "accepted admission was incorrectly reported as completion")
    require(not f.comments(), "failed evidence lookup published a misleading comment")
    command(good=False, **{"prepared-state": "different-source"})
    fixtures.save_json(f.issue_path, closed)
    calls_before = (f.path / "gh-calls.jsonl").read_text().count('"method": "GET"')
    server.stop()
    server.start()
    connected = command(post=True)
    require(connected["status"] == "completed" and connected["batch_id"] == batch and connected["run_id"] == run,
            "reconnect did not use actual accepted terminal evidence")
    calls_after = (f.path / "gh-calls.jsonl").read_text().count('"method": "GET"')
    require(calls_after == calls_before, "accepted reconnect re-fetched a now-closed issue instead of saved evidence")
    require(f.counts()["async_batches"] == 1, "reconnect submitted duplicate work")
    report["checks"].append("offline_server_restart_durable_mapping_actual_admission_and_closed_issue_reconnect")

    results = f.cli("results", batch, "--json")
    require(results["terminal"] is True and results["succeeded"] is True
            and results["tasks"][0]["run_id"] == run, "actual Fabric terminal outcome disagrees")
    manifest = results["tasks"][0]["result"]["artifact_manifest_id"]
    require(connected["artifact_manifest_id"] == manifest, "published manifest is not actual evidence")
    artifacts = f.cli("runs", "artifacts", run, "--json")["artifacts"]
    retained = {}
    for artifact in artifacts:
        path = (f.root / artifact["relative_path"]).resolve()
        require(path.is_relative_to(f.root.resolve()), "artifact escaped owned root")
        data = path.read_bytes()
        require(len(data) == artifact["size_bytes"] and hashlib.sha256(data).hexdigest() == artifact["sha256"],
                "actual artifact content digest mismatch")
        retained[path.name] = path
    require(fixtures.TRACKED.rstrip() in retained["tracked.patch"].read_text(), "actual tracked mutation was not retained")
    with tarfile.open(retained["untracked.tar"]) as archive:
        members = [member for member in archive.getmembers() if member.name.removeprefix("./") == "new-source"]
        require(len(members) == 1 and members[0].isfile(), "actual untracked file was not retained")
        with archive.extractfile(members[0]) as stream:
            require(stream.read() == fixtures.UNTRACKED.encode(), "actual untracked content mismatch")
    require(fixtures.PRIVATE_LOG in retained["stdout.log"].read_text(), "real worker log was not captured")
    require(fixtures.snapshot(f.source) == source_before and fixtures.snapshot(ready) == ready_before,
            "original or prepared source mutated")
    require(not any(f.path.rglob("INJECTED")), "untrusted issue text was executed")
    require(len(f.comments()) == 1, "explicit evidence posting did not produce one comment")
    body = f.comments()[0]["body"]
    for identifier in (batch, run, manifest):
        require(identifier in body, "safe comment omitted actual evidence identity")
    for private in (fixtures.BODY, fixtures.PRIVATE_LOG, fixtures.TOKEN, TOKEN, SECRET, str(f.path)):
        require(private not in body, "private data leaked into posted comment")
    report["checks"].append("real_artifact_bytes_digests_immutable_sources_and_sanitized_evidence_comment")

    fixtures.save_json(f.issue_path, f.issue)
    replay_code, _ = server.request(PREFIX + "/github", raw, signed=True, delivery=str(uuid.uuid4()))
    require(replay_code == 200, "signed replay did not reuse its tombstone")
    require(command()["status"] == "idle", "completed replay was redelivered")
    new_raw = server.webhook(f, "edited")
    new_id = hashlib.sha256(new_raw).hexdigest()
    require(server.request(PREFIX + "/github", new_raw, signed=True)[0] == 202, "same-snapshot new event was not queued")
    repeated = command(post=True)
    require(repeated["event_id"] == new_id and repeated["batch_id"] == batch and repeated["run_id"] == run
            and repeated["status"] == "completed", "same approved snapshot did not reuse the actual Core run")
    require(f.counts()["async_batches"] == 1 and f.counts()["batch_tasks"] == 1 and len(f.comments()) == 1,
            "replay created duplicate admission or comment")
    for directory in (f.root / "connect-bridge", f.root / "issue-bridge"):
        require(stat.S_IMODE(directory.stat().st_mode) == 0o700, "mapping directory is not private")
        for path in directory.glob("*.json"):
            require(stat.S_IMODE(path.stat().st_mode) == 0o600, "mapping file is not private")
    for path in f.root.rglob("*"):
        if path.is_file():
            data = path.read_bytes()
            for secret in (TOKEN, SECRET, fixtures.TOKEN):
                require(secret.encode() not in data, "secret leaked into Core logs/journals/artifacts or local intent")
    hosted = server.state.read_text()
    require(all(value not in hosted for value in (TOKEN, SECRET, fixtures.BODY, fixtures.PRIVATE_LOG)),
            "hosted queue retained secrets or raw issue/worker content")
    report.update(batch_id=batch, run_id=run, artifact_manifest_id=manifest, artifact_count=len(artifacts), event_id=event_id)
    report["checks"].append("replay_and_same_snapshot_reuse_core_admission_without_credentials_in_workers_or_journals")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fabric", type=Path, required=True, help="absolute existing target/debug/fabric")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    require(args.fabric.is_absolute() and args.fabric.is_file() and os.access(args.fabric, os.X_OK),
            "--fabric must be an absolute existing executable")
    os.umask(0o077)
    fixture = server = None
    report = {"status": "FAIL", "provider_calls": 0, "checks": []}
    try:
        fixture = fixtures.Fixture(args.fabric.resolve(), args.keep)
        server = Server(fixture)
        check(fixture, server, report)
        report["status"] = "PASS"
    except (AssertionError, OSError, ValueError, KeyError, TypeError, sqlite3.Error,
            subprocess.SubprocessError, tarfile.TarError) as error:
        report["error"] = str(error) if isinstance(error, AssertionError) else type(error).__name__
        for secret in (TOKEN, SECRET, fixtures.TOKEN):
            report["error"] = report["error"].replace(secret, "[redacted]")
    finally:
        if server is not None:
            server.stop()
        if fixture is not None:
            stopped = fixture.cleanup()
            report["manager_stopped"] = stopped
            if not stopped:
                report.update(status="FAIL", error="owned manager did not stop; fixture retained")
            if fixture.keep or report["status"] != "PASS":
                report["fixture_directory"] = str(fixture.path)
                fixtures.save_json(fixture.path / "evidence.json", report)
            else:
                for directory, _, _ in os.walk(fixture.path):
                    Path(directory).chmod(0o700)
                shutil.rmtree(fixture.path)
    print(json.dumps(report, sort_keys=True))
    return 0 if report["status"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
