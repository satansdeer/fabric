#!/usr/bin/env python3
"""#11 provider-free real external SDK -> generic Connect -> Core/fake -> scoped HTTP reply.

Requires an already-built fabric and sibling fabric-fake-harness, Node >=22.18 and git.
Never calls GitHub or a native provider. Uses only temporary, owner-private state.
"""
import argparse
import copy
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
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("connect_fixture", HERE / "test-fabric-connect-agent.py")
connect = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(connect)
fixtures = connect.fixtures
BRIDGE = HERE.parent / "integrations/github/bridge.mjs"
SDK = HERE.parent / "sdk/typescript/cli.mjs"
WEBHOOK_KEY = b"provider-free-webhook-key-32-bytes-long"
REPO = "fixture-owner/fixture-repo"
ACTOR = {"id": 900, "login": "fixture-app[bot]"}


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def private_json(path, value):
    fixtures.save_json(path, value)
    path.chmod(0o600)


def event(number=11):
    return {"action": "opened", "repository": {"id": 100, "full_name": REPO}, "installation": {"id": 200},
        "sender": {"id": 300, "login": "fixture-author"}, "issue": {"number": number, "state": "open",
        "html_url": f"https://github.com/{REPO}/issues/{number}", "title": "Frozen café 😀 \x7f approval",
        "body": fixtures.BODY, "user": {"id": 300, "login": "fixture-author"}, "labels": [{"name": "ready"}]}}


def issue_data(body):
    issue = body["issue"]
    data = {"repo": REPO, "issue_number": issue["number"], "issue_url": issue["html_url"],
        "author": issue["user"]["login"], "title": issue["title"], "body": issue["body"] or ""}
    approved = {key: data[key] for key in ("repo", "issue_number", "author", "title", "body")}
    data["approval_sha256"] = hashlib.sha256(json.dumps(approved, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()).hexdigest()
    return data


def signed(body, key=WEBHOOK_KEY, delivery="11111111-1111-4111-8111-111111111111", raw=None):
    raw = compact(body) if raw is None else raw
    now = int(time.time())
    return {"body": compact({"event": "issues", "delivery": delivery, "raw": raw}), "timestamp": now,
        "expires_at": now + 120, "nonce": delivery,
        "authorization": "sha256=" + hmac.new(key, raw.encode(), hashlib.sha256).hexdigest()}


class Provider:
    """Only provider HTTP is a fixture. Admission, SDK process and harness are real."""
    def __init__(self):
        self.comments, self.deny_write, self.lose_post, self.scope_ok = {}, False, False, True
        self.calls = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def respond(self, code, value):
                raw = compact(value).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_GET(self):
                self.handle_request("GET")

            def do_POST(self):
                self.handle_request("POST")

            def do_PATCH(self):
                self.handle_request("PATCH")

            def handle_request(self, method):
                if self.headers.get("Authorization") != "Bearer " + fixtures.TOKEN:
                    return self.respond(401, {"message": "denied"})
                path = urlsplit(self.path).path
                owner.calls.append((method, path))
                if method == "GET" and path == "/installation/repositories":
                    repos = [{"id": 100, "full_name": REPO}]
                    if not owner.scope_ok:
                        repos.append({"id": 101, "full_name": "other/repo"})
                    return self.respond(200, {"total_count": len(repos), "repositories": repos})
                prefix = f"/repos/{REPO}/issues/"
                if not path.startswith(prefix):
                    return self.respond(404, {})
                tail = path[len(prefix):].split("/")
                if tail[0] == "comments" and method == "PATCH":
                    if owner.deny_write:
                        return self.respond(403, {})
                    comment = next(c for comments in owner.comments.values() for c in comments if c["id"] == int(tail[1]))
                    value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    comment["body"] = value["body"]
                    return self.respond(200, comment)
                if not tail[0].isdigit() or int(tail[0]) not in (11, 12):
                    return self.respond(404, {})
                number = int(tail[0])
                if len(tail) == 1 and method == "GET":
                    return self.respond(200, event(number)["issue"])
                if tail[1:] != ["comments"]:
                    return self.respond(404, {})
                comments = owner.comments.setdefault(number, [])
                if method == "GET":
                    return self.respond(200, comments)
                if method == "POST":
                    if owner.deny_write:
                        return self.respond(403, {})
                    value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    comment = {"id": 700 + sum(map(len, owner.comments.values())), "body": value["body"], "user": ACTOR}
                    comments.append(comment)
                    if owner.lose_post:
                        owner.lose_post = False
                        self.close_connection = True
                        return  # Committed provider write, genuinely lost HTTP response.
                    return self.respond(201, comment)
                self.respond(405, {})

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.origin = "http://127.0.0.1:" + str(self.server.server_port)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


def acceptance(f):
    node = str(Path(shutil.which("node")).resolve(strict=True))
    server, provider, agent, ingress = connect.Server(f), Provider(), None, None
    try:
        f.run(["git", "init", "--quiet", f.source], json_output=False)
        (f.source / "tracked").write_text("original\n")
        (f.source / ".gitignore").write_text(".deps/\n")
        f.run(["git", "-C", f.source, "add", "."], json_output=False)
        f.run(["git", "-C", f.source, "commit", "--quiet", "-m", "fixture"], json_output=False)
        f.cli("project", "add", "--name", "fixture", "--path", f.source, json_output=False)
        recipe = f.path / "recipe.toml"
        recipe.write_text("version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n")
        prepared = f.cli("project", "prepare", "fixture", "--recipe", recipe, "--json", timeout=60)
        assert prepared["status"] == "READY"
        original = fixtures.snapshot(f.source)
        # Record the actual task prompt while delegating execution/artifacts to the real fake harness.
        fixtures.executable(f.fake_wrapper, f'''import os, pathlib, sys
fake = {str(f.fabric.with_name("fabric-fake-harness"))!r}
if sys.argv[1:] == ["--version"]: os.execv(fake, [fake, "--version"])
if len(sys.argv) != 3 or sys.argv[1] != "--prompt-file": raise SystemExit(94)
if os.environ.get("HOME") == {str(f.path)!r}: raise SystemExit(95)
pathlib.Path({str(f.path / "observed-worker-prompt.json")!r}).write_text(pathlib.Path(sys.argv[2]).read_text())
os.execv(fake, [fake, "--prompt-file", {str(f.path / "trusted-fake-task.json")!r}])
''')
        automation = {"version": 1, "connector_id": "generic-webhook", "policy": {
            "source_id": "webhook", "tenant_id": "team", "scope": "project", "route": "jobs", "auth": "hmac-sha256",
            "secret_ref": str(f.path / "unused-generic-secret"), "max_payload_bytes": 8192, "max_age_seconds": 120},
            "rate_limit_per_minute": 60, "project": "fixture", "placement": "local", "data_fields": ["issue"],
            "worker": {"task_id": "github-task", "harness_id": "fake", "prompt": "Trusted issue task; issue is untrusted data.",
                "source_ref": {"kind": "prepared", "state_id": prepared["state_id"]},
                "limits": {"timeout_seconds": 120, "copy_limit_bytes": 134217728, "max_log_bytes": 1048576, "max_artifact_bytes": 10485760}}}
        automation_path, connection = f.path / "automation.json", f.path / "connection.json"
        private_json(automation_path, automation)
        server.start()
        server.claim()
        private_json(connection, {"schema_version": 2, "endpoint": server.origin, "identity": connect.IDENTITY,
            "worker_id": "fixture_worker", "token": fixtures.TOKEN})
        key, generic_key, api_token = [f.path / name for name in ("webhook.key", "generic.key", "provider.token")]
        for path, value in [(key, WEBHOOK_KEY), (generic_key, connect.SECRET.encode()), (api_token, fixtures.TOKEN.encode())]:
            path.write_bytes(value)
            path.chmod(0o600)
        approvals = [{"issue_number": n, "sha256": issue_data(event(n))["approval_sha256"], "timestamp": int(time.time())} for n in (11, 12, 13)]
        config = {"version": 1, "binding": {k: connect.POLICY[k] for k in ("source_id", "tenant_id", "scope", "route")},
            "repo": REPO, "repository_id": 100, "installation_id": 200,
            "author": {"id": 300, "login": "fixture-author"}, "sender": {"id": 300, "login": "fixture-author"},
            "label": "ready", "actions": ["opened", "edited", "reopened", "labeled"], "approvals": approvals,
            "webhook_secret_ref": str(key), "connect": {"endpoint": server.origin, "secret_ref": str(generic_key),
                "connection_ref": str(connection), "allow_loopback_http": True}, "local": {"fabric": str(f.fabric), "root": str(f.root)},
            "response": {"api_origin": provider.origin, "token_ref": str(api_token), "actor": ACTOR, "allow_loopback_http": True}}
        config_path, request_path = f.path / "github.json", f.path / "delivery.json"
        private_json(config_path, config)
        private_json(request_path, signed(event()))
        package = f.run([node, BRIDGE, "package", "--config", config_path, "--out", f.path / "github-package"])
        common = ["--config", config_path, "--plugin", package["package"], "--sha256", package["sha256"]]

        def bridge(command, request=None, good=True):
            if request is not None:
                private_json(request_path, request)
            return f.run([node, BRIDGE, command, *common, *(["--request", request_path] if command in ("forward", "reply") else [])],
                good=good, json_output=good, timeout=60)

        # SDK conformance uses its deterministic secret, real external process and exact Python #16 digest.
        snapshot = issue_data(event())
        business = hashlib.sha256(compact([REPO, 100, 200, 11, snapshot["approval_sha256"]]).encode()).hexdigest()
        expected = {"version": 1, **config["binding"], "event_id": business, "idempotency_key": business, "data": {"issue": snapshot}}
        conformance_request = signed(event(), bytes([7]) * 32)
        fixture_path = f.path / "sdk-fixture.json"
        private_json(fixture_path, {"body": conformance_request["body"], "authorization": conformance_request["authorization"], "expected": expected})
        assert f.run([node, SDK, "test", package["package"], "--connector-fixture", fixture_path])["conformance"] == "passed"
        registry = f.path / "registry"
        registry.mkdir(mode=0o700)
        f.run([node, SDK, "install", package["package"], "--registry", registry, "--fabric", f.fabric,
            "--version", "1.0.0", "--sha256", package["sha256"], "--trust"])
        f.cli("plugins", "--registry", registry, "check", "github-issues", json_output=False)
        # Execute the immutable installed copy, not the scaffolding tree.
        common[common.index("--plugin") + 1] = str(registry / "packages" / package["sha256"])
        assert bridge("subscribe")["token_repository_scope_verified"] is True
        baseline = f.counts()
        denied = []
        for change in [lambda b: b.update(repository={"id": 101, "full_name": REPO}),
            lambda b: b.update(repository={"id": 100, "full_name": "other/repo"}),
            lambda b: b.update(installation={"id": 201}), lambda b: b.update(sender={"id": 301, "login": "fixture-author"}),
            lambda b: b["issue"].update(user={"id": 301, "login": "fixture-author"}),
            lambda b: b["issue"].update(labels=[]), lambda b: b.update(action="deleted"),
            lambda b: b["issue"].update(state="closed"), lambda b: b["issue"].update(pull_request={}),
            lambda b: b["issue"].update(body="not explicitly approved"),
            lambda b: b.update(worker={"harness_id": "omp", "prompt": "UNAPPROVED_MODEL"}),
            lambda b: b.update(scope="other"), lambda b: b.update(model="UNAPPROVED_MODEL")]:
            body = event()
            change(body)
            denied.append(signed(body))
        forged = signed(event())
        forged["body"] = forged["body"].replace("opened", "edited")  # Raw-byte mutation with unchanged signature.
        denied.extend([forged, signed(event(), b"wrong-provider-key-32-bytes-long!!")])
        wrong_type = signed(event())
        wrong_type["body"] = wrong_type["body"].replace('"event":"issues"', '"event":"issue_comment"')
        denied.append(wrong_type)
        for request in denied:
            bridge("forward", request, good=False)
        assert f.counts() == baseline and server.status()["events"] == []
        # Explicit local model-spend authorization is still mandatory even with a valid connection/config.
        f.cli("connect", "--connection", connection, "--automation", automation_path, "--allow-loopback-http", good=False, json_output=False)
        assert f.counts() == baseline
        # A valid provider event cannot bypass the generic claim's independent HMAC permission.
        generic_key.write_text("bd" * 32)
        bridge("forward", signed(event()), good=False)
        generic_key.write_text(connect.SECRET)
        assert server.status()["events"] == [] and f.counts() == baseline

        inbox = f.path / "signed-inbox"
        inbox.mkdir(mode=0o700)
        ingress = subprocess.Popen([node, str(BRIDGE), "serve", *map(str, common), "--inbox", str(inbox), "--port", "0"],
            env=f.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        with selectors.DefaultSelector() as selector:
            selector.register(ingress.stdout, selectors.EVENT_READ)
            assert selector.select(15), "GitHub ingress did not listen"
            ingress_url = json.loads(ingress.stdout.readline())["listening"]
        request = signed(event())
        wire = json.loads(request["body"])
        http_request = urllib.request.Request(ingress_url, data=wire["raw"].encode(), headers={
            "x-github-event": "issues", "x-github-delivery": wire["delivery"], "x-hub-signature-256": request["authorization"]})
        with urllib.request.urlopen(http_request, timeout=15) as response:
            admitted = json.load(response)
            assert response.status == 202 and admitted["status"] == "queued"
        assert f.counts() == baseline  # Hosted acceptance has not run a worker.
        saved = inbox / (business + ".json")
        assert saved.exists()
        bridge("reply", private_json_value(saved), good=False)
        assert not provider.comments
        agent = subprocess.Popen([str(f.fabric), "--root", str(f.root), "connect", "--connection", str(connection),
            "--automation", str(automation_path), "--fake-binary", str(f.fake_wrapper), "--allow-loopback-http"],
            env=f.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        def terminal(event_id):
            return connect.wait_for(lambda: next((e for e in server.status()["events"] if e["event_id"] == event_id and e["status"] in ("completed", "failed")), None), "real fake task did not reach terminal receipt")
        completed = terminal(admitted["event_id"])
        assert completed["status"] == "completed"
        counts = f.counts()
        for action in ("labeled", "reopened", "edited", "opened"):
            body = event()
            body["action"] = action
            body["label"] = {"name": "ready"}
            result = bridge("forward", signed(body, delivery="22222222-2222-4222-8222-222222222222"))
            assert result["duplicate"] is True and result["event_id"] == admitted["event_id"]
        assert f.counts() == counts
        results = f.cli("results", completed["batch_id"], "--json")
        task = results["tasks"][0]
        assert results["terminal"] and results["succeeded"] and task["run_id"] == completed["run_id"]
        retained = {}
        for artifact in task["artifacts"]:
            path = f.root / artifact["relative_path"]
            assert path.resolve().is_relative_to(f.root)
            content = path.read_bytes()
            assert len(content) == artifact["size_bytes"] and hashlib.sha256(content).hexdigest() == artifact["sha256"]
            retained[path.name] = path
        assert fixtures.TRACKED.rstrip().encode() in retained["tracked.patch"].read_bytes()
        with tarfile.open(retained["untracked.tar"]) as archive:
            member = next(m for m in archive.getmembers() if m.name.removeprefix("./") == "new-source")
            assert archive.extractfile(member).read() == fixtures.UNTRACKED.encode()
        prompt = (f.path / "observed-worker-prompt.json").read_text()
        assert "Trusted issue task" in prompt and snapshot["approval_sha256"] in prompt and "UNAPPROVED_MODEL" not in prompt
        assert fixtures.snapshot(f.source) == original
        # Independent provider denial cannot create work or an optimistic success response.
        provider.scope_ok = False
        bridge("reply", signed(event()), good=False)
        provider.scope_ok = True
        provider.deny_write = True
        bridge("reply", signed(event()), good=False)
        assert not provider.comments.get(11) and f.counts() == counts
        provider.deny_write, provider.lose_post = False, True
        bridge("reply", signed(event()), good=False)
        assert len(provider.comments[11]) == 1
        reply = bridge("reply", signed(event()))
        assert reply["duplicate"] is True and reply["batch_id"] == completed["batch_id"] and reply["run_id"] == completed["run_id"]
        assert reply["artifact_manifest_id"] == completed["artifact_manifest_id"]
        comment = provider.comments[11][0]
        assert "succeeded: true" in comment["body"] and completed["artifact_manifest_id"] in comment["body"]
        assert fixtures.BODY not in comment["body"] and fixtures.PRIVATE_LOG not in comment["body"] and fixtures.TOKEN not in comment["body"]
        comment["user"] = {"id": 901, "login": ACTOR["login"]}
        bridge("reply", signed(event()), good=False)
        comment["user"] = ACTOR
        # Failed worker outcomes retain real artifacts and must be reported as failure, never success.
        fake_task = f.path / "trusted-fake-task.json"
        task_spec = json.loads(fake_task.read_text())
        task_spec["exit_code"] = 7
        fixtures.save_json(fake_task, task_spec)
        second = bridge("forward", signed(event(12)))
        failed = terminal(second["event_id"])
        assert failed["status"] == "failed"
        failure_reply = bridge("reply", signed(event(12)))
        assert failure_reply["run_id"] == failed["run_id"] and "succeeded: false" in provider.comments[12][0]["body"]
        # Unmodified generic ingress still works on the SAME claim/native agent, without provider fields.
        task_spec["exit_code"] = 0
        fixtures.save_json(fake_task, task_spec)
        code, generic = server.webhook(compact({"version": 1, "event_id": "plain-generic", "timestamp": int(time.time()), "data": {"title": "generic remains generic"}}).encode())
        assert code == 202 and terminal(generic["event_id"])["status"] == "completed"
        assert len(server.status()["events"]) == 3 and len(provider.comments[11]) == 1 and len(provider.comments[12]) == 1
        assert not (f.path / "forbidden-fabric-call").exists() and not (f.source / "INJECTED").exists()
        # Even an authenticated hosted consumer cannot substitute another real run's handles.
        connect.stop(agent)
        agent = None
        identity_paths = list((f.root / "connect").glob("*/identity.json"))
        assert len(identity_paths) == 1
        local_id = private_json_value(identity_paths[0])["local_id"]
        forged_receipt = bridge("forward", signed(event(13)))
        def consumer(suffix, **fields):
            code, result = server.request(connect.PREFIX + suffix,
                {"worker_id": "fixture_worker", "local_id": local_id, **fields})
            assert code == 200, (code, result)
            return result
        consumer("/heartbeat", online=True)
        lease = consumer("/poll", event_id=forged_receipt["event_id"])["event"]
        handles = {key: completed[key] for key in ("batch_id", "run_id")}
        consumer("/events/" + lease["event_id"] + "/ack", lease_id=lease["lease_id"], status="running", **handles)
        consumer("/events/" + lease["event_id"] + "/ack", lease_id=lease["lease_id"], status="completed",
            artifact_manifest_id=completed["artifact_manifest_id"], **handles)
        before_forged_reply, calls = f.counts(), len(provider.calls)
        bridge("reply", signed(event(13)), good=False)
        assert f.counts() == before_forged_reply and len(provider.calls) == calls
        print(json.dumps({"check": "issue11-github-external", "batch_id": completed["batch_id"], "run_id": completed["run_id"],
            "artifact_manifest_id": completed["artifact_manifest_id"], "failed_run_id": failed["run_id"], "native_provider_calls": 0}))
    finally:
        connect.stop(agent)
        connect.stop(ingress)
        connect.stop(server.process)
        provider.close()


def private_json_value(path):
    return json.loads(path.read_text())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fabric", required=True, type=Path)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--bundle", type=Path, help="Exercise connector and SDK from an extracted release instead of the checkout")
    args = parser.parse_args()
    if args.bundle:
        global BRIDGE, SDK
        bundle = args.bundle.resolve(strict=True)
        BRIDGE, SDK = bundle / "integrations/github/bridge.mjs", bundle / "sdk/cli.mjs"
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
