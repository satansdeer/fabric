#!/usr/bin/python3
"""Provider-free public remote.v1 adapter to the real generic agent binary.

Test-only export faults model the completed-result/controller crash window. No
Kubernetes objects, provider, production fault flag, or replacement worker.
"""
import json
import hashlib
import io
import os
from pathlib import Path
import subprocess
import select
import time
import sys
import tarfile

config = json.loads(Path(sys.argv[1]).read_text())
root = Path(config["root"])
agent = None
binding = None
sequence = 0
pending = None
instance = "original-instance"
resource = {"backend": "remote.v1", "resource_id": "fixture-storage-original"}
malformed_export = None


def call(operation, value):
    global agent, binding, sequence
    environment = value if operation.startswith("environment.") else value.get("environment", value.get("resource", {}).get("environment"))
    assert environment["resource"] == resource
    if agent is None:
        binding = environment
        argv = [config["agent"], str(root / "home"), str(root / "work")]
        claim = root / "initialized"
        try:
            fd = os.open(claim, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError:
            pass
        else:
            os.close(fd)
            argv.append("--initialize-home")
        agent = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, env={"PATH": "/usr/bin:/bin", "FABRIC_TARGET_INSTANCE": instance})
        agent.stdin.write(json.dumps(binding).encode() + b"\n")
        agent.stdin.flush()
    assert environment == binding
    sequence += 1
    agent.stdin.write(json.dumps(dict(id=sequence, operation=operation, input=value)).encode() + b"\n")
    agent.stdin.flush()
    reply = json.loads(agent.stdout.readline(65537))
    assert reply["id"] == sequence and "error" not in reply, reply
    return reply["result"]


def invoke(operation, value):
    global malformed_export
    if operation == "workspace.read" and malformed_export is not None:
        offset = value["offset"]
        assert 0 <= offset < len(malformed_export)
        return {"bytes": list(malformed_export[offset:offset + 8192])}
    if operation in ("environment.ensure", "environment.inspect"):
        if value["resource"] is None:
            assert operation == "environment.ensure"
            return call(operation, dict(value, resource=resource))
    if operation == "workspace.export" and (root / "pause-export").exists():
        (root / "export-ready").write_bytes(b"completed before controller crash")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if select.select([sys.stdin], [], [], 0.05)[0]:
                assert os.read(sys.stdin.fileno(), 1) == b""
                raise RuntimeError("original controller disconnected")
        raise RuntimeError("test controller did not terminate")
    # Leave an actual durable completed result and every retained remote byte.
    # Failure happens before exporting/downloading anything to the controller.
    fail = root / "fail-export"
    if operation == "workspace.export" and fail.exists():
        fail.unlink()
        raise RuntimeError("completed result retained")
    result = call(operation, value)
    corrupt = root / "corrupt-export"
    if operation == "workspace.export" and corrupt.exists():
        mode = corrupt.read_text()
        if mode == "hash":
            result["sha256"] = "0" * 64
        elif mode == "size":
            result["size"] = 134217729
        elif mode == "binding":
            result["witness"]["instance"] = "foreign-instance"
        elif mode in ("malformed_tar", "missing_tree", "missing_log", "log_type"):
            buffer = io.BytesIO()
            with tarfile.open(fileobj=buffer, mode="w") as archive:
                for name in ("tree", "stdout.log", "stderr.log", "events.jsonl"):
                    if (mode == "missing_tree" and name == "tree") or (mode == "missing_log" and name == "stderr.log"):
                        continue
                    entry = tarfile.TarInfo(name)
                    entry.mode = 0o700 if name == "tree" else 0o600
                    if name == "tree" or (mode == "log_type" and name == "stdout.log"):
                        entry.type = tarfile.DIRTYPE
                    archive.addfile(entry, io.BytesIO())
            malformed_export = b"not a tar archive" if mode == "malformed_tar" else buffer.getvalue()
            result["size"] = len(malformed_export)
            result["sha256"] = hashlib.sha256(malformed_export).hexdigest()
        else:
            raise AssertionError(mode)
    if operation == "workspace.reclaim":
        assert result == {"reclaimed": True}
    return result


try:
    for line in sys.stdin:
        request = json.loads(line)
        method, params = request["method"], request["params"]
        with (root / "requests.jsonl").open("a") as log:
            log.write(json.dumps(request) + "\n")
        if method == "initialize":
            result = dict(protocol=1, id="remote-proof", version="0.1.0", kind="target")
        elif method in ("health", "shutdown"):
            result = {"ready": True}
        elif method == "target.capabilities":
            result = dict(placements=["remote-proof"], operations=["prepare", "start", "inspect", "cancel", "collect", "reclaim"], optional=["environment.v1", "pty.v1", "remote.v1"])
        elif method == "target.placement":
            assert params == {"placement": "remote-proof", "backends": ["remote.v1"]}
            result = {"backend": "remote.v1"}
        elif method == "target.environment.capabilities":
            result = dict(version=1, persistent_home=True, interactive_exec_pty=True, infrastructure_diagnostics=True, operations=["ensure", "inspect", "destroy", "workspace"])
        elif method == "target.remote.invoke":
            assert pending is None and params["version"] == 1
            try:
                pending = dict(state="complete", result=invoke(params["operation"], params["input"]))
            except Exception as error:
                with (root / "errors.jsonl").open("a") as log:
                    log.write(json.dumps({"operation": params["operation"], "error": repr(error)}) + "\n")
                pending = dict(state="failed", scope="target", kind="output")
            result = {"ticket": "single-flight"}
        elif method == "target.remote.poll":
            assert params == {"ticket": "single-flight"} and pending is not None
            result, pending = pending, None
        else:
            raise AssertionError(method)
        print(json.dumps(dict(jsonrpc="2.0", id=request["id"], result=result)), flush=True)
        if method == "shutdown":
            break
finally:
    if agent is not None:
        agent.stdin.close()
        try:
            agent.wait(timeout=5)
        except subprocess.TimeoutExpired:
            agent.kill()
            agent.wait()
        if (root / "export-ready").exists():
            (root / "export-closed").write_bytes(b"original agent reaped")
