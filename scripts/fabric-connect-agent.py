#!/usr/bin/env python3
"""Outbound generic Connect transport, embedded in the native CLI; Python standard library only."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import selectors
import signal
import stat
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

LIMIT = 1024 * 1024
HEX = re.compile(r"[0-9a-f]{64}\Z")
STOP = threading.Event()


class Rejected(Exception):
    pass


class Disconnected(Exception):
    pass


def encode(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def private_json(path, limit=65536):
    path = Path(path)
    if not path.is_absolute() or path.resolve() != path:
        raise Rejected("configuration path must be canonical and absolute")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_nlink != 1:
            raise Rejected("configuration must be an owner-only regular file")
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise Rejected("configuration exceeds its bound")
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise Rejected("configuration must be a JSON object")
    return value, raw


def directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if path.resolve() != path or not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise Rejected("storage must be canonical, private and owner-controlled")


def persist(path, value):
    raw = encode(value)
    if len(raw) > 65536:
        raise Rejected("local delivery journal exceeds its bound")
    temporary = path.with_name("." + path.name + "." + secrets.token_hex(8))
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    finally:
        temporary.unlink(missing_ok=True)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


class Client:
    def __init__(self, path, loopback):
        self.config, _ = private_json(path)
        config = self.config
        if set(config) != {"schema_version", "endpoint", "identity", "worker_id", "token"} or config["schema_version"] != 2:
            raise Rejected("Connect requires a scoped schema-2 connection")
        identity = config["identity"]
        if not isinstance(identity, dict) or set(identity) != {"account", "project", "automation"} or not all(
                isinstance(value, str) and re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,62}", value) for value in identity.values()):
            raise Rejected("invalid claimed identity")
        if not isinstance(config["endpoint"], str):
            raise Rejected("invalid endpoint")
        url = urllib.parse.urlsplit(config["endpoint"])
        local = loopback and url.scheme == "http" and url.hostname in ("127.0.0.1", "::1")
        if (url.scheme != "https" and not local) or not url.hostname or url.username or url.password or url.path not in ("", "/") or url.query or url.fragment:
            raise Rejected("Connect requires an HTTPS origin (literal loopback HTTP is test-only)")
        if not isinstance(config["worker_id"], str) or not re.fullmatch(r"[A-Za-z0-9_-]{8,100}", config["worker_id"]) or not isinstance(config["token"], str) or not re.fullmatch(r"[!-~]{32,512}", config["token"]):
            raise Rejected("invalid scoped credential")
        self.origin = config["endpoint"].rstrip("/")
        self.prefix = "/v1/projects/{account}/{project}/automations/{automation}".format(**identity)
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        self.local_id = None

    def request(self, suffix, body=None):
        if body is not None:
            body = dict(body, worker_id=self.config["worker_id"], local_id=self.local_id)
        request = urllib.request.Request(self.origin + self.prefix + suffix,
            data=None if body is None else encode(body), headers={"Authorization": "Bearer " + self.config["token"], "Content-Type": "application/json"})
        try:
            with self.opener.open(request, timeout=15) as response:
                raw = response.read(LIMIT + 1)
                if response.status != 200 or len(raw) > LIMIT:
                    raise Rejected("invalid bounded Connect response")
                result = json.loads(raw)
                if not isinstance(result, dict):
                    raise Rejected("invalid Connect response")
                return result
        except urllib.error.HTTPError as failure:
            if failure.code in (400, 401, 403, 404, 413) or (failure.code == 409 and suffix == "/heartbeat") or 300 <= failure.code < 400:
                raise Rejected("Connect rejected authentication, identity or protocol") from None
            raise Disconnected() from None
        except (urllib.error.URLError, TimeoutError, OSError, ValueError):
            raise Disconnected() from None


def cli(args, client, *command, payload=None):
    # Never propagate transport credentials to a manager, plugin, harness or worker.
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("FABRIC_CONNECT") and client.config["token"] not in value}
    process = subprocess.Popen([str(args.fabric), "--root", str(args.root), *map(str, command)],
        stdin=subprocess.PIPE if payload is not None else subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
    output = bytearray()
    try:
        if payload is not None:
            process.stdin.write(payload if isinstance(payload, bytes) else encode(payload))
            process.stdin.close()
        deadline = time.monotonic() + 30
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while time.monotonic() < deadline:
                if not selector.select(min(0.2, max(0, deadline - time.monotonic()))):
                    continue
                chunk = os.read(process.stdout.fileno(), min(65536, LIMIT + 1 - len(output)))
                if not chunk:
                    break
                output.extend(chunk)
                if len(output) > LIMIT:
                    raise Rejected("local manager response exceeds its bound")
            else:
                raise Disconnected()
        if process.wait(timeout=max(0.1, deadline - time.monotonic())) != 0:
            raise Rejected("local manager rejected delivery; no acknowledgement sent")
        value = json.loads(output)
        if not isinstance(value, dict):
            raise Rejected("invalid local manager response")
        return value
    except (subprocess.TimeoutExpired, BrokenPipeError, ValueError):
        # Admission may already have committed. The durable receipt key resolves ambiguity.
        raise Disconnected() from None
    finally:
        process.stdout.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def validate_automation(automation, client):
    policy = automation.get("policy", {})
    identity = client.config["identity"]
    if (policy.get("tenant_id"), policy.get("scope"), policy.get("route")) != (identity["account"], identity["project"], identity["automation"]):
        raise Rejected("local route does not match claimed identity")
    worker = automation.get("worker", {})
    source = worker.get("source_ref", {})
    limits = worker.get("limits", {})
    if automation.get("placement") != "local" or source.get("kind") != "prepared" or not source.get("state_id"):
        raise Rejected("Connect requires a pinned prepared local source")
    for field, ceiling in (("timeout_seconds", 604800), ("copy_limit_bytes", 134217728), ("max_log_bytes", 134217728), ("max_artifact_bytes", 134217728)):
        number = limits.get(field)
        if type(number) is not int or not 1 <= number <= ceiling:
            raise Rejected("Connect requires explicit bounded resource limits")
    return {"kind": "generic", **{field: policy.get(field) for field in ("source_id", "tenant_id", "scope", "route", "max_payload_bytes")}}


def validate_event(event, expected):
    raw = event.get("trigger_json")
    if not isinstance(raw, str) or len(raw.encode("utf-8")) > 8192:
        raise Rejected("invalid normalized delivery bytes")
    trigger = json.loads(raw)
    if trigger != event.get("trigger"):
        raise Rejected("normalized delivery bytes changed")
    if event.get("source") != "generic" or not HEX.fullmatch(event.get("event_id", "")) or set(trigger) != {"version", "source_id", "tenant_id", "scope", "route", "event_id", "idempotency_key", "data"}:
        raise Rejected("invalid normalized delivery")
    if trigger.get("version") != 1 or any(trigger.get(key) != expected[key] for key in ("source_id", "tenant_id", "scope", "route")) or trigger.get("event_id") != event["event_id"] or trigger.get("idempotency_key") != event["event_id"] or not isinstance(trigger.get("data"), dict):
        raise Rejected("delivery exceeds local route authority")


def emit(state, **fields):
    print(json.dumps({"local_status": state, **fields}), flush=True)


def deliver(args, client, expected, snapshot, pending_path, pending):
    query = {} if pending is None else {"event_id": pending["event"]["event_id"]}
    event = client.request("/poll", query).get("event")
    if event is None:
        if pending is not None:
            status = client.request("/status")
            if any(item.get("event_id") == pending["event"]["event_id"] for item in status.get("events", [])):
                return pending  # A leased delivery may temporarily be queued behind backoff.
            # Hosted terminal receipts have finite retention; Core evidence and identity do not.
            handles = pending.get("handles")
            if not handles:
                raise Rejected("hosted receipt is missing for unresolved admission")
            results = cli(args, client, "results", handles["batch_id"], "--json")
            tasks = results.get("tasks", [])
            if results.get("terminal") is not True or len(tasks) != 1 or tasks[0].get("run_id") != handles["run_id"]:
                raise Rejected("missing hosted receipt has no matching terminal Core evidence")
            persist(pending_path, {})
            emit("local online", event_id=pending["event"]["event_id"],
                 status="completed" if results.get("succeeded") is True else "failed", **handles)
            return None
        return pending
    validate_event(event, expected)
    if pending is not None and pending["event"]["trigger_json"] != event["trigger_json"]:
        raise Rejected("durable receipt content changed")
    if event["status"] in ("completed", "failed", "expired"):
        if pending is not None:
            handles = pending.get("handles")
            if not handles or any(event.get(key) != value for key, value in handles.items()):
                raise Rejected("terminal receipt does not match durable admission")
            persist(pending_path, {})
        return None
    if pending is None:
        pending = {"event": event}
        persist(pending_path, pending)  # Intent is durable before any side effect can be admitted.
    if "handles" not in pending:
        command = ["connect-admit", "--automation", snapshot]
        if args.fake_binary:
            command.extend(("--fake-binary", args.fake_binary))
        accepted = cli(args, client, *command, payload=event["trigger_json"].encode("utf-8"))
        tasks = accepted.get("tasks", [])
        if accepted.get("accepted") is not True or len(tasks) != 1:
            raise Rejected("manager did not return atomic admission handles")
        handles = {"batch_id": accepted["batch_id"], "run_id": tasks[0]["run_id"]}
        if event["status"] == "running" and any(event.get(key) != value for key, value in handles.items()):
            raise Rejected("hosted receipt belongs to a different local admission")
        pending["handles"] = handles
        persist(pending_path, pending)
        emit("local online", event_id=event["event_id"], **handles)
    handles = pending["handles"]
    if event["status"] == "delivering":
        client.request("/events/" + event["event_id"] + "/ack", dict(handles, lease_id=event["lease_id"], status="running"))
    elif event["status"] != "running" or any(event.get(key) != value for key, value in handles.items()):
        raise Rejected("invalid receipt lease or admission mapping")
    results = cli(args, client, "results", handles["batch_id"], "--json")
    if results.get("terminal") is not True:
        return pending
    tasks = results.get("tasks", [])
    if len(tasks) != 1 or tasks[0].get("run_id") != handles["run_id"]:
        raise Rejected("manager results do not match admission")
    receipt = dict(handles, lease_id=event["lease_id"], status="completed" if results.get("succeeded") is True else "failed")
    manifest = (tasks[0].get("result") or {}).get("artifact_manifest_id")
    if manifest:
        receipt["artifact_manifest_id"] = manifest
    client.request("/events/" + event["event_id"] + "/ack", receipt)
    persist(pending_path, {})  # Only discard local intent after the hosted terminal commit replies.
    emit("local online", event_id=event["event_id"], status=receipt["status"], **handles)
    return None


def run(args):
    args.fabric = args.fabric.resolve(strict=True)
    if args.fake_binary:
        args.fake_binary = args.fake_binary.resolve(strict=True)
    elif not args.allow_model_spend:
        raise Rejected("explicit local execution authority is required")
    client = Client(args.connection, args.allow_loopback_http)
    automation, _ = private_json(args.automation)
    if args.fake_binary and automation.get("worker", {}).get("harness_id") not in ("fake", "fake-a", "fake-b"):
        raise Rejected("--fake-binary requires a local fake harness policy")
    expected = validate_automation(automation, client)
    args.root = args.root.absolute()
    directory(args.root)
    directory(args.root / "connect")
    key = hashlib.sha256(encode([client.origin, client.config["identity"]])).hexdigest()
    local = args.root / "connect" / key
    directory(local)
    lock_fd = os.open(local / "lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(lock_fd, "rb") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise Rejected("this local connection is already active") from None
        digest = hashlib.sha256(encode([automation, str(args.fake_binary), args.allow_model_spend])).hexdigest()
        identity_path = local / "identity.json"
        if identity_path.exists():
            identity, _ = private_json(identity_path)
            if identity.get("configuration_sha256") != digest or not HEX.fullmatch(identity.get("local_id", "")):
                raise Rejected("connection policy changed; retain the admitted policy and journal")
        else:
            identity = {"local_id": secrets.token_hex(32), "configuration_sha256": digest}
            persist(identity_path, identity)
        client.local_id = identity["local_id"]
        snapshot = local / "automation.json"
        persist(snapshot, automation)
        pending_path = local / "pending.json"
        pending = private_json(pending_path)[0] if pending_path.exists() else {}
        pending = pending or None
        backoff = 1
        authenticated = False
        try:
            while not STOP.is_set():
                try:
                    if not authenticated:
                        status = client.request("/status")
                        if status.get("policy") != expected or status.get("worker", {}).get("id") != client.config["worker_id"]:
                            raise Rejected("hosted policy does not match trusted local automation")
                        client.request("/heartbeat", {"online": True})
                        authenticated = True
                        emit("local online", endpoint=client.origin + client.prefix)
                    client.request("/heartbeat", {"online": True})
                    pending = deliver(args, client, expected, snapshot, pending_path, pending)
                    backoff = 1
                    STOP.wait(5)
                except Disconnected:
                    authenticated = False
                    # Reload intent: an exception may follow a durable write or a committed admission.
                    pending = private_json(pending_path)[0] if pending_path.exists() else {}
                    pending = pending or None
                    emit("local offline")
                    STOP.wait(backoff)
                    backoff = min(30, backoff * 2)
        finally:
            try:
                client.request("/heartbeat", {"online": False})
            except (Rejected, Disconnected):
                pass  # Server independently marks a missing heartbeat offline after 60 seconds.
            emit("local offline")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("fabric", "root", "connection", "automation"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--fake-binary", type=Path)
    parser.add_argument("--allow-model-spend", action="store_true")
    parser.add_argument("--allow-loopback-http", action="store_true")
    args = parser.parse_args()
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: STOP.set())
    try:
        run(args)
        return 0
    except (Rejected, OSError, ValueError, TypeError, KeyError):
        # Never echo webhook data, credentials, child stderr or server bodies.
        print('{"error":"Connect stopped: invalid private policy, identity or local admission; no success acknowledgement"}', file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
