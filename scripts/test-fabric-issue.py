#!/usr/bin/env python3
"""Provider-free issue bridge integration check; requires existing Fabric binaries."""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import stat
import subprocess
import sys
import tarfile
import tempfile
import time


REPO = "satansdeer/fabric"
ISSUE = 16
TOKEN = "fixture-only-not-a-real-credential-7c873b"
PRIVATE_LOG = "PRIVATE_WORKER_OUTPUT_DO_NOT_POST_75f3"
BODY = "Untrusted fixture scope; $(touch INJECTED). BODY_MUST_NOT_BE_POSTED_903a"
TRACKED = "trusted fake mutation ; $(touch INJECTED)\n"
UNTRACKED = "actual retained untracked content\n"


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def save_json(path, value):
    path.write_text(json.dumps(value, sort_keys=True) + "\n")


def executable(path, source):
    path.write_text("#!" + sys.executable + "\n" + source)
    path.chmod(0o700)


def snapshot(path):
    return {
        str(item.relative_to(path)): (
            "link:" + os.readlink(item) if item.is_symlink()
            else hashlib.sha256(item.read_bytes()).hexdigest()
        )
        for item in sorted(path.rglob("*"))
        if ".git" not in item.relative_to(path).parts
        and (item.is_file() or item.is_symlink())
    }


class Fixture:
    def __init__(self, fabric, keep):
        self.path = Path(tempfile.mkdtemp(prefix="fabric-issue-check-")).resolve()
        self.keep = keep
        self.fabric = fabric
        self.root = self.path / "private fabric"
        self.source = self.path / "source ; literal"
        self.bin = self.path / "bin"
        self.bin.mkdir()
        self.source.mkdir()
        self.env = {
            "PATH": str(self.bin) + ":/usr/bin:/bin",
            "HOME": str(self.path), "LC_ALL": "C",
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_AUTHOR_NAME": "Fabric Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
            "GIT_COMMITTER_NAME": "Fabric Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
            "GH_TOKEN": TOKEN, "UNSELECTED_SECRET": TOKEN,
        }
        self.bridge = Path(__file__).resolve().with_name("fabric-issue.py")
        self.issue = {
            "number": ISSUE, "state": "open", "title": "Fixture issue ; literal",
            "body": BODY, "html_url": f"https://github.com/{REPO}/issues/{ISSUE}",
            "user": {"login": "fixture-author"}, "labels": [{"name": "ready"}],
        }
        self.issue_path = self.path / "issue.json"
        save_json(self.issue_path, self.issue)
        self.install_fakes()

    def run(self, args, *, good=True, json_output=True, timeout=30, input_text=None):
        result = subprocess.run(
            [str(arg) for arg in args], env=self.env, cwd=self.path,
            input=input_text, capture_output=True, text=True, timeout=timeout,
        )
        require(TOKEN not in result.stdout + result.stderr, "credential sentinel leaked in command output")
        require((result.returncode == 0) == good,
                "unexpected command exit: " + Path(str(args[0])).name + " " + str(result.returncode))
        if json_output:
            try:
                value = json.loads(result.stdout)
            except (ValueError, TypeError):
                raise AssertionError("command stdout is not exactly one JSON value") from None
            require(isinstance(value, dict), "command stdout is not a JSON object")
            return value
        return result

    def cli(self, *args, **kwargs):
        return self.run([self.fabric, "--root", self.root, *args], **kwargs)

    def bridge_command(self, command, *args, good=True):
        return self.run([sys.executable, self.bridge, command, *args], good=good, timeout=45)

    def issue_args(self):
        return ["--repo", REPO, "--issue", str(ISSUE), "--allow-author", "fixture-author",
                "--allow-label", "ready", "--gh", self.bin / "gh"]

    def counts(self):
        with sqlite3.connect((self.root / "fabric.db").as_uri() + "?mode=ro", uri=True) as db:
            return {table: db.execute("SELECT count(*) FROM " + table).fetchone()[0]
                    for table in ("batches", "batch_tasks", "runs", "async_batches", "async_tasks")}

    def comments(self):
        path = self.path / "comments.json"
        return json.loads(path.read_text()) if path.exists() else []

    def install_fakes(self):
        executable(self.bin / "fabric", f"""from pathlib import Path
Path({str(self.path / 'forbidden-fabric-call')!r}).write_text('called')
raise SystemExit(97)
""")
        executable(self.bin / "gh", f"""import json
from pathlib import Path
import sys
root = Path({str(self.path)!r})
args = sys.argv[1:]
if not args or args.pop(0) != 'api':
    raise SystemExit(91)
endpoint = None
method = 'GET'
fields = {{}}
while args:
    arg = args.pop(0)
    if arg in ('-X', '--method'):
        method = args.pop(0).upper()
    elif arg in ('-f', '--raw-field', '-F', '--field'):
        key, value = args.pop(0).split('=', 1)
        fields[key] = value
        if method == 'GET': method = 'POST'
    elif arg == '--input':
        file = args.pop(0)
        fields.update(json.loads(sys.stdin.read() if file == '-' else Path(file).read_text()))
        if method == 'GET': method = 'POST'
    elif arg in ('-H', '--header'):
        args.pop(0)
    elif not arg.startswith('-') and endpoint is None:
        endpoint = arg.lstrip('/')
    elif arg != '--paginate':
        raise SystemExit(92)
with (root / 'gh-calls.jsonl').open('a') as stream:
    stream.write(json.dumps({{'method': method, 'endpoint': endpoint}}) + '\\n')
issue_endpoint = 'repos/{REPO}/issues/{ISSUE}'
comments_path = root / 'comments.json'
comments = json.loads(comments_path.read_text()) if comments_path.exists() else []
if method == 'GET' and endpoint == issue_endpoint:
    print((root / 'issue.json').read_text())
elif method == 'GET' and endpoint.split('?')[0] == issue_endpoint + '/comments':
    print(json.dumps(comments))
elif method in ('POST', 'PATCH') and (endpoint == issue_endpoint + '/comments' or endpoint.startswith('repos/{REPO}/issues/comments/')):
    if (root / 'fail-post').exists():
        sys.stderr.write({TOKEN!r} + ' raw GitHub stderr must not escape')
        raise SystemExit(1)
    body = fields['body']
    if method == 'PATCH':
        comment_id = int(endpoint.rsplit('/', 1)[1])
        comment = next(item for item in comments if item['id'] == comment_id)
        comment['body'] = body
    else:
        comment = {{'id': 700 + len(comments), 'body': body, 'user': {{'login': 'fixture-controller'}},
                   'html_url': 'https://github.com/{REPO}/issues/{ISSUE}#issuecomment-' + str(700 + len(comments))}}
        comments.append(comment)
    comments_path.write_text(json.dumps(comments))
    print(json.dumps(comment))
else:
    raise SystemExit(93)
""")
        fake = self.fabric.with_name("fabric-fake-harness")
        require(fake.is_file() and os.access(fake, os.X_OK), "sibling fabric-fake-harness is required")
        task = self.path / "trusted-fake-task.json"
        save_json(task, {
            "required_files": ["tracked", ".deps/ready"],
            "writes": [{"path": "tracked", "content": TRACKED},
                       {"path": "new-source", "content": UNTRACKED}],
            "events": [{"type": "output", "text": PRIVATE_LOG}],
            "absent_environment": ["GH_TOKEN", "UNSELECTED_SECRET"],
            "exit_code": 0,
        })
        self.fake_wrapper = self.bin / "trusted fake ; literal"
        executable(self.fake_wrapper, f"""import json
import os
from pathlib import Path
import sys
fake = {str(fake)!r}
args = sys.argv[1:]
if args == ['--version']:
    os.execv(fake, [fake, *args])
if len(args) != 2 or args[0] != '--prompt-file':
    raise SystemExit(94)
if os.environ.get('HOME') == {str(self.path)!r}:
    raise SystemExit(95)
prompt = Path(args[1]).read_text()
json.loads(prompt)
Path({str(self.path / 'observed-worker-prompt.json')!r}).write_text(prompt)
os.execv(fake, [fake, '--prompt-file', {str(task)!r}])
""")

    def cleanup(self):
        """Only use owned-root manager APIs; never signal a persisted PID."""
        if not self.root.exists():
            return True
        batches = []
        try:
            if (self.root / "fabric.db").exists():
                with sqlite3.connect((self.root / "fabric.db").as_uri() + "?mode=ro", uri=True) as db:
                    batches = [row[0] for row in db.execute("SELECT batch_id FROM async_batches")]
            for batch in batches:
                subprocess.run([str(self.fabric), "--root", str(self.root), "cancel", batch, "--json"],
                               env=self.env, cwd=self.path, capture_output=True, timeout=5)
            if batches or (self.root / "manager.lock").exists():
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    stopped = subprocess.run(
                        [str(self.fabric), "--root", str(self.root), "manager", "stop", "--json"],
                        env=self.env, cwd=self.path, capture_output=True, timeout=5,
                    )
                    if stopped.returncode == 0:
                        return True
                    time.sleep(0.1)
                return False
            return True
        except (OSError, sqlite3.Error, subprocess.TimeoutExpired):
            return False


def check(fixture, report):
    f = fixture
    baseline = f.bridge_command("preview", *f.issue_args())
    require(baseline["repo"] == REPO and baseline["issue_number"] == ISSUE, "preview identity mismatch")
    require(baseline["author"] == "fixture-author" and baseline["body"] == BODY
            and baseline["title"] == f.issue["title"] and baseline["issue_url"] == f.issue["html_url"],
            "preview snapshot mismatch")
    approval = baseline["approval_sha256"]
    require(len(approval) == 64 and all(c in "0123456789abcdef" for c in approval), "invalid approval digest")
    require(not f.root.exists() and not (f.path / "forbidden-fabric-call").exists(), "preview admitted work")
    require(not f.comments(), "preview posted a comment")
    report["checks"].append("preview_is_read_only")

    for args in (["init", "--quiet"],):
        f.run(["/usr/bin/git", "-C", f.source, *args], json_output=False)
    (f.source / "tracked").write_text("original\n")
    (f.source / ".gitignore").write_text(".deps/\n")
    f.run(["/usr/bin/git", "-C", f.source, "add", "."], json_output=False)
    f.run(["/usr/bin/git", "-C", f.source, "commit", "--quiet", "-m", "disposable fixture"], json_output=False)
    f.cli("project", "add", "--name", "fixture", "--path", f.source, json_output=False)
    recipe = f.path / "recipe.toml"
    recipe.write_text("version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n")
    prepared = f.cli("project", "prepare", "fixture", "--recipe", recipe, "--json", timeout=60)
    require(prepared["status"] == "READY", "real preparation did not reach READY")
    ready = Path(prepared["prepared_path"])
    source_before, ready_before = snapshot(f.source), snapshot(ready)
    initial_counts = f.counts()
    require(all(count == 0 for count in initial_counts.values()), "fixture already has worker admissions")
    source_args = ["--root", f.root, "--project", "fixture", "--prepared-state", prepared["state_id"]]

    def dispatch(*, harness="fake", approve=approval, source=None, fabric=None, good=True, post=False):
        args = [*f.issue_args(), "--approve", approve, "--fabric", fabric or f.fabric,
                *(source or source_args), "--harness", harness]
        if harness == "fake":
            args += ["--fake-binary", f.fake_wrapper]
        if post:
            args += ["--post"]
        return f.bridge_command("dispatch", *args, good=good)

    cases = [
        {"body": BODY + " changed after approval"},
        {"user": {"login": "unauthorized"}},
        {"labels": []}, {"state": "closed"},
        {"pull_request": {"url": f"https://api.github.com/repos/{REPO}/pulls/{ISSUE}"}},
    ]
    for changes in cases:
        issue = copy.deepcopy(f.issue)
        issue.update(changes)
        save_json(f.issue_path, issue)
        dispatch(good=False, fabric=f.bin / "fabric")
        require(f.counts() == initial_counts, "rejected issue created worker admission")
    save_json(f.issue_path, f.issue)
    unauthorized_repo = f.issue_args()
    unauthorized_repo[1] = "other/repository"
    f.bridge_command("preview", *unauthorized_repo, good=False)
    dispatch(harness="omp", good=False, fabric=f.bin / "fabric")
    require(not (f.path / "forbidden-fabric-call").exists(), "pre-admission rejection invoked Fabric")
    dispatch(source=["--root", f.root, "--project", "fixture", "--prepared-state", "missing-ready-state"], good=False)
    require(f.counts() == initial_counts, "invalid source or missing spend admitted work")
    require(not f.comments(), "rejected work posted a comment")
    report["checks"].append("authorization_digest_spend_and_source_rejected_before_admission")

    (f.path / "fail-post").touch()
    accepted = dispatch(post=True, good=False)
    (f.path / "fail-post").unlink()
    require(accepted["accepted"] is True, "dispatch was not accepted")
    require(len(accepted["run_ids"]) == 1 and accepted["batch_id"] and accepted["idempotency_key"], "missing accepted handles")
    require(accepted["issue_url"] == f.issue["html_url"], "dispatch issue identity changed")
    batch, run = accepted["batch_id"], accepted["run_ids"][0]
    state = Path(accepted["state_file"])
    require(state.is_file() and state.resolve().is_relative_to((f.root / "issue-bridge").resolve()), "state is outside private bridge root")
    require(stat.S_IMODE(state.stat().st_mode) == 0o600, "state mode is not 0600")
    require(stat.S_IMODE(state.parent.stat().st_mode) == 0o700, "state directory mode is not 0700")
    saved = json.loads(state.read_text())
    require(isinstance(saved, dict) and TOKEN not in state.read_text(), "state contains credential sentinel")
    require(isinstance(saved.get("schema_version", saved.get("version")), int), "state lacks explicit schema version")
    for reference in (approval, BODY, str(f.root), prepared["state_id"], batch, run, accepted["idempotency_key"]):
        require(reference in json.dumps(saved), "state omitted immutable provenance or accepted handles")
    require(not f.comments(), "failed dispatch post created a fake comment")
    require(f.counts()["async_batches"] == 1 and f.counts()["batch_tasks"] == 1, "dispatch was not a single durable task")
    repeat = dispatch()
    for key in ("batch_id", "run_ids", "idempotency_key", "state_file"):
        require(repeat[key] == accepted[key], "repeat dispatch changed " + key)
    require(f.counts()["async_batches"] == 1 and f.counts()["batch_tasks"] == 1, "repeat dispatch created new work")
    report.update(batch_id=batch, run_ids=[run], idempotency_key=accepted["idempotency_key"])
    report["checks"].append("durable_single_admission_and_repeat_idempotency")

    show_args = ["--state", state, "--fabric", f.fabric, "--gh", f.bin / "gh"]
    shown = f.bridge_command("show", *show_args, "--wait-seconds", "30")
    actual_status = f.cli("status", batch, "--json")
    results = f.cli("results", batch, "--json")
    require(shown["status"] == actual_status and shown["results"] == results, "show did not expose actual Fabric evidence")
    require(shown["batch_id"] == batch and shown["run_ids"] == [run]
            and shown["issue_url"] == accepted["issue_url"], "show lost accepted identity")
    require(results["terminal"] is True and results["succeeded"] is True, "real worker did not succeed")
    require(len(results["tasks"]) == 1 and results["tasks"][0]["run_id"] == run, "terminal mapping mismatch")
    task = results["tasks"][0]
    require(task["state"] == "SUCCEEDED", "worker state is not SUCCEEDED")
    result = task["result"]
    require(result["prepared_state_id"] == prepared["state_id"]
            and result["base_commit_sha"] == prepared["source_commit_sha"], "worker source provenance changed")
    manifest = result["artifact_manifest_id"]
    run_show = f.cli("runs", "show", run, "--json")
    require(run_show["workspace"]["state"] == "RECLAIMED"
            and not Path(run_show["workspace"]["managed_path"]).exists(), "workspace was not reclaimed")
    artifacts = f.cli("runs", "artifacts", run, "--json")["artifacts"]
    require(artifacts, "no retained artifacts")
    retained = {}
    for artifact in artifacts:
        path = (f.root / artifact["relative_path"]).resolve()
        require(path.is_relative_to(f.root.resolve()), "artifact escaped private root")
        data = path.read_bytes()
        require(len(data) == artifact["size_bytes"] and hashlib.sha256(data).hexdigest() == artifact["sha256"], "artifact digest/size mismatch")
        retained[path.name] = path
    require(TRACKED.rstrip() in retained["tracked.patch"].read_text(), "tracked patch lacks actual worker mutation")
    with tarfile.open(retained["untracked.tar"]) as archive:
        candidates = [member for member in archive.getmembers() if member.name.removeprefix("./") == "new-source"]
        require(len(candidates) == 1 and candidates[0].isfile(), "untracked artifact missing file")
        with archive.extractfile(candidates[0]) as stream:
            require(stream.read() == UNTRACKED.encode(), "untracked artifact content mismatch")
    require(PRIVATE_LOG in retained["stdout.log"].read_text(), "actual private worker log was not captured")
    require(snapshot(f.source) == source_before and snapshot(ready) == ready_before, "original or READY tree mutated")
    require(not any(f.path.rglob("INJECTED")), "issue text was executed as a shell command")
    prompt = (f.path / "observed-worker-prompt.json").read_text()
    require(BODY in json.dumps(json.loads(prompt), ensure_ascii=False), "worker prompt omitted issue scope")
    report.update(artifact_manifest_id=manifest, artifact_count=len(artifacts),
                  artifacts=artifacts, cleanup="RECLAIMED")
    report["checks"].append("actual_terminal_artifacts_digests_mutations_and_immutable_sources")

    # Inspection must not depend on the issue still being eligible for dispatch.
    closed = copy.deepcopy(f.issue)
    closed.update(state="closed", labels=[])
    save_json(f.issue_path, closed)
    f.bridge_command("show", *show_args, "--post")
    comments = f.comments()
    require(len(comments) == 1, "explicit show did not create exactly one comment")
    body = comments[0]["body"]
    for reference in (batch, run, accepted["idempotency_key"], manifest):
        require(reference in body, "comment omitted actual evidence reference")
    require("SUCCEEDED" in body.upper() or "SUCCESS" in body.upper(), "comment omitted actual terminal outcome")
    for private in (BODY, PRIVATE_LOG, TOKEN, str(f.path), str(f.root), str(f.source)):
        require(private not in body, "private evidence leaked into GitHub comment")
    f.bridge_command("show", *show_args, "--post")
    require(len(f.comments()) == 1, "repeated post duplicated comment instead of updating")
    before_failure = f.counts()
    (f.path / "fail-post").touch()
    failed = f.bridge_command("show", *show_args, "--post", good=False)
    require(batch in json.dumps(failed) and run in json.dumps(failed), "post failure lost accepted handles")
    require(TOKEN not in state.read_text(), "post failure persisted credential sentinel")
    (f.path / "fail-post").unlink()
    save_json(f.issue_path, f.issue)
    retried = dispatch()
    require(retried["batch_id"] == batch and retried["run_ids"] == [run], "post failure caused resubmission")
    require(f.counts() == before_failure, "show/post or recovery created new worker work")
    for persisted in (f.root / "issue-bridge").rglob("*"):
        if persisted.is_file():
            require(TOKEN.encode() not in persisted.read_bytes(), "bridge persisted credential sentinel")
    require(f.bridge_command("show", *show_args)["results"] == results, "post failure damaged saved evidence")
    report["checks"].append("sanitized_actual_comment_update_and_post_failure_without_resubmission")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fabric", required=True, type=Path, help="absolute existing target/debug/fabric")
    parser.add_argument("--keep", action="store_true", help="retain private fixture and evidence.json")
    args = parser.parse_args()
    require(args.fabric.is_absolute() and args.fabric.is_file() and os.access(args.fabric, os.X_OK), "--fabric must be an absolute executable")
    os.umask(0o077)
    fixture = None
    report = {"status": "FAIL", "provider_calls": 0, "checks": []}
    try:
        fixture = Fixture(args.fabric.resolve(), args.keep)
        check(fixture, report)
        report["status"] = "PASS"
    except (AssertionError, OSError, ValueError, KeyError, sqlite3.Error, subprocess.SubprocessError, tarfile.TarError) as error:
        # Commands may contain private issue/provider output: never print captured streams.
        report["error"] = str(error).replace(TOKEN, "[redacted]") if isinstance(error, AssertionError) else type(error).__name__
    finally:
        if fixture is not None:
            stopped = fixture.cleanup()
            report["manager_stopped"] = stopped
            if not stopped:
                report.update(status="FAIL", error="owned manager did not stop; fixture retained for recovery")
            if fixture.keep or report["status"] != "PASS":
                report["fixture_directory"] = str(fixture.path)
                save_json(fixture.path / "evidence.json", report)
            else:
                for directory, _, _ in os.walk(fixture.path):
                    Path(directory).chmod(0o700)
                shutil.rmtree(fixture.path)
    print(json.dumps(report, sort_keys=True))
    return 0 if report["status"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
