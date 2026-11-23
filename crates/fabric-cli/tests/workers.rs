#![cfg(unix)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
const DEADLINE: Duration = Duration::from_secs(20);

struct Process {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Process {
    fn output(&mut self, timeout: Duration) -> Option<Output> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().ok()? {
                return Some(Output {
                    status,
                    stdout: fs::read(&self.stdout).ok()?,
                    stderr: fs::read(&self.stderr).ok()?,
                });
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[path = "../../fabric-runner/tests/support/reference_plugins.rs"]
mod reference_plugins;

struct Fixture {
    path: PathBuf,
    repo: PathBuf,
    base: Value,
    batches: Vec<String>,
    submitted: bool,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fabric-workers-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        reference_plugins::install(&path);
        let path = fs::canonicalize(path).unwrap();
        let mut fixture = Self {
            repo: path.join("repo ; literal"),
            path,
            base: Value::Null,
            batches: Vec::new(),
            submitted: false,
        };
        fs::create_dir(&fixture.repo).unwrap();
        fixture.git(&["init", "--quiet"]);
        fs::write(fixture.repo.join("tracked"), "original").unwrap();
        fs::write(fixture.repo.join(".gitignore"), ".deps/\n").unwrap();
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "--quiet", "-m", "initial"]);
        let mut add = fixture.cli();
        add.args(["project", "add", "--name", "test", "--path"])
            .arg(&fixture.repo);
        checked(&fixture.run(add));
        let recipe = fixture.path.join("recipe.toml");
        fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n").unwrap();
        let mut prepare = fixture.cli();
        prepare
            .args(["project", "prepare", "test", "--json", "--recipe"])
            .arg(recipe);
        fixture.base = parsed(&fixture.run(prepare));
        fixture.wrapper();
        fixture
    }

    fn git(&self, args: &[&str]) {
        checked(
            &Command::new("/usr/bin/git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .arg("-C")
                .arg(&self.repo)
                .args(args)
                .output()
                .unwrap(),
        );
    }

    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fabric"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.path)
            .env("FABRIC_PLUGIN_REGISTRY", self.path.join("plugin-registry"))
            .env("UNSELECTED_SECRET", "must-not-leak")
            .arg("--root")
            .arg(self.path.join("storage"));
        command
    }

    fn spawn(&self, mut command: Command) -> Process {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let stdout = self.path.join(format!("command-{id}.stdout"));
        let stderr = self.path.join(format!("command-{id}.stderr"));
        let child = command
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        Process {
            child,
            stdout,
            stderr,
        }
    }

    fn run(&self, command: Command) -> Output {
        let mut process = self.spawn(command);
        process
            .output(DEADLINE)
            .unwrap_or_else(|| panic!("CLI timed out: {:?}", fs::read_to_string(&process.stderr)))
    }

    #[track_caller]
    fn query(&self, verb: &str, id: &str) -> Value {
        let mut command = self.cli();
        command.args([verb, id, "--json"]);
        parsed(&self.run(command))
    }

    fn wrapper(&self) {
        let executable = self.path.join("gated-fake");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
set -eu
for arg in "$@"; do
  if [ "$arg" = --version ]; then exec '{fake}' "$@"; fi
done
case "$1" in
  --protocol-a|--protocol-b) prompt=$(cat "$3");;
  --prompt-file) prompt=$(cat "$2");;
  *) exit 89;;
esac
case "$prompt" in
  *\"content\":\"first* ) id=first;;
  *\"content\":\"second* ) id=second;;
  *\"content\":\"third* ) id=third;;
  *\"content\":\"fourth* ) id=fourth;;
  *) exit 90;;
esac
root='{root}'
printf 'start\n' > "$root/started.$id"
trap 'touch "$root/stopped.$id"' EXIT
trap 'exit 143' TERM INT
i=0
while [ ! -f "$root/release.$id" ] && [ ! -f "$root/release-all" ]; do
  i=$((i+1)); test "$i" -lt 1500
  sleep 0.02
done
'{fake}' "$@"
"#,
                fake = env!("CARGO_BIN_EXE_fabric-fake-harness"),
                root = self.path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn task(&self, id: &str, harness: &str) -> Value {
        json!({
            "task_id":id,"harness_id":harness,
            "prompt":json!({
                "required_files":[".deps/ready"],
                "writes":[{"path":"tracked","content":format!("{id} ; $(touch INJECTED) 'quoted'\nsecond line")},
                          {"path":"new-source","content":format!("new-{id}")}],
                "absent_environment":["UNSELECTED_SECRET"]
            }).to_string()
        })
    }

    fn request(&self, request: &Value) -> PathBuf {
        let path = self.path.join(format!(
            "request-{}.json",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
        path
    }

    fn spawn_command(&self, request: &Path) -> Command {
        let mut command = self.cli();
        command
            .args(["workers", "spawn", "--request"])
            .arg(request)
            .arg("--json")
            .arg("--fake-binary")
            .arg(self.path.join("gated-fake"));
        command
    }

    fn accept(&mut self, request: &Value) -> Value {
        let path = self.request(request);
        self.submitted = true;
        let accepted = parsed(&self.run(self.spawn_command(&path)));
        self.batches
            .push(accepted["batch_id"].as_str().unwrap().to_owned());
        fs::remove_file(path).unwrap();
        assert_eq!(accepted["accepted"], true);
        accepted
    }

    fn terminal(&self, id: &str) -> Value {
        let start = Instant::now();
        loop {
            let mut wait = self.cli();
            wait.args(["wait", id, "--timeout-seconds", "1", "--json"]);
            let status = parsed(&self.run(wait));
            if status["terminal"] == true {
                return status;
            }
            assert!(start.elapsed() < DEADLINE, "batch did not finish: {status}");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::write(self.path.join("release-all"), "");
        for id in &self.batches {
            let mut cancel = self.cli();
            cancel.args(["cancel", id, "--json"]);
            let _ = self.spawn(cancel).output(Duration::from_secs(2));
        }
        if self.submitted || self.path.join("storage/manager.lock").exists() {
            let start = Instant::now();
            loop {
                let mut stop = self.cli();
                stop.args(["manager", "stop", "--json"]);
                if self
                    .spawn(stop)
                    .output(Duration::from_secs(2))
                    .is_some_and(|out| out.status.success())
                {
                    break;
                }
                if start.elapsed() > Duration::from_secs(10) {
                    // Retain the root rather than delete state still owned by a manager.
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let start = Instant::now();
        while ["first", "second", "third", "fourth"].iter().any(|id| {
            self.path.join(format!("started.{id}")).exists()
                && !self.path.join(format!("stopped.{id}")).exists()
        }) {
            if start.elapsed() > Duration::from_secs(35) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        fn writable(path: &Path) {
            if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries.flatten() {
                        writable(&entry.path());
                    }
                }
            }
        }
        writable(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[track_caller]
fn checked(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}

#[track_caller]
fn parsed(output: &Output) -> Value {
    checked(output);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}: {output:?}"))
}

fn eventually(mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(start.elapsed() < DEADLINE, "fixture condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn generic_fake_protocols_return_early_handles_and_persist_results() {
    let mut fixture = Fixture::new();
    let accepted = fixture.accept(&json!({
        "project":"test","max_concurrency":2,
        "tasks":[fixture.task("first", "fake-a"),fixture.task("second", "fake-b")]
    }));
    let handles = accepted["tasks"].as_array().unwrap();
    assert_eq!(handles.len(), 2);
    for (index, id) in ["first", "second"].iter().enumerate() {
        assert_eq!(handles[index]["task_id"], *id);
        assert_eq!(handles[index]["status"], "queued");
        assert!(!handles[index]["run_id"].as_str().unwrap().is_empty());
        eventually(|| fixture.path.join(format!("started.{id}")).exists());
    }
    assert_ne!(handles[0]["run_id"], handles[1]["run_id"]);
    let batch = accepted["batch_id"].as_str().unwrap();
    assert_eq!(fixture.query("status", batch)["terminal"], false);
    assert_eq!(fixture.query("results", batch)["succeeded"], Value::Null);
    fs::write(fixture.path.join("release.first"), "").unwrap();
    let first = handles[0]["run_id"].as_str().unwrap();
    fixture.terminal(first);
    assert_eq!(
        fixture.query("status", batch)["terminal"],
        false,
        "second gate is independent"
    );
    fs::write(fixture.path.join("release.second"), "").unwrap();
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["succeeded"], true);
    let mut workspaces = Vec::new();
    for (index, task) in results["tasks"].as_array().unwrap().iter().enumerate() {
        assert_eq!(task["task_id"], handles[index]["task_id"]);
        assert_eq!(task["run_id"], handles[index]["run_id"]);
        assert_eq!(task["state"], "SUCCEEDED");
        let run = &task["result"];
        assert_eq!(run["harness"], ["fake-a", "fake-b"][index]);
        assert_eq!(run["prepared_state_id"], fixture.base["state_id"]);
        assert_eq!(run["base_commit_sha"], fixture.base["source_commit_sha"]);
        assert!(run["worker_attempts"][0]["model_requested"].is_null());
        workspaces.push(run["workspace_id"].as_str().unwrap().to_owned());
        if index == 0 {
            assert_eq!(
                run["worker_attempts"][0]["model_resolved"],
                "fixture-a-default"
            );
        } else {
            assert!(run["worker_attempts"][0]["model_resolved"].is_null());
        }
        let id = task["run_id"].as_str().unwrap();
        let mut show = fixture.cli();
        show.args(["runs", "show", id, "--json"]);
        let shown = parsed(&fixture.run(show));
        assert_eq!(shown["workspace"]["state"], "RECLAIMED");
        assert!(!Path::new(shown["workspace"]["managed_path"].as_str().unwrap()).exists());
        let mut artifacts = fixture.cli();
        artifacts.args(["runs", "artifacts", id, "--json"]);
        let files = parsed(&fixture.run(artifacts));
        let files = files["artifacts"].as_array().unwrap();
        assert!(!files.is_empty());
        let mut patch_found = false;
        let mut stdout = None;
        let mut events = None;
        for artifact in files {
            let path = artifact["relative_path"].as_str().unwrap();
            let bytes = fs::read(fixture.path.join("storage").join(path)).unwrap();
            assert_eq!(artifact["size_bytes"], bytes.len() as u64);
            assert_eq!(artifact["sha256"], format!("{:x}", Sha256::digest(&bytes)));
            if path.ends_with("tracked.patch") {
                patch_found = true;
                assert!(String::from_utf8_lossy(&bytes).contains(&format!(
                    "{} ; $(touch INJECTED)",
                    ["first", "second"][index]
                )));
            }
            if path.ends_with("stdout.log") {
                stdout = Some(String::from_utf8(bytes).unwrap());
            } else if path.ends_with("events.jsonl") {
                events = Some(String::from_utf8(bytes).unwrap());
            }
        }
        assert!(patch_found);
        let stdout = stdout.unwrap();
        let native_first = stdout.lines().next().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(native_first).is_ok(),
            index == 0
        );
        let events: Vec<Value> = events
            .unwrap()
            .lines()
            .filter_map(|line| {
                serde_json::from_str::<Value>(line)
                    .unwrap()
                    .get("event")
                    .cloned()
            })
            .collect();
        assert_eq!(events.first().unwrap()["type"], "started");
        assert_eq!(events.last().unwrap()["type"], "completed");
        assert_eq!(events.last().unwrap()["success"], true);
        assert!(events.iter().any(|event| event["type"] == "output"));
        assert!(events.iter().any(|event| event["type"] == "tool"));
    }
    assert_ne!(workspaces[0], workspaces[1]);
    assert_eq!(fs::read(fixture.repo.join("tracked")).unwrap(), b"original");
    assert_eq!(
        fs::read(Path::new(fixture.base["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        b"original"
    );
    assert!(!fixture.repo.join("INJECTED").exists());
    assert!(!fixture.path.join("INJECTED").exists());
}

#[test]
fn managed_parent_waits_for_same_root_children_before_terminating() {
    let mut fixture = Fixture::new();
    let tasks = ["first", "second", "third", "fourth"].map(|id| fixture.task(id, "fake-a"));
    let children = fixture.request(&json!({
        "project":"test","max_concurrency":2,
        "tasks":tasks
    }));
    let controller = fixture.path.join("controller-fake");
    fs::write(
        &controller,
        format!(
            r#"#!/usr/bin/python3
import json, os, pathlib, subprocess, sys, time
root = pathlib.Path({root})
binary = {binary}
fake = {fake}
if "--version" in sys.argv:
    os.execv(fake, [fake, *sys.argv[1:]])
def cli(*args):
    result = subprocess.run(
        [binary, "--root", str(root / "storage"), *args],
        env=dict(PATH="/usr/bin:/bin", HOME=str(root),
                 FABRIC_PLUGIN_REGISTRY=str(root / "plugin-registry")),
        stdin=subprocess.DEVNULL, capture_output=True, timeout=65, check=True)
    return json.loads(result.stdout)
def record(name, value):
    path = root / name
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value))
    temporary.rename(path)
def failed(kind, value, tb):
    import traceback
    (root / "controller-error").write_text(
        "".join(traceback.format_exception(kind, value, tb)) +
        repr(getattr(value, "stderr", None)))
sys.excepthook = failed
accepted = cli("workers", "spawn", "--request", {children},
               "--fake-binary", str(root / "gated-fake"), "--json")
record("children-accepted.json", accepted)
batch = accepted["batch_id"]
status = cli("wait", batch, "--timeout-seconds", "60", "--json")
assert status["terminal"], status
results = cli("results", batch, "--json")
assert results["terminal"] and results["succeeded"], results
record("children-observed.json", results)
deadline = time.monotonic() + 20
while not (root / "release.parent").exists() and not (root / "release-all").exists():
    assert time.monotonic() < deadline, "parent release timed out"
    time.sleep(0.02)
os.execv(fake, [fake, *sys.argv[1:]])
"#,
            root = serde_json::to_string(&fixture.path).unwrap(),
            binary = serde_json::to_string(env!("CARGO_BIN_EXE_fabric")).unwrap(),
            fake = serde_json::to_string(env!("CARGO_BIN_EXE_fabric-fake-harness")).unwrap(),
            children = serde_json::to_string(&children).unwrap(),
        ),
    )
    .unwrap();
    fs::set_permissions(&controller, fs::Permissions::from_mode(0o700)).unwrap();
    let mut task = fixture.task("parent", "fake-a");
    task["limits"] = json!({"timeout_seconds":90});
    let parent_request = fixture.request(&json!({"project":"test","tasks":[task]}));
    let mut command = fixture.cli();
    command
        .args(["workers", "spawn", "--request"])
        .arg(parent_request)
        .arg("--fake-binary")
        .arg(controller)
        .arg("--json");
    fixture.submitted = true;
    let parent = parsed(&fixture.run(command));
    let parent_batch = parent["batch_id"].as_str().unwrap();
    fixture.batches.push(parent_batch.to_owned());
    let marker = |fixture: &Fixture, name: &str| {
        let path = fixture.path.join(name);
        eventually(|| path.exists() || fixture.query("status", parent_batch)["terminal"] == true);
        assert!(
            path.exists(),
            "missing {name}; controller error: {:?}; results: {}",
            fs::read_to_string(fixture.path.join("controller-error")),
            fixture.query("results", parent_batch)
        );
    };
    marker(&fixture, "children-accepted.json");
    let accepted: Value =
        serde_json::from_slice(&fs::read(fixture.path.join("children-accepted.json")).unwrap())
            .unwrap();
    let child_batch = accepted["batch_id"].as_str().unwrap();
    fixture.batches.push(child_batch.to_owned());
    assert_ne!(parent_batch, child_batch);

    // Two children occupy this batch's pool while the parent's own pool stays live.
    for id in ["first", "second"] {
        marker(&fixture, &format!("started.{id}"));
    }
    assert_eq!(fixture.query("status", parent_batch)["state"], "RUNNING");
    let status = fixture.query("status", child_batch);
    for index in [2, 3] {
        assert_eq!(status["tasks"][index]["state"], "QUEUED", "{status}");
    }
    fs::write(fixture.path.join("release.first"), "").unwrap();
    fixture.terminal(accepted["tasks"][0]["run_id"].as_str().unwrap());
    marker(&fixture, "started.third");
    assert!(!fixture.path.join("started.fourth").exists());
    assert_eq!(
        fixture.query("status", child_batch)["tasks"][3]["state"],
        "QUEUED"
    );
    for id in ["second", "third", "fourth"] {
        fs::write(fixture.path.join(format!("release.{id}")), "").unwrap();
    }
    fixture.terminal(child_batch);
    marker(&fixture, "children-observed.json");
    let observed: Value =
        serde_json::from_slice(&fs::read(fixture.path.join("children-observed.json")).unwrap())
            .unwrap();
    assert_eq!(observed, fixture.query("results", child_batch));
    assert_eq!(observed["terminal"], true);
    assert_eq!(observed["succeeded"], true);
    for task in observed["tasks"].as_array().unwrap() {
        assert_eq!(task["state"], "SUCCEEDED");
        let run = &task["result"];
        assert_eq!(run["prepared_state_id"], fixture.base["state_id"]);
        assert!(!Path::new(run["workspace_path"].as_str().unwrap()).exists());
        let artifacts = task["artifacts"].as_array().unwrap();
        assert!(!artifacts.is_empty());
        for artifact in artifacts {
            let bytes = fs::read(
                fixture
                    .path
                    .join("storage")
                    .join(artifact["relative_path"].as_str().unwrap()),
            )
            .unwrap();
            assert_eq!(artifact["size_bytes"], bytes.len() as u64);
            assert_eq!(artifact["sha256"], format!("{:x}", Sha256::digest(bytes)));
        }
    }
    assert_eq!(fixture.query("status", parent_batch)["state"], "RUNNING");
    assert_eq!(fixture.query("results", parent_batch)["terminal"], false);
    let mut stop = fixture.cli();
    stop.args(["manager", "stop", "--json"]);
    assert!(!fixture.run(stop).status.success(), "parent is still owned");
    fs::write(fixture.path.join("release.parent"), "").unwrap();
    fixture.terminal(parent_batch);
    let result = fixture.query("results", parent_batch);
    assert_eq!(result["succeeded"], true, "{result}");
    assert!(
        !Path::new(
            result["tasks"][0]["result"]["workspace_path"]
                .as_str()
                .unwrap()
        )
        .exists()
    );
    let path = fixture.path.clone();
    drop(fixture);
    assert!(
        !path.exists(),
        "fixture failed to stop manager and remove workspaces"
    );
}

impl Fixture {
    fn inventory(&self) -> Vec<i64> {
        let db = rusqlite::Connection::open_with_flags(
            self.path.join("storage/fabric.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        [
            "runs",
            "workspaces",
            "batches",
            "batch_tasks",
            "worker_attempts",
            "async_batches",
            "async_tasks",
            "async_events",
            "batch_task_sources",
            "storage_reservations",
            "worker_leases",
            "cleanup_intents",
        ]
        .map(|table| {
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        })
        .to_vec()
    }

    fn reject(&self, request: &Value, kind: &str) -> Value {
        let path = self.request(request);
        let output = self.run(self.spawn_command(&path));
        assert!(!output.status.success(), "{request}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], kind, "{error}");
        error
    }

    fn native_fixtures(&self) {
        fs::create_dir_all(self.path.join("bin")).unwrap();
        fs::create_dir_all(self.path.join(".omp/agent")).unwrap();
        fs::create_dir_all(self.path.join(".codex")).unwrap();
        fs::write(
            self.path.join(".omp/agent/agent.db"),
            "opaque-omp-auth-sentinel",
        )
        .unwrap();
        fs::write(self.path.join(".omp/agent/config.yml"), "theme: dark\n").unwrap();
        fs::write(
            self.path.join(".codex/auth.json"),
            "opaque-codex-auth-sentinel",
        )
        .unwrap();
        fs::write(
            self.path.join(".codex/config.toml"),
            "# native model stays native\n",
        )
        .unwrap();
        for (harness, version, body) in [
            (
                "omp",
                "omp/19.1.0",
                r#"
test -f "$PI_CODING_AGENT_DIR/agent.db"
test -f "$PI_CODING_AGENT_DIR/config.yml"
printf 'omp native result' > tracked
printf '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"omp fixture output"}]}}\n'
printf '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"omp fixture output"}}\n'
"#,
            ),
            (
                "codex",
                "codex-cli 0.171.0",
                r#"
test "$1" = exec
test -f "$CODEX_HOME/auth.json"
test -f "$CODEX_HOME/config.toml"
printf 'codex native result' > tracked
printf '{"type":"item.completed","item":{"type":"agent_message","text":"codex fixture output"}}\n'
printf '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1,"cached_input_tokens":0}}\n'
"#,
            ),
        ] {
            let executable = self.path.join("bin").join(harness);
            fs::write(
                &executable,
                format!(
                    r#"#!/bin/sh
set -eu
if [ "$#" = 1 ] && [ "$1" = --version ]; then printf '{version}\n'; exit 0; fi
test -z "${{UNSELECTED_SECRET-}}${{OPENAI_API_KEY-}}${{CODEX_API_KEY-}}${{HTTPS_PROXY-}}"
test -n "$HOME"
test -n "$XDG_CONFIG_HOME"
for arg in "$@"; do
  test "$arg" != --model
  test "$arg" != -m
done
printf '%s\n' "$@" > '{root}/native-{harness}.argv'
{body}
"#,
                    root = self.path.display()
                ),
            )
            .unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    fn native_cli(&self) -> Command {
        let mut command = self.cli();
        command.env(
            "PATH",
            format!("{}:/usr/bin:/bin", self.path.join("bin").display()),
        );
        command
    }

    fn connect(&self, harness: &str) {
        let mut command = self.native_cli();
        command.args(["harness", "connect", harness, "--yes", "--json"]);
        assert_eq!(parsed(&self.run(command))["connected"], true);
    }

    fn accept_native(&mut self, request: &Value) -> Value {
        let path = self.request(request);
        let mut command = self.native_cli();
        command
            .args(["workers", "spawn", "--request"])
            .arg(path)
            .arg("--json");
        self.submitted = true;
        let accepted = parsed(&self.run(command));
        assert_eq!(accepted["accepted"], true);
        self.batches
            .push(accepted["batch_id"].as_str().unwrap().to_owned());
        accepted
    }

    fn prepare_current(&self) -> Value {
        let mut prepare = self.cli();
        prepare
            .args(["project", "prepare", "test", "--json", "--recipe"])
            .arg(self.path.join("recipe.toml"));
        parsed(&self.run(prepare))
    }

    fn commit_tracked(&self, contents: &str) {
        fs::write(self.repo.join("tracked"), contents).unwrap();
        self.git(&["add", "tracked"]);
        self.git(&["commit", "--quiet", "-m", contents]);
    }

    fn storage_pressure(&self) {
        let recipe = self.path.join("pressure.toml");
        fs::write(
            &recipe,
            "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf pressure > pressure']\n",
        )
        .unwrap();
        let mut options = fabric_core::workspace::storage::StorageOptions::default();
        options.policy.min_free_bytes = u64::MAX;
        let error = fabric_core::workspace::storage::prepare(
            &self.path.join("storage"),
            "test",
            &recipe,
            &options,
        )
        .unwrap_err();
        assert_eq!(error.kind, fabric_domain::ErrorKind::Storage);
        assert!(error.message.contains("storage_admission"), "{error}");
    }

    fn start_manager(&self) -> Process {
        let mut command = self.cli();
        command.args(["manager", "serve", "--json"]);
        let mut manager = self.spawn(command);
        eventually(|| {
            assert!(
                manager.child.try_wait().unwrap().is_none(),
                "manager exited: {:?}",
                fs::read_to_string(&manager.stderr)
            );
            fs::read_to_string(&manager.stdout)
                .unwrap()
                .lines()
                .next()
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
                .is_some_and(|ready| ready["manager_ready"] == true)
        });
        manager
    }
}

#[test]
fn explicit_prepared_and_exact_sha_capture_after_checkout_disappears() {
    let mut fixture = Fixture::new();
    let a = fixture.base.clone();
    fs::write(fixture.repo.join("tracked"), "commit B\n").unwrap();
    fixture.git(&["add", "tracked"]);
    fixture.git(&["commit", "--quiet", "-m", "B"]);
    let mut prepare = fixture.cli();
    prepare
        .args(["project", "prepare", "test", "--json", "--recipe"])
        .arg(fixture.path.join("recipe.toml"));
    let b = parsed(&fixture.run(prepare));
    assert_ne!(a["source_commit_sha"], b["source_commit_sha"]);
    fs::write(fixture.path.join("release-all"), "").unwrap();
    for (source, base, old_contents) in [
        (json!({"kind":"current-head"}), &b, "commit B"),
        (
            json!({"kind":"prepared","state_id":a["state_id"]}),
            &a,
            "original",
        ),
        (
            json!({"kind":"commit","sha":a["source_commit_sha"]}),
            &a,
            "original",
        ),
    ] {
        if source["kind"] == "prepared" {
            fs::rename(&fixture.repo, fixture.path.join("renamed-checkout")).unwrap();
        } else if source["kind"] == "commit" {
            fs::remove_dir_all(fixture.path.join("renamed-checkout")).unwrap();
        }
        let mut task = fixture.task("first", "fake-a");
        task["source_ref"] = source;
        let accepted = fixture.accept(&json!({"project":"test","tasks":[task]}));
        let batch = accepted["batch_id"].as_str().unwrap();
        fixture.terminal(batch);
        let results = fixture.query("results", batch);
        assert_eq!(results["succeeded"], true, "{results}");
        let run = &results["tasks"][0]["result"];
        assert_eq!(run["prepared_state_id"], base["state_id"]);
        assert_eq!(run["base_commit_sha"], base["source_commit_sha"]);
        let id = results["tasks"][0]["run_id"].as_str().unwrap();
        let mut show = fixture.cli();
        show.args(["runs", "show", id, "--json"]);
        let shown = parsed(&fixture.run(show));
        assert_eq!(shown["workspace"]["state"], "RECLAIMED");
        assert!(!Path::new(shown["workspace"]["managed_path"].as_str().unwrap()).exists());
        let mut artifacts = fixture.cli();
        artifacts.args(["runs", "artifacts", id, "--json"]);
        let files = parsed(&fixture.run(artifacts));
        let mut patch_found = false;
        let mut untracked_found = false;
        for artifact in files["artifacts"].as_array().unwrap() {
            let path = artifact["relative_path"].as_str().unwrap();
            let bytes = fs::read(fixture.path.join("storage").join(path)).unwrap();
            assert_eq!(artifact["size_bytes"], bytes.len() as u64);
            assert_eq!(artifact["sha256"], format!("{:x}", Sha256::digest(&bytes)));
            if path.ends_with("tracked.patch") {
                let patch = String::from_utf8(bytes).unwrap();
                assert!(patch.contains(&format!("-{old_contents}")), "{patch}");
                assert!(patch.contains("+first ; $(touch INJECTED)"), "{patch}");
                patch_found = true;
            } else if artifact["kind"] == "untracked_archive" {
                let restored = fixture
                    .path
                    .join(format!("restored-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
                fs::create_dir(&restored).unwrap();
                checked(
                    &Command::new("/usr/bin/tar")
                        .arg("-xpf")
                        .arg(fixture.path.join("storage").join(path))
                        .arg("-C")
                        .arg(&restored)
                        .output()
                        .unwrap(),
                );
                assert_eq!(fs::read(restored.join("new-source")).unwrap(), b"new-first");
                untracked_found = true;
            }
        }
        assert!(patch_found, "tracked patch missing: {files}");
        assert!(untracked_found, "untracked artifact missing: {files}");
        for (base, contents) in [(&a, b"original".as_slice()), (&b, b"commit B\n".as_slice())] {
            assert_eq!(
                fs::read(Path::new(base["prepared_path"].as_str().unwrap()).join("tracked"))
                    .unwrap(),
                contents
            );
            assert!(
                !Path::new(base["prepared_path"].as_str().unwrap())
                    .join("new-source")
                    .exists()
            );
        }
        assert!(!fixture.path.join("INJECTED").exists());
    }
    let before = fixture.inventory();
    fixture.reject(
        &json!({"project":"test","tasks":[fixture.task("first","fake-a")]}),
        "source",
    );
    assert_eq!(fixture.inventory(), before);
}

#[test]
fn unsupported_placement_and_late_task_preflight_allocate_nothing() {
    let fixture = Fixture::new();
    let valid = json!({"project":"test","tasks":[fixture.task("first","fake-a"),fixture.task("second","fake-b")]});
    let before = fixture.inventory();
    for placement in ["remote", "cloud", "byoc"] {
        let mut request = valid.clone();
        request["placement"] = json!(placement);
        fixture.reject(&request, "unsupported_placement");
        assert_eq!(fixture.inventory(), before);
        assert!(!fixture.path.join("storage/manager.lock").exists());
    }
    for (field, value, kind) in [
        (
            "model",
            json!("cannot-enforce-this"),
            "unsupported_capability",
        ),
        ("model", json!(""), "invalid_arguments"),
        (
            "limits",
            json!({"max_cost_usd":0.01}),
            "unsupported_capability",
        ),
        ("limits", json!({"timeout_seconds":0}), "invalid_arguments"),
        ("limits", json!({"max_log_bytes":0}), "invalid_arguments"),
        (
            "limits",
            json!({"max_artifact_bytes":0}),
            "invalid_arguments",
        ),
        (
            "source_ref",
            json!({"kind":"prepared","state_id":"ps-not-supported"}),
            "source",
        ),
        (
            "source_ref",
            json!({"kind":"commit","sha":"a".repeat(40)}),
            "source",
        ),
        ("resume", json!("previous-session"), "invalid_arguments"),
        ("skills", json!(["portable"]), "invalid_arguments"),
        (
            "execution_context",
            json!({"kind":"native","plugins":["portable"]}),
            "invalid_arguments",
        ),
        (
            "Portable profile.skills/plugins/mcp are no longer supported",
            json!(true),
            "invalid_arguments",
        ),
    ] {
        let mut request = valid.clone();
        request["tasks"][1][field] = value;
        fixture.reject(&request, kind);
        assert_eq!(fixture.inventory(), before, "{request}");
        assert!(!fixture.path.join("storage/manager.lock").exists());
        assert!(!fixture.path.join("started.first").exists());
    }
    for directory in [
        "workspaces",
        "fork-staging",
        "artifact-staging",
        "artifacts",
    ] {
        let path = fixture.path.join("storage").join(directory);
        assert!(!path.exists() || fs::read_dir(path).unwrap().next().is_none());
    }
}

#[test]
fn supported_explicit_model_is_observed_and_default_concurrency_is_serial() {
    let mut fixture = Fixture::new();
    let mut first = fixture.task("first", "fake-a");
    first["model"] = json!("literal-model ; $(touch MODEL_INJECTED)");
    first["source_ref"] = json!({"kind":"current-head"});
    let accepted = fixture.accept(&json!({"project":"test","placement":"local",
        "tasks":[first,fixture.task("second","fake-b")]}));
    eventually(|| fixture.path.join("started.first").exists());
    let batch = accepted["batch_id"].as_str().unwrap();
    let status = fixture.query("status", batch);
    assert_eq!(status["tasks"][1]["state"], "QUEUED");
    assert!(!fixture.path.join("started.second").exists());
    fs::write(fixture.path.join("release.first"), "").unwrap();
    eventually(|| fixture.path.join("started.second").exists());
    fs::write(fixture.path.join("release.second"), "").unwrap();
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["succeeded"], true);
    let attempt = &results["tasks"][0]["result"]["worker_attempts"][0];
    assert_eq!(
        attempt["model_requested"],
        "literal-model ; $(touch MODEL_INJECTED)"
    );
    assert_eq!(attempt["model_resolved"], attempt["model_requested"]);
    let artifacts = fixture.path.join("storage/artifacts").join(
        results["tasks"][0]["result"]["artifact_manifest_id"]
            .as_str()
            .unwrap(),
    );
    let native: Vec<Value> = fs::read_to_string(artifacts.join("stdout.log"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(native.iter().any(|event| event["kind"] == "model"
        && event["model"] == "literal-model ; $(touch MODEL_INJECTED)"));
    assert!(results["tasks"][1]["result"]["worker_attempts"][0]["model_requested"].is_null());
    assert!(results["tasks"][1]["result"]["worker_attempts"][0]["model_resolved"].is_null());
    assert!(!fixture.path.join("MODEL_INJECTED").exists());
}

#[test]
fn generic_omp_and_codex_use_native_defaults_and_explicit_context_without_provider_calls() {
    let mut fixture = Fixture::new();
    fixture.native_fixtures();
    fixture.connect("omp");
    fixture.connect("codex");
    let prompt = "literal ; $(touch NATIVE_INJECTED) 'quoted'\nno provider call";
    for explicit in [false, true] {
        let mut tasks = json!([
            {"task_id":"omp-native","harness_id":"omp","prompt":prompt},
            {"task_id":"codex-native","harness_id":"codex","prompt":prompt}
        ]);
        if explicit {
            tasks[0]["execution_context"] = json!({
                "kind":"explicit","harness_version":"omp/19.1.0",
                "native_config_refs":[{"kind":"native-agent-dir","path":fixture.path.join(".omp/agent")}],
                "connection_refs":[{"kind":"native-auth","name":"native-home"}]
            });
            tasks[1]["execution_context"] = json!({
                "kind":"explicit","harness_version":"codex-cli 0.171.0",
                "native_config_refs":[{"kind":"native-config-dir","path":fixture.path.join(".codex")}],
                "connection_refs":[{"kind":"native-auth","name":"native-config-dir"}]
            });
        } else {
            tasks[1]["execution_context"] = json!({"kind":"native"});
        }
        let accepted =
            fixture.accept_native(&json!({"project":"test","tasks":tasks,"max_concurrency":2}));
        let batch = accepted["batch_id"].as_str().unwrap();
        fixture.terminal(batch);
        let results = fixture.query("results", batch);
        assert_eq!(results["succeeded"], true, "{results}");
        for (index, harness) in ["omp", "codex"].iter().enumerate() {
            let result = &results["tasks"][index]["result"];
            assert_eq!(result["harness"], *harness);
            assert_eq!(result["state"], "SUCCEEDED");
            assert!(result["worker_attempts"][0]["model_requested"].is_null());
            assert!(result["worker_attempts"][0]["model_resolved"].is_null());
            assert_eq!(result["profile"]["portable"], false);
            assert_eq!(result["profile"]["digest"].as_str().unwrap().len(), 64);
            assert!(!Path::new(result["workspace_path"].as_str().unwrap()).exists());
            let argv =
                fs::read_to_string(fixture.path.join(format!("native-{harness}.argv"))).unwrap();
            assert!(argv.contains(prompt), "{argv}");
            let root = fixture
                .path
                .join("storage/artifacts")
                .join(result["artifact_manifest_id"].as_str().unwrap());
            assert!(
                fs::read_to_string(root.join("tracked.patch"))
                    .unwrap()
                    .contains(&format!("{harness} native result"))
            );
            let events: Vec<Value> = fs::read_to_string(root.join("events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(
                events
                    .iter()
                    .any(|record| record["event"]["type"] == "output"
                        && record["event"]["text"] == format!("{harness} fixture output")),
                "{events:?}"
            );
        }
    }
    assert!(!fixture.path.join("NATIVE_INJECTED").exists());
    assert_eq!(fs::read(fixture.repo.join("tracked")).unwrap(), b"original");
}

#[test]
fn unverifiable_native_model_rejects_before_manager_admission() {
    let fixture = Fixture::new();
    fixture.native_fixtures();
    fixture.connect("codex");
    let before = fixture.inventory();
    let path = fixture.request(&json!({"project":"test","tasks":[
        {"task_id":"native","harness_id":"codex","prompt":"fixture only","model":"unattested-model"}
    ]}));
    let mut command = fixture.native_cli();
    command
        .args(["workers", "spawn", "--request"])
        .arg(path)
        .arg("--json");
    let output = fixture.run(command);
    assert!(!output.status.success(), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "unsupported_capability");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MODEL_UNVERIFIED")
    );
    assert_eq!(fixture.inventory(), before);
    assert!(!fixture.path.join("storage/manager.lock").exists());
    assert!(!fixture.path.join("native-codex.argv").exists());
}

#[test]
fn per_task_timeout_survives_submission_and_does_not_cancel_healthy_sibling() {
    let mut fixture = Fixture::new();
    let mut first = fixture.task("first", "fake-a");
    first["limits"] = json!({"timeout_seconds":1});
    let mut second = fixture.task("second", "fake-b");
    second["limits"] = json!({"timeout_seconds":10});
    let accepted =
        fixture.accept(&json!({"project":"test","max_concurrency":2,"tasks":[first,second]}));
    eventually(|| fixture.path.join("started.first").exists());
    eventually(|| fixture.path.join("started.second").exists());
    let first = accepted["tasks"][0]["run_id"].as_str().unwrap();
    fixture.terminal(first);
    let failed = fixture.query("results", first);
    assert_eq!(failed["succeeded"], false);
    assert_eq!(failed["tasks"][0]["result"]["error_kind"], "timeout");
    assert_eq!(
        failed["tasks"][0]["result"]["worker_attempts"][0]["termination_reason"],
        "timeout"
    );
    assert!(
        !Path::new(
            failed["tasks"][0]["result"]["workspace_path"]
                .as_str()
                .unwrap()
        )
        .exists()
    );
    assert_eq!(
        fixture.query("status", accepted["batch_id"].as_str().unwrap())["terminal"],
        false
    );
    fs::write(fixture.path.join("release.second"), "").unwrap();
    fixture.terminal(accepted["batch_id"].as_str().unwrap());
    let results = fixture.query("results", accepted["batch_id"].as_str().unwrap());
    assert_eq!(results["succeeded"], false);
    assert_eq!(results["tasks"][1]["state"], "SUCCEEDED");
}

#[test]
fn per_task_log_and_artifact_limits_bound_capture_without_losing_failure_evidence() {
    let mut fixture = Fixture::new();
    let mut first = fixture.task("first", "fake-a");
    let mut prompt: Value = serde_json::from_str(first["prompt"].as_str().unwrap()).unwrap();
    prompt["events"] = json!([{"kind":"output","body":"x".repeat(8192)}]);
    first["prompt"] = json!(prompt.to_string());
    first["limits"] = json!({"max_log_bytes":512,"max_artifact_bytes":1048576});
    let mut second = fixture.task("second", "fake-b");
    second["limits"] = json!({"max_artifact_bytes":32});
    let accepted =
        fixture.accept(&json!({"project":"test","max_concurrency":2,"tasks":[first,second]}));
    fs::write(fixture.path.join("release-all"), "").unwrap();
    let batch = accepted["batch_id"].as_str().unwrap();
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["succeeded"], false);
    let first = &results["tasks"][0]["result"];
    assert_eq!(first["state"], "SUCCEEDED");
    assert_eq!(first["capture"]["limit_bytes"], 512);
    assert_eq!(first["capture"]["truncated"], true);
    assert!(first["capture"]["persisted_bytes"].as_u64().unwrap() <= 512);
    assert!(first["capture"]["stdout_bytes_seen"].as_u64().unwrap() > 512);
    let second = &results["tasks"][1];
    assert_eq!(second["state"], "NEEDS_ATTENTION");
    let mut show = fixture.cli();
    show.args(["runs", "show", second["run_id"].as_str().unwrap(), "--json"]);
    let shown = parsed(&fixture.run(show));
    assert_eq!(shown["run"]["error_kind"], "artifact_capture");
    assert_eq!(shown["workspace"]["state"], "RECOVERY_REQUIRED");
    let retained = Path::new(shown["workspace"]["managed_path"].as_str().unwrap());
    assert!(retained.exists());
    assert_eq!(
        fs::read_to_string(retained.join("tracked")).unwrap(),
        "second ; $(touch INJECTED) 'quoted'\nsecond line"
    );
    assert!(shown["run"]["artifact_manifest_id"].is_null());
}

#[test]
fn v1_submit_keeps_its_response_and_nonempty_portable_profiles_require_native_migration() {
    let mut fixture = Fixture::new();
    let legacy = json!({"version":1,"op":"spawn_many","project":"test","concurrency":1,"default_harness":"fake-a",
        "tasks":[{"id":"first","prompt":fixture.task("first","fake-a")["prompt"]}],
        "options":{"copy_limit_bytes":10485760}});
    let path = fixture.request(&legacy);
    let mut command = fixture.cli();
    command
        .args(["submit", "--request"])
        .arg(path)
        .arg("--json")
        .arg("--fake-binary")
        .arg(fixture.path.join("gated-fake"));
    fixture.submitted = true;
    let accepted = parsed(&fixture.run(command));
    fixture
        .batches
        .push(accepted["batch_id"].as_str().unwrap().to_owned());
    assert_eq!(accepted["accepted"], true);
    assert_eq!(accepted["run_ids"].as_array().unwrap().len(), 1);
    assert!(accepted.get("tasks").is_none());
    fs::write(fixture.path.join("release-all"), "").unwrap();
    fixture.terminal(accepted["batch_id"].as_str().unwrap());
    assert_eq!(
        fixture.query("results", accepted["run_ids"][0].as_str().unwrap())["succeeded"],
        true
    );
    let path = fixture.request(&legacy);
    let mut command = fixture.cli();
    command
        .args(["api", "--request"])
        .arg(path)
        .arg("--json")
        .arg("--fake-binary")
        .arg(fixture.path.join("gated-fake"));
    let response = parsed(&fixture.run(command));
    assert_eq!(response["version"], 1);
    assert_eq!(response["op"], "spawn_many");
    assert_eq!(response["batch"]["succeeded"], true);
    assert_eq!(response["batch"]["tasks"][0]["id"], "first");
    assert_eq!(response["batch"]["tasks"][0]["result"]["harness"], "fake-a");
    assert_eq!(
        response["batch"]["tasks"][0]["result"]["state"],
        "SUCCEEDED"
    );
    let before = fixture.inventory();
    for field in ["skills", "plugins", "mcp"] {
        let mut request = legacy.clone();
        request["tasks"][0]["request"] = json!({"profile":{
            "harness_id":"fake-a","harness_version":"fabric-fake-a 1",
            "skills":[],"plugins":[],"mcp":[]
        }});
        request["tasks"][0]["request"]["profile"][field] = json!(["old-portable-import"]);
        let path = fixture.request(&request);
        let mut command = fixture.cli();
        command
            .args(["submit", "--request"])
            .arg(path)
            .arg("--json");
        let output = fixture.run(command);
        assert!(!output.status.success(), "{output:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "unsupported_capability");
        let message = error["error"]["message"].as_str().unwrap().to_lowercase();
        assert!(
            message.contains("native") && message.contains("harness"),
            "{error}"
        );
        assert_eq!(fixture.inventory(), before);
    }
}

#[test]
fn source_ownership_readiness_exact_sha_and_missing_head_reject_before_admission() {
    let fixture = Fixture::new();
    let mut add = fixture.cli();
    add.args(["project", "add", "--name", "other", "--path"])
        .arg(&fixture.repo);
    checked(&fixture.run(add));
    let db = rusqlite::Connection::open(fixture.path.join("storage/fabric.db")).unwrap();
    db.execute(
        "INSERT INTO prepared_states
         SELECT 'ps-not-ready', project_id, preparation_key || '-pending',
         'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', recipe_digest, harness_environment_digest,
         prepared_path || '-pending', 'PREPARING', created_at, NULL, size_bytes
         FROM prepared_states WHERE state_id = ?1",
        [fixture.base["state_id"].as_str().unwrap()],
    )
    .unwrap();
    drop(db);
    let before = fixture.inventory();
    for (project, source, kind) in [
        (
            "test",
            json!({"kind":"prepared","state_id":"ps-unknown"}),
            "source",
        ),
        (
            "other",
            json!({"kind":"prepared","state_id":fixture.base["state_id"]}),
            "source",
        ),
        (
            "test",
            json!({"kind":"prepared","state_id":"ps-not-ready"}),
            "source",
        ),
        (
            "test",
            json!({"kind":"commit","sha":"a".repeat(40)}),
            "source",
        ),
        (
            "test",
            json!({"kind":"commit","sha":"b".repeat(40)}),
            "source",
        ),
        (
            "test",
            json!({"kind":"commit","sha":fixture.base["source_commit_sha"].as_str().unwrap()[..12]}),
            "invalid_arguments",
        ),
        (
            "test",
            json!({"kind":"commit","sha":"z".repeat(40)}),
            "invalid_arguments",
        ),
        (
            "test",
            json!({"kind":"prepared","state_id":fixture.repo}),
            "invalid_id",
        ),
        (
            "test",
            json!({"kind":"path","path":fixture.repo}),
            "invalid_arguments",
        ),
    ] {
        let mut task = fixture.task("second", "fake-b");
        task["source_ref"] = source;
        fixture.reject(&json!({"project":project,"tasks":[task]}), kind);
        assert_eq!(fixture.inventory(), before);
    }
    fixture.commit_tracked("unprepared C\n");
    fixture.reject(
        &json!({"project":"test","tasks":[fixture.task("first","fake-a")]}),
        "source",
    );
    let mut exact_c = fixture.task("first", "fake-a");
    let output = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(&fixture.repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    checked(&output);
    exact_c["source_ref"] =
        json!({"kind":"commit","sha":String::from_utf8(output.stdout).unwrap().trim()});
    fixture.reject(&json!({"project":"test","tasks":[exact_c]}), "source");
    assert_eq!(fixture.inventory(), before);
    fs::rename(&fixture.repo, fixture.path.join("missing-checkout")).unwrap();
    fixture.reject(
        &json!({"project":"test","tasks":[fixture.task("first","fake-a")]}),
        "source",
    );
    assert_eq!(fixture.inventory(), before);
    let container = Path::new(fixture.base["prepared_path"].as_str().unwrap())
        .parent()
        .unwrap();
    fs::set_permissions(container, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(container, fixture.path.join("missing-prepared-base")).unwrap();
    for source in [
        json!({"kind":"prepared","state_id":fixture.base["state_id"]}),
        json!({"kind":"commit","sha":fixture.base["source_commit_sha"]}),
    ] {
        let mut task = fixture.task("first", "fake-a");
        task["source_ref"] = source;
        fixture.reject(&json!({"project":"test","tasks":[task]}), "source");
        assert_eq!(fixture.inventory(), before);
    }
    assert!(!fixture.path.join("storage/manager.lock").exists());
    assert!(!fixture.path.join("started.first").exists());
    assert!(!fixture.path.join("started.second").exists());
    for directory in [
        "workspaces",
        "fork-staging",
        "artifacts",
        "artifact-staging",
    ] {
        let path = fixture.path.join("storage").join(directory);
        assert!(!path.exists() || fs::read_dir(path).unwrap().next().is_none());
    }
}

#[test]
fn dirty_checkout_is_reported_excluded_and_preserved_with_durable_source_provenance() {
    let mut fixture = Fixture::new();
    fs::write(fixture.repo.join("tracked"), "PRIVATE dirty tracked\n").unwrap();
    fs::write(
        fixture.repo.join("private-untracked"),
        "PRIVATE untracked\n",
    )
    .unwrap();
    let path = fixture.request(&json!({"project":"test","tasks":[fixture.task("first","fake-a")]}));
    fixture.submitted = true;
    let output = fixture.run(fixture.spawn_command(&path));
    let accepted = parsed(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("dirty"),
        "{output:?}"
    );
    let batch = accepted["batch_id"].as_str().unwrap();
    fixture.batches.push(batch.to_owned());
    eventually(|| fixture.path.join("started.first").exists());
    fs::write(fixture.path.join("release.first"), "").unwrap();
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["succeeded"], true, "{results}");
    let run = &results["tasks"][0]["result"];
    assert_eq!(
        run["source"],
        json!({
            "source_ref":{"kind":"current-head"},
            "prepared_state_id":fixture.base["state_id"],
            "base_commit_sha":fixture.base["source_commit_sha"],
            "checkout_dirty":true,"dirty_changes_excluded":true
        })
    );
    let artifacts = fixture
        .path
        .join("storage/artifacts")
        .join(run["artifact_manifest_id"].as_str().unwrap());
    let patch = fs::read_to_string(artifacts.join("tracked.patch")).unwrap();
    assert!(patch.contains("-original"), "{patch}");
    assert!(!patch.contains("PRIVATE"), "{patch}");
    let listing = Command::new("/usr/bin/tar")
        .arg("-tf")
        .arg(artifacts.join("untracked.tar"))
        .output()
        .unwrap();
    checked(&listing);
    assert!(!String::from_utf8_lossy(&listing.stdout).contains("private-untracked"));
    assert_eq!(
        fs::read(fixture.repo.join("tracked")).unwrap(),
        b"PRIVATE dirty tracked\n"
    );
    assert_eq!(
        fs::read(fixture.repo.join("private-untracked")).unwrap(),
        b"PRIVATE untracked\n"
    );
    assert_eq!(
        fs::read(Path::new(fixture.base["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        b"original"
    );
    let mut stop = fixture.cli();
    stop.args(["manager", "stop", "--json"]);
    checked(&fixture.run(stop));
    assert_eq!(fixture.query("results", batch), results);
}

#[test]
fn inline_native_prompt_and_source_flags_preserve_literal_bytes_and_defaults() {
    let mut fixture = Fixture::new();
    fixture.native_fixtures();
    fixture.connect("codex");
    let prompt = "literal ; $(touch INLINE_INJECTED) 'quoted' \"double\"\nsecond line\n";
    for selector in [None, Some("--prepared-state"), Some("--commit")] {
        let mut command = fixture.native_cli();
        command.args([
            "workers",
            "spawn",
            "--project",
            "test",
            "--harness",
            "codex",
            "--prompt",
            prompt,
            "--json",
        ]);
        if let Some(selector) = selector {
            let value = if selector == "--prepared-state" {
                &fixture.base["state_id"]
            } else {
                &fixture.base["source_commit_sha"]
            };
            command.args([
                selector,
                value.as_str().unwrap(),
                "--task-id",
                "inline-literal",
            ]);
        }
        fixture.submitted = true;
        let accepted = parsed(&fixture.run(command));
        let batch = accepted["batch_id"].as_str().unwrap();
        fixture.batches.push(batch.to_owned());
        assert_eq!(accepted["accepted"], true);
        assert_eq!(accepted["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(
            accepted["tasks"][0]["task_id"],
            if selector.is_none() {
                "task"
            } else {
                "inline-literal"
            }
        );
        fixture.terminal(batch);
        let results = fixture.query("results", batch);
        assert_eq!(results["succeeded"], true, "{results}");
        let result = &results["tasks"][0]["result"];
        assert_eq!(result["prepared_state_id"], fixture.base["state_id"]);
        assert!(result["worker_attempts"][0]["model_requested"].is_null());
        assert!(result["worker_attempts"][0]["model_resolved"].is_null());
        assert_eq!(result["profile"]["portable"], false);
        let argv = fs::read_to_string(fixture.path.join("native-codex.argv")).unwrap();
        assert!(argv.contains(prompt), "{argv}");
        assert!(!Path::new(result["workspace_path"].as_str().unwrap()).exists());
    }
    assert!(!fixture.path.join("INLINE_INJECTED").exists());
    assert!(!fixture.repo.join("INLINE_INJECTED").exists());
    assert!(
        !Path::new(fixture.base["prepared_path"].as_str().unwrap())
            .join("INLINE_INJECTED")
            .exists()
    );
}

#[test]
fn inline_request_source_and_remote_conflicts_allocate_nothing() {
    let fixture = Fixture::new();
    fixture.native_fixtures();
    fixture.connect("codex");
    let path = fixture.request(&json!({"project":"test","tasks":[{"task_id":"native","harness_id":"codex","prompt":"fixture"}]}));
    let before = fixture.inventory();
    for inline_field in [
        vec!["--project", "test"],
        vec!["--harness", "codex"],
        vec!["--prompt", "literal"],
        vec!["--task-id", "task"],
        vec!["--model", "literal"],
        vec![
            "--prepared-state",
            fixture.base["state_id"].as_str().unwrap(),
        ],
        vec![
            "--commit",
            fixture.base["source_commit_sha"].as_str().unwrap(),
        ],
    ] {
        let mut command = fixture.native_cli();
        command
            .args(["workers", "spawn", "--request"])
            .arg(&path)
            .args(inline_field)
            .arg("--json");
        let output = fixture.run(command);
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(fixture.inventory(), before);
    }
    for extra in [
        vec![
            "--prepared-state",
            fixture.base["state_id"].as_str().unwrap(),
            "--commit",
            fixture.base["source_commit_sha"].as_str().unwrap(),
        ],
        vec!["--placement", "remote"],
    ] {
        let mut command = fixture.native_cli();
        command
            .args([
                "workers",
                "spawn",
                "--project",
                "test",
                "--harness",
                "codex",
                "--prompt",
                "literal",
                "--json",
            ])
            .args(extra);
        let output = fixture.run(command);
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(fixture.inventory(), before);
    }
    assert!(!fixture.path.join("storage/manager.lock").exists());
    assert!(!fixture.path.join("native-codex.argv").exists());
}

#[test]
fn mixed_queued_sources_are_frozen_and_all_pins_survive_restart_and_eviction() {
    let mut fixture = Fixture::new();
    let mut a_candidates = vec![fixture.base.clone()];
    for marker in ["A2", "A3"] {
        fs::write(fixture.path.join("recipe.toml"), format!("version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready; printf {marker} > .deps/marker']\n")).unwrap();
        a_candidates.push(fixture.prepare_current());
    }
    fixture.commit_tracked("commit B\n");
    let mut b_candidates = vec![fixture.prepare_current()];
    fs::write(fixture.path.join("recipe.toml"), "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready; printf B2 > .deps/marker']\n").unwrap();
    b_candidates.push(fixture.prepare_current());
    let root = fixture.path.join("storage");
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    let mut unselected = a_candidates.into_iter().filter(|state| {
        store
            .cache_retention(&state["state_id"].as_str().unwrap().parse().unwrap())
            .unwrap()
            .is_none()
    });
    let a = unselected.next().unwrap();
    let disposable = unselected.next().unwrap();
    let b = b_candidates
        .into_iter()
        .find(|state| {
            store
                .cache_retention(&state["state_id"].as_str().unwrap().parse().unwrap())
                .unwrap()
                .is_none()
        })
        .unwrap();
    drop(store);
    let mut manager = fixture.start_manager();
    let mut tasks = [
        fixture.task("first", "fake-a"),
        fixture.task("second", "fake-b"),
        fixture.task("third", "fake-a"),
        fixture.task("fourth", "fake-b"),
    ];
    tasks[0]["source_ref"] = json!({"kind":"prepared","state_id":a["state_id"]});
    tasks[1]["source_ref"] = json!({"kind":"prepared","state_id":b["state_id"]});
    tasks[2]["source_ref"] = json!({"kind":"current-head"});
    tasks[3]["source_ref"] = json!({"kind":"commit","sha":a["source_commit_sha"]});
    let accepted = fixture.accept(&json!({"project":"test","max_concurrency":1,"tasks":tasks}));
    let batch = accepted["batch_id"].as_str().unwrap();
    eventually(|| fixture.path.join("started.first").exists());
    let (current_a, current_b) = {
        let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
        for base in [&a, &b] {
            assert_eq!(
                store
                    .cache_retention(&base["state_id"].as_str().unwrap().parse().unwrap())
                    .unwrap()
                    .as_deref(),
                Some("historical_reference")
            );
        }
        let project = b["project_id"].as_str().unwrap().parse().unwrap();
        (
            store
                .ready_state_for_commit(&project, a["source_commit_sha"].as_str().unwrap())
                .unwrap()
                .unwrap(),
            store
                .ready_state_for_commit(&project, b["source_commit_sha"].as_str().unwrap())
                .unwrap()
                .unwrap(),
        )
    };
    let pins = {
        let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
        let mut statement = db
            .prepare(
                "SELECT s.prepared_state_id, s.provenance_json FROM batch_task_sources s
             JOIN batch_tasks t USING(run_id) WHERE t.batch_id = ?1 ORDER BY t.position",
            )
            .unwrap();
        let pins = statement
            .query_map([batch], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            pins.iter()
                .map(|(state, _)| state.as_str())
                .collect::<Vec<_>>(),
            [
                a["state_id"].as_str().unwrap(),
                b["state_id"].as_str().unwrap(),
                current_b.state_id.as_str(),
                current_a.state_id.as_str(),
            ],
        );
        for (state, source) in &pins {
            assert_eq!(
                serde_json::from_str::<Value>(source).unwrap()["prepared_state_id"],
                *state
            );
        }
        assert!(
            db.execute(
                "UPDATE batch_task_sources SET prepared_state_id = prepared_state_id
             WHERE run_id IN (SELECT run_id FROM batch_tasks WHERE batch_id = ?1)",
                [batch],
            )
            .is_err()
        );
        assert!(
            db.execute(
                "DELETE FROM batch_task_sources
             WHERE run_id IN (SELECT run_id FROM batch_tasks WHERE batch_id = ?1)",
                [batch],
            )
            .is_err()
        );
        pins
    };
    assert!(!fixture.path.join("started.second").exists());
    manager.child.kill().unwrap();
    manager.child.wait().unwrap();
    fixture.commit_tracked("unprepared C\n");
    fs::write(fixture.repo.join("tracked"), "dirty after admission\n").unwrap();
    fixture.storage_pressure();
    assert!(
        !Path::new(disposable["prepared_path"].as_str().unwrap()).exists(),
        "pressure did not exercise cache eviction"
    );
    for base in [&a, &b] {
        assert!(Path::new(base["prepared_path"].as_str().unwrap()).exists());
        let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
        assert_eq!(
            store
                .cache_retention(&base["state_id"].as_str().unwrap().parse().unwrap())
                .unwrap()
                .as_deref(),
            Some("historical_reference")
        );
    }
    manager = fixture.start_manager();
    eventually(|| fixture.path.join("started.second").exists());
    fixture.storage_pressure();
    for base in [&a, &b] {
        assert!(Path::new(base["prepared_path"].as_str().unwrap()).exists());
    }
    fs::write(fixture.path.join("release-all"), "").unwrap();
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["tasks"][0]["state"], "NEEDS_ATTENTION");
    let current_a_id = json!(current_a.state_id);
    let current_b_id = json!(current_b.state_id);
    for (index, (state_id, sha, old_contents)) in [
        (&b["state_id"], &b["source_commit_sha"], "commit B"),
        (&current_b_id, &b["source_commit_sha"], "commit B"),
        (&current_a_id, &a["source_commit_sha"], "original"),
    ]
    .into_iter()
    .enumerate()
    {
        let task = &results["tasks"][index + 1];
        assert_eq!(task["state"], "SUCCEEDED", "{results}");
        let run = &task["result"];
        assert_eq!(
            run["source"],
            serde_json::from_str::<Value>(&pins[index + 1].1).unwrap()
        );
        assert_eq!(&run["prepared_state_id"], state_id);
        assert_eq!(&run["base_commit_sha"], sha);
        assert_eq!(
            run["source"]["checkout_dirty"],
            if index == 1 {
                json!(false)
            } else {
                Value::Null
            }
        );
        let artifacts = root
            .join("artifacts")
            .join(run["artifact_manifest_id"].as_str().unwrap());
        let patch = fs::read_to_string(artifacts.join("tracked.patch")).unwrap();
        assert!(patch.contains(&format!("-{old_contents}")), "{patch}");
        assert!(!patch.contains("dirty after admission"));
        assert!(!Path::new(run["workspace_path"].as_str().unwrap()).exists());
    }
    eventually(|| fixture.path.join("stopped.first").exists());
    assert_eq!(
        fs::read(fixture.repo.join("tracked")).unwrap(),
        b"dirty after admission\n"
    );
    let mut stop = fixture.cli();
    stop.args(["manager", "stop", "--json"]);
    checked(&fixture.run(stop));
    assert!(manager.output(DEADLINE).is_some());
    assert_eq!(fixture.query("results", batch), results);
}

#[test]
fn idempotent_source_submission_reconnects_after_checkout_disappears() {
    let mut fixture = Fixture::new();
    let path = fixture.request(&json!({
        "project":"test","tasks":[fixture.task("first","fake-a")]
    }));
    let mut submit = fixture.spawn_command(&path);
    submit.args(["--idempotency-key", "frozen-source"]);
    fixture.submitted = true;
    let accepted = parsed(&fixture.run(submit));
    let batch = accepted["batch_id"].as_str().unwrap();
    fixture.batches.push(batch.to_owned());
    fs::write(fixture.path.join("release-all"), "").unwrap();
    fixture.terminal(batch);
    fs::rename(&fixture.repo, fixture.path.join("missing-checkout")).unwrap();
    for restart in [false, true] {
        if restart {
            let mut stop = fixture.cli();
            stop.args(["manager", "stop", "--json"]);
            checked(&fixture.run(stop));
        }
        let mut retry = fixture.spawn_command(&path);
        retry.args(["--idempotency-key", "frozen-source"]);
        let original = parsed(&fixture.run(retry));
        assert_eq!(original["batch_id"], accepted["batch_id"]);
        assert_eq!(
            original["tasks"][0]["run_id"],
            accepted["tasks"][0]["run_id"]
        );
        assert_eq!(fixture.query("results", batch)["succeeded"], true);
    }
}

#[test]
fn manager_stop_acknowledges_release_before_immediate_reconnect() {
    let mut fixture = Fixture::new();
    let path = fixture.request(&json!({
        "project":"test","tasks":[fixture.task("first","fake-a")]
    }));
    fs::write(fixture.path.join("release-all"), "").unwrap();
    fixture.submitted = true;
    let accepted = parsed(&fixture.run(fixture.spawn_command(&path)));
    let batch = accepted["batch_id"].as_str().unwrap();
    fixture.batches.push(batch.to_owned());
    fixture.terminal(batch);
    let expected = fixture.query("results", batch);
    assert_eq!(expected["succeeded"], true);
    for cycle in 0..20 {
        let mut stop = fixture.cli();
        stop.args(["manager", "stop", "--json"]);
        let output = fixture.run(stop);
        assert!(output.status.success(), "stop cycle {cycle}: {output:?}");
        let mut reconnect = fixture.cli();
        reconnect.args(["results", batch, "--json"]);
        let output = fixture.run(reconnect);
        assert!(
            output.status.success(),
            "results after stop cycle {cycle}: {output:?}"
        );
        assert_eq!(parsed(&output), expected, "cycle {cycle}");
    }
}
