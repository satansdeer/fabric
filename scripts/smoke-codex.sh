#!/bin/sh
# Explicit live opt-in only; provider charges/account limits apply.
exec python3 - "$@" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

PIN = "codex-cli 0.171.0"
summary = {"status": "NOT_VERIFIED", "harness": "codex", "expected_version": PIN,
           "reason": "opt-in required: --run --profile ABS_PATH; live calls may incur provider charges; native account limits apply"}
root = None
invoked = False

def emit():
    print(json.dumps(summary, sort_keys=True))

def command(args, timeout=30, env=None):
    # Spool output to bounded files; never retain or print raw provider/errors.
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        process = subprocess.Popen(args, stdout=out, stderr=err, stdin=subprocess.DEVNULL, env=env)
        import time
        deadline = time.monotonic() + timeout
        while process.poll() is None:
            if time.monotonic() > deadline or os.fstat(out.fileno()).st_size + os.fstat(err.fileno()).st_size > 2 * 1024 * 1024:
                process.kill()
                process.wait()
                raise RuntimeError("command bound exceeded")
            time.sleep(0.05)
        if process.returncode:
            raise RuntimeError("command failed")
        out.seek(0)
        data = out.read(2 * 1024 * 1024 + 1)
        if len(data) > 2 * 1024 * 1024:
            raise RuntimeError("output bound exceeded")
        return data

try:
    args = sys.argv[1:]
    if not args:
        emit()
        sys.exit(0)
    if len(args) != 3 or args[:2] != ["--run", "--profile"]:
        raise ValueError("explicit --run --profile ABS_PATH required")
    profile = Path(args[2])
    if not profile.is_absolute() or not profile.is_file() or profile.stat().st_size > 65536:
        raise ValueError("invalid nonsecret profile path")
    with profile.open("rb") as stream:
        selected = json.loads(stream.read(65537))
    if selected.get("harness_id") != "codex" or selected.get("harness_version") != PIN:
        raise ValueError("profile must bind pinned Codex")
    binary = Path(os.environ.get("FABRIC_BINARY", "target/debug/fabric")).resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        summary["reason"] = "existing Fabric binary unavailable"
        emit()
        sys.exit(0)
    # Doctor is version-only and auth-free. No Fabric root is allocated yet.
    try:
        doctor = json.loads(command([str(binary), "harnesses", "doctor", "codex", "--json"]))
        if doctor.get("version") != PIN:
            raise RuntimeError("unsupported version")
    except Exception:
        summary["reason"] = "Codex missing, unsupported, or nonbilling doctor failed"
        emit()
        sys.exit(0)
    summary["observed_version"] = doctor["version"]
    root = Path(tempfile.mkdtemp(prefix="fabric-codex-smoke-"))
    source = root / "source"
    source.mkdir()
    git_env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
                   GIT_AUTHOR_NAME="Fabric Smoke", GIT_AUTHOR_EMAIL="smoke@example.invalid",
                   GIT_COMMITTER_NAME="Fabric Smoke", GIT_COMMITTER_EMAIL="smoke@example.invalid")
    command(["git", "-C", str(source), "init", "--quiet"], env=git_env)
    (source / "README").write_text("Disposable Fabric smoke source.\n")
    command(["git", "-C", str(source), "add", "."], env=git_env)
    command(["git", "-C", str(source), "commit", "--quiet", "-m", "smoke"], env=git_env)
    storage = root / "fabric"
    cli = [str(binary), "--root", str(storage)]
    command(cli + ["project", "add", "--name", "smoke", "--path", str(source)])
    recipe = root / "recipe.toml"
    recipe.write_text("version=1\n[[steps]]\nargv=['/usr/bin/true']\n")
    command(cli + ["project", "prepare", "smoke", "--recipe", str(recipe), "--json"], timeout=60)
    prompt = root / "prompt.txt"
    prompt.write_text("Reply FABRIC_SMOKE_OK. Do not edit files or invoke tools.")
    invoked = True
    report = json.loads(command(cli + ["run", "smoke", "--harness", "codex", "--profile", str(profile),
        "--prompt-file", str(prompt), "--json", "--timeout-seconds", "60", "--max-log-bytes", "1048576",
        "--max-artifact-bytes", "10485760", "--copy-limit-bytes", "10485760"], timeout=120))
    summary["run_id"] = report["run_id"]
    summary["profile_digest"] = report["profile"]["digest"]
    if report["state"] != "SUCCEEDED" or report["harness_version"] != PIN or report["exit_code"] != 0:
        raise RuntimeError("run did not succeed")
    with sqlite3.connect(f"file:{storage / 'fabric.db'}?mode=ro", uri=True) as db:
        workspace_state = db.execute("SELECT state FROM workspaces WHERE workspace_id=?", (report["workspace_id"],)).fetchone()
        files = db.execute("SELECT kind,relative_path,sha256,size_bytes FROM artifacts WHERE run_id=?", (report["run_id"],)).fetchall()
    if workspace_state != ("RECLAIMED",) or Path(report["workspace_path"]).exists() or not files:
        raise RuntimeError("workspace not reclaimed or artifacts absent")
    manifest = None
    output_ok = False
    for kind, relative, digest, size in files:
        path = (storage / relative).resolve()
        if not path.is_relative_to(storage.resolve()) or size > 10485760:
            raise RuntimeError("unsafe artifact")
        data = path.read_bytes()
        if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
            raise RuntimeError("artifact verification failed")
        if kind == "manifest":
            manifest = json.loads(data)
        if path.name == "events.jsonl":
            for line in data.splitlines():
                event = json.loads(line).get("event", {})
                if event.get("type") == "output" and "FABRIC_SMOKE_OK" in json.dumps(event):
                    output_ok = True
    if manifest is None or manifest.get("profile") != report["profile"] or not output_ok:
        raise RuntimeError("missing verified smoke evidence")
    summary.update(status="PASS", reason="actual live run succeeded; artifacts verified; workspace reclaimed",
                   artifact_count=len(files), cleanup="RECLAIMED", evidence_root=str(root))
except SystemExit:
    raise
except Exception:
    summary.update(status="FAIL" if invoked else "NOT_VERIFIED",
                   reason="live invocation or evidence verification failed" if invoked else "preflight/setup rejected")
    if root is not None:
        summary["recovery_root"] = str(root)
# Keep evidence/source on success as well; never delete recovery workspaces.
emit()
sys.exit(1 if summary["status"] == "FAIL" else 0)
PY
