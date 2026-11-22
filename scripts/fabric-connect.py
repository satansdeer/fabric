#!/usr/bin/env python3
"""Explicit outbound-only consumer for the owner-provisioned Fabric GitHub inbox."""

import argparse
import contextlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.parse
import urllib.request


spec = importlib.util.spec_from_file_location("fabric_issue", Path(__file__).with_name("fabric-issue.py"))
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)
PREFIX = "/v1/projects/satansdeer/fabric/automations/issues"
HEX = re.compile(r"[0-9a-f]{64}\Z")
WORKER = re.compile(r"[A-Za-z0-9_-]{8,100}\Z")
UUID = re.compile(r"[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\Z")
STATES = {"queued", "delivering", "running", "completed", "failed", "expired"}


class ConnectError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise ConnectError(message)


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise ConnectError("Invalid command arguments; use --help.")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, new_url):
        raise ConnectError("Connection redirects are not allowed.")


class Client:
    def __init__(self, args):
        # Do not resolve the final component: open_private rejects symlinks.
        path = Path(args.connection)
        self.connection = bridge.read_state(path)
        value = self.connection
        require(set(value) == {"schema_version", "endpoint", "worker_id", "token"}
                and type(value.get("schema_version")) is int and value["schema_version"] == 1,
                "Invalid connection configuration.")
        endpoint = value.get("endpoint")
        require(isinstance(endpoint, str), "Invalid connection endpoint.")
        parsed = urllib.parse.urlsplit(endpoint)
        origin = (bool(parsed.hostname) and parsed.path in {"", "/"}
                  and not parsed.query and not parsed.fragment
                  and parsed.username is None and parsed.password is None
                  and all(33 <= ord(char) <= 126 for char in endpoint))
        loopback = (args.allow_loopback_http and parsed.scheme == "http"
                    and parsed.hostname in {"127.0.0.1", "::1"})
        require(origin and (parsed.scheme == "https" or loopback),
                "Connection endpoint must be an owner-configured HTTPS origin.")
        require(isinstance(value.get("worker_id"), str) and WORKER.fullmatch(value["worker_id"]),
                "Invalid connection worker identity.")
        token = value.get("token")
        require(isinstance(token, str) and 32 <= len(token) <= 512
                and all(33 <= ord(char) <= 126 for char in token), "Invalid connection credential.")
        self.endpoint = endpoint.rstrip("/")
        self.worker_id = value["worker_id"]
        self.token = token
        # Never send owner credentials through a proxy selected by ambient environment.
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def request(self, suffix, payload=None):
        data = None if payload is None else bridge.canonical({"worker_id": self.worker_id, **payload}).encode()
        request = urllib.request.Request(
            self.endpoint + PREFIX + suffix, data=data,
            headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json"},
            method="GET" if payload is None else "POST")
        try:
            with self.opener.open(request, timeout=15) as response:
                require(response.status == 200, "Connection request was not accepted.")
                raw = response.read(bridge.MAX_JSON + 1)
            require(len(raw) <= bridge.MAX_JSON, "Connection response exceeded the size limit.")
            value = json.loads(raw)
            require(isinstance(value, dict), "Invalid connection response.")
            return value
        except (urllib.error.URLError, OSError, ValueError):
            raise ConnectError("Connection request failed; remote response withheld. Repeat the same command to reconcile saved intent.") from None

    def ack(self, event, status, handles=None, reason=None):
        payload = {"lease_id": event["lease_id"], "status": status}
        if handles:
            payload.update(handles)
        if reason:
            payload["reason"] = reason
        self.request("/events/" + event["event_id"] + "/ack", payload)


def safe_id(value):
    require(isinstance(value, str) and bridge.TOKEN.fullmatch(value), "Invalid remote evidence identity.")
    return value


def event_record(value, issue=None, leased=False):
    require(isinstance(value, dict) and value.get("schema_version") == 1
            and value.get("source") == "github" and value.get("repo") == bridge.REPO
            and isinstance(value.get("event_id"), str) and HEX.fullmatch(value["event_id"])
            and type(value.get("issue_number")) is int and 0 < value["issue_number"] <= 2147483647
            and (issue is None or value["issue_number"] == issue)
            and value.get("action") in {"opened", "edited", "reopened", "labeled"}
            and isinstance(value.get("delivery_id"), str) and UUID.fullmatch(value["delivery_id"])
            and value.get("status") in STATES, "Invalid remote event identity or state.")
    output = {key: value[key] for key in ("schema_version", "source", "event_id", "repo", "issue_number",
                                        "action", "delivery_id", "status")}
    if leased:
        require(value["status"] in {"delivering", "running"}, "Remote event is not dispatchable.")
        output["lease_id"] = safe_id(value.get("lease_id"))
    for name in ("batch_id", "run_id", "artifact_manifest_id"):
        if value.get(name) is not None:
            output[name] = safe_id(value[name])
    require(("batch_id" in output) == ("run_id" in output), "Incomplete remote evidence mapping.")
    if output["status"] in {"running", "completed"}:
        require("batch_id" in output, "Remote state lacks accepted evidence.")
    if output["status"] in {"queued", "delivering", "expired"}:
        require(not any(key in output for key in ("batch_id", "run_id", "artifact_manifest_id")),
                "Remote unadmitted state contains evidence handles.")
    return output


def worker_record(value, client):
    require(isinstance(value, dict) and value.get("id") == client.worker_id
            and type(value.get("online")) is bool, "Invalid remote worker identity.")
    timestamp = value.get("last_seen_at")
    require(timestamp is None or (isinstance(timestamp, str) and re.fullmatch(
        r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z", timestamp)), "Invalid remote heartbeat.")
    return {"id": client.worker_id, "online": value["online"], "last_seen_at": timestamp}


def status(client):
    value = client.request("/status")
    require(value.get("schema_version") == 1 and value.get("owner") == "satansdeer"
            and value.get("project") == "fabric" and value.get("automation") == "issues"
            and isinstance(value.get("events"), list) and len(value["events"]) <= 256,
            "Invalid remote status identity.")
    events = [event_record(event) for event in value["events"]]
    return {"schema_version": 1, "owner": "satansdeer", "project": "fabric", "automation": "issues",
            "worker": worker_record(value.get("worker"), client), "events": events,
            "counts": {state: sum(event["status"] == state for event in events) for state in sorted(STATES)}}


@contextlib.contextmanager
def subprocess_environment(client):
    # gh may use its own local credential; connection credentials never reach gh/Fabric.
    removed = {key: value for key, value in os.environ.items()
               if key.startswith("FABRIC_CONNECT_") or client.token in value}
    try:
        for key in removed:
            del os.environ[key]
        yield
    finally:
        os.environ.update(removed)


def configuration(args, client):
    require(args.harness == "fake" or args.allow_model_spend,
            "Native workers require --allow-model-spend and prior Fabric native consent.")
    require(args.fake_binary is None or args.harness == "fake", "Fake executable requires the fake harness.")
    require(type(args.issue) is int and 0 < args.issue <= 2147483647
            and args.allow_author == "satansdeer" and args.allow_label == "ready"
            and args.project == "fabric" and HEX.fullmatch(args.approve),
            "Issue, approval or owner route is not allowed.")
    bridge.token(args.prepared_state, "prepared state ID")
    args.root = str(bridge.absolute(args.root, "--root"))
    args.fabric = str(bridge.absolute(args.fabric, "--fabric"))
    if args.fake_binary:
        args.fake_binary = str(bridge.absolute(args.fake_binary, "--fake-binary"))
    return {"endpoint": client.endpoint, "worker_id": client.worker_id, "issue_number": args.issue,
            "approval_sha256": args.approve, "root": args.root, "fabric": args.fabric,
            "project": args.project, "source": {"kind": "prepared", "state_id": args.prepared_state},
            "harness": args.harness, "fake_binary": args.fake_binary,
            "allow_author": args.allow_author, "allow_label": args.allow_label}


# Only explicit live-issue validation failures can reject an unadmitted delivery.
ISSUE_REJECTIONS = {
    "GitHub issue number mismatch.", "Pull requests are not accepted as issues.",
    "The issue must be open.", "GitHub issue URL mismatch.", "Issue author is not allowed.",
    "Invalid issue labels.", "Invalid issue label metadata.", "Required issue label is missing.",
    "Invalid or oversized issue label.", "Invalid or oversized issue title.", "Invalid or oversized issue body.",
}


def approved_snapshot(args, client, event, output, can_reject):
    try:
        snapshot = bridge.issue_snapshot(args)
    except bridge.BridgeError as error:
        if can_reject and str(error) in ISSUE_REJECTIONS:
            client.ack(event, "failed", reason="issue_rejected")
            output["status"] = "failed"
        raise
    if snapshot["approval_sha256"] != args.approve:
        if can_reject:
            client.ack(event, "failed", reason="approval_mismatch")
            output["status"] = "failed"
        raise ConnectError("Live issue approval mismatch; no new admission was attempted.")
    return snapshot


def mapped_state(mapping, config):
    key = mapping.get("idempotency_key")
    require(isinstance(key, str) and re.fullmatch(r"issue-[0-9a-f]{64}", key), "Invalid saved bridge identity.")
    path = Path(config["root"]) / "issue-bridge" / (key + ".json")
    require(mapping.get("bridge_state") == str(path), "Saved bridge path mismatch.")
    if not path.exists():
        require(mapping.get("accepted") is None, "Saved accepted bridge evidence is unavailable; do not resubmit.")
        return path, None
    state = bridge.read_state(path)
    snapshot = state.get("snapshot")
    require(isinstance(snapshot, dict) and snapshot.get("approval_sha256") == config["approval_sha256"]
            and snapshot.get("issue_number") == config["issue_number"] and snapshot.get("repo") == bridge.REPO
            and snapshot.get("author") == config["allow_author"]
            and state.get("root") == config["root"] and state.get("fake_binary") == config["fake_binary"]
            and state.get("request") == bridge.worker_request(snapshot, config["project"],
                                                               config["source"]["state_id"], config["harness"])
            and bridge.identity(snapshot, state["request"], config["fake_binary"]) == key
            and state.get("idempotency_key") == key, "Saved bridge provenance mismatch.")
    return path, state


def save_handles(mapping, path, state, output):
    batch, runs = bridge.accepted_handles(state["accepted"], state["request"])
    handles = {"batch_id": batch, "run_id": runs[0]}
    require(mapping.get("accepted") in (None, handles), "Saved accepted mapping changed.")
    mapping["accepted"] = handles
    bridge.save_state(path, mapping)
    output.update(handles, accepted=True, status="running")
    output.pop("admission", None)
    return handles


def once(args, client, output):
    config = configuration(args, client)  # Spend gate before even polling.
    response = client.request("/poll", {"issue_number": args.issue})
    worker_record(response.get("worker"), client)
    require("event" in response, "Invalid remote poll response.")
    if response.get("event") is None:
        output.update(status="idle", event_id=None)
        return
    event = event_record(response["event"], args.issue, leased=True)
    output.update(event_id=event["event_id"], status=event["status"])
    directory = Path(args.root) / "connect-bridge"
    bridge.private_directory(directory)
    path = directory / (event["event_id"] + ".json")
    provenance = {key: event[key] for key in ("schema_version", "source", "event_id", "repo", "issue_number", "action", "delivery_id")}
    # ponytail: one local lock serializes this bounded route; use per-event locks for multiple routes.
    with bridge.locked(directory / "route.json"), subprocess_environment(client):
        if path.exists():
            mapping = bridge.read_state(path)
            require(mapping.get("configuration") == config and mapping.get("event") == provenance,
                    "Saved event configuration differs; repeat the original command to reconnect.")
        else:
            require(event["status"] != "running", "Remote running event has no local mapping; recover original private state, do not resubmit.")
            snapshot = approved_snapshot(args, client, event, output, can_reject=True)
            request = bridge.worker_request(snapshot, args.project, args.prepared_state, args.harness)
            key = bridge.identity(snapshot, request, args.fake_binary)
            mapping = {"schema_version": 1, "configuration": config, "event": provenance,
                       "idempotency_key": key, "bridge_state": str(Path(args.root) / "issue-bridge" / (key + ".json")),
                       "accepted": None}
            bridge.save_state(path, mapping)  # Durable binding precedes any admission.
        state_path, state = mapped_state(mapping, config)
        if state is None or state.get("accepted") is None:
            require(event["status"] != "running", "Remote accepted handles lack local confirmation; do not resubmit.")
            # A saved intent may represent a lost admission response: never mark it failed.
            snapshot = approved_snapshot(args, client, event, output, can_reject=state is None)
            request = bridge.worker_request(snapshot, args.project, args.prepared_state, args.harness)
            require(bridge.identity(snapshot, request, args.fake_binary) == mapping["idempotency_key"],
                    "Saved approval identity changed; do not resubmit.")
            output["admission"] = "uncertain"
            dispatch_args = argparse.Namespace(**vars(args))
            dispatch_args.post = False
            admitted = {}
            try:
                bridge.dispatch(dispatch_args, admitted)
            except BaseException:
                # Admission can succeed before evidence/persistence fails. Preserve any actual saved handles.
                if state_path.exists():
                    saved = bridge.read_state(state_path)
                    if saved.get("accepted") is not None:
                        save_handles(mapping, path, saved, output)
                raise
            state = bridge.read_state(state_path)
        handles = save_handles(mapping, path, state, output)
        if event["status"] == "running":
            require(all(event.get(key) == value for key, value in handles.items()), "Remote/local accepted handles disagree; do not resubmit.")
        client.ack(event, "running", handles)
        shown = {}
        show_args = argparse.Namespace(fabric=args.fabric, state=str(state_path), wait_seconds=30,
                                       gh=args.gh, post=False)
        bridge.show(show_args, shown)
        results = shown["results"]
        require(type(results.get("terminal")) is bool, "Fabric did not provide terminal evidence.")
        result = results["tasks"][0].get("result")
        manifest = result.get("artifact_manifest_id") if isinstance(result, dict) else None
        if manifest is not None:
            handles = {**handles, "artifact_manifest_id": safe_id(manifest)}
            output["artifact_manifest_id"] = manifest
        if results["terminal"]:
            require(type(results.get("succeeded")) is bool, "Fabric did not provide an actual terminal outcome.")
            output["status"] = "completed" if results["succeeded"] else "failed"
            client.ack(event, output["status"], handles)
        if args.post:
            show_args.post = True
            # Reuse only the existing allowlisted evidence comment, never remote text or logs.
            with bridge.locked(state_path):
                bridge.post(show_args, bridge.read_state(state_path), state_path, shown)
            output["posting"] = shown["posting"]


def parse_args():
    parser = Parser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True, parser_class=Parser)
    for name in ("status", "once"):
        command = commands.add_parser(name)
        command.add_argument("--connection", required=True)
        command.add_argument("--allow-loopback-http", action="store_true")
        if name == "once":
            command.add_argument("--issue", required=True, type=int)
            for option in ("approve", "fabric", "root", "project", "prepared-state", "allow-author", "allow-label"):
                command.add_argument("--" + option, required=True)
            command.add_argument("--harness", choices=("fake", "omp", "codex"), required=True)
            command.add_argument("--gh", default="gh")
            command.add_argument("--fake-binary")
            command.add_argument("--allow-model-spend", action="store_true")
            command.add_argument("--post", action="store_true")
    return parser.parse_args()


def main():
    output = {}
    try:
        args = parse_args()
        args.repo = bridge.REPO
        client = Client(args)
        if args.command == "status":
            output.update(status(client))
        else:
            once(args, client, output)
    except (ConnectError, bridge.BridgeError, OSError, ValueError, KeyError, TypeError, KeyboardInterrupt):
        error = sys.exc_info()[1]
        output["error"] = str(error) if isinstance(error, ConnectError) else "Local bridge operation failed; raw error withheld. Repeat the same command to reconcile saved intent."
        if output.get("admission") == "uncertain":
            output["recovery"] = "Admission is uncertain. Preserve private state; explicitly repeat the SAME command only, never create a new approval to retry."
        elif output.get("accepted") is True:
            output["recovery"] = "Accepted handles are retained. Repeat the SAME command to observe evidence; do not resubmit."
        print(bridge.canonical(output))
        return 1
    print(bridge.canonical(output))
    return 0


if __name__ == "__main__":
    sys.exit(main())
