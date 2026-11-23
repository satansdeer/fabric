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
    fn output(&mut self) -> Output {
        self.output_before(DEADLINE).unwrap_or_else(|| {
            panic!(
                "CLI timed out or output was unavailable: {:?}",
                fs::read_to_string(&self.stderr)
            )
        })
    }

    fn output_before(&mut self, timeout: Duration) -> Option<Output> {
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

    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
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

struct Fixture {
    path: PathBuf,
    repo: PathBuf,
    base: Value,
    batches: Vec<String>,
    manager: Option<Process>,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fabric-async-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(path).unwrap();
        let mut fixture = Self {
            repo: path.join("repo ; literal"),
            path,
            base: Value::Null,
            batches: Vec::new(),
            manager: None,
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
        let output = Command::new("/usr/bin/git")
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
            .unwrap();
        checked(&output);
    }

    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fabric"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.path)
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
        self.spawn(command).output()
    }

    fn query(&self, verb: &str, id: &str) -> Value {
        let mut command = self.cli();
        command.args([verb, id, "--json"]);
        parsed(&self.run(command))
    }

    fn wrapper(&self) {
        let script = format!(
            r#"#!/bin/sh
set -eu
if [ "$1" = --version ]; then exec '{fake}' "$@"; fi
test "$1" = --prompt-file
prompt=$(cat "$2")
found=no
for id in {ids}; do
  case "$prompt" in *\"content\":\"task-$id\"*) found=yes; break;; esac
done
test "$found" = yes
root='{root}'
printf 'start\n' >> "$root/started.$id"
trap 'touch "$root/stopped.$id"' EXIT
trap 'touch "$root/signalled.$id"; exit 143' TERM INT
i=0
while [ ! -f "$root/release.$id" ] && [ ! -f "$root/release-all" ]; do
  printf . >> "$root/heartbeat.$id"
  i=$((i+1)); test "$i" -lt 1500
  sleep 0.02
done
'{fake}' "$@"
"#,
            fake = env!("CARGO_BIN_EXE_fabric-fake-harness"),
            ids = (0..64).map(|i| i.to_string()).collect::<Vec<_>>().join(" "),
            root = self.path.display()
        );
        let executable = self.path.join("gated-fake");
        fs::write(&executable, script).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn request(&self, labels: &[usize], concurrency: usize, fail: Option<usize>) -> PathBuf {
        let tasks: Vec<_> = labels
            .iter()
            .map(|i| {
                json!({
                    "id":format!("task-{i}"), "prompt":json!({
                        "required_files":[".deps/ready"],
                        "writes":[{"path":"tracked","content":format!("task-{i}")},
                                  {"path":"new-source","content":format!("new-{i}")}],
                        "events":[{"type":"fixture","task":i}],
                        "absent_environment":["UNSELECTED_SECRET"],
                        "exit_code":if fail == Some(*i) {7} else {0}
                    }).to_string()
                })
            })
            .collect();
        let path = self.path.join(format!(
            "request-{}.json",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version":1,"op":"spawn_many","project":"test","default_harness":"fake",
                "concurrency":concurrency,"tasks":tasks,
                "options":{"copy_limit_bytes":10485760,"timeout_seconds":45}
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    fn submit_command(&self, request: &Path, key: Option<&str>) -> Command {
        let mut command = self.cli();
        command
            .args(["submit", "--request"])
            .arg(request)
            .arg("--json")
            .arg("--fake-binary")
            .arg(self.path.join("gated-fake"));
        if let Some(key) = key {
            command.args(["--idempotency-key", key]);
        }
        command
    }

    fn accept(&mut self, request: &Path, key: Option<&str>) -> Value {
        let accepted = parsed(&self.run(self.submit_command(request, key)));
        assert_eq!(accepted["accepted"], true);
        assert!(!accepted["run_ids"].as_array().unwrap().is_empty());
        self.batches
            .push(accepted["batch_id"].as_str().unwrap().to_owned());
        accepted
    }

    fn release(&self, label: usize) {
        fs::write(self.path.join(format!("release.{label}")), "").unwrap();
    }

    fn started(&self, label: usize) {
        eventually(|| self.path.join(format!("started.{label}")).exists());
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
    fn start_manager(&mut self) {
        assert!(self.manager.is_none());
        let mut command = self.cli();
        command.args(["manager", "serve", "--json"]);
        let mut process = self.spawn(command);
        eventually(|| {
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "manager exited: {:?}",
                fs::read_to_string(&process.stderr)
            );
            fs::read_to_string(&process.stdout)
                .unwrap()
                .lines()
                .next()
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
                .is_some_and(|ready| ready["manager_ready"] == true)
        });
        self.manager = Some(process);
    }

    fn crash_manager(&mut self) {
        // This Child is ours; no PID obtained from persisted run metadata is signalled.
        self.manager.take().unwrap().kill();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::write(self.path.join("release-all"), "");
        // Gates let even an orphaned worker exit without signalling a reconstructed PID.
        for id in &self.batches {
            let mut cancel = self.cli();
            cancel.args(["cancel", id, "--json"]);
            let _ = self.spawn(cancel).output_before(Duration::from_secs(2));
        }
        let start = Instant::now();
        let stopped = loop {
            let mut stop = self.cli();
            stop.args(["manager", "stop", "--json"]);
            if self
                .spawn(stop)
                .output_before(Duration::from_secs(2))
                .is_some_and(|out| out.status.success())
            {
                break true;
            }
            if start.elapsed() > Duration::from_secs(10) || self.batches.is_empty() {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let owned_manager = self.manager.is_some();
        self.manager.take();
        if !stopped && !owned_manager && !self.batches.is_empty() {
            // A detached manager that refused to stop may still own work; retain its root.
            return;
        }
        let start = Instant::now();
        while (0..64).any(|i| {
            self.path.join(format!("started.{i}")).exists()
                && !self.path.join(format!("stopped.{i}")).exists()
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

fn checked(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}

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
fn four_workers_survive_submitter_exit_and_reconnect_with_captured_artifacts() {
    let mut fixture = Fixture::new();
    let request = fixture.request(&[0, 1, 2, 3], 4, None);
    // No foreground manager: submit must auto-start one and then actually exit.
    let accepted = fixture.accept(&request, None);
    fs::remove_file(request).unwrap();
    assert_eq!(accepted["run_ids"].as_array().unwrap().len(), 4);
    for i in 0..4 {
        fixture.started(i);
    }
    let batch = accepted["batch_id"].as_str().unwrap();
    let status = fixture.query("status", batch);
    assert_eq!(status["terminal"], false, "all workers are still gated");
    assert_eq!(status["tasks"].as_array().unwrap().len(), 4);
    assert_eq!(fixture.query("results", batch)["succeeded"], Value::Null);
    for i in 0..4 {
        fixture.release(i);
    }
    assert_eq!(fixture.terminal(batch)["terminal"], true);
    let results = fixture.query("results", batch);
    assert_eq!(results["succeeded"], true);
    for (i, task) in results["tasks"].as_array().unwrap().iter().enumerate() {
        assert_eq!(task["task_id"], format!("task-{i}"));
        assert_eq!(task["run_id"], accepted["run_ids"][i]);
        assert_eq!(task["state"], "SUCCEEDED");
        assert_eq!(
            task["result"]["prepared_state_id"],
            fixture.base["state_id"]
        );
        let run = task["run_id"].as_str().unwrap();
        let mut show = fixture.cli();
        show.args(["runs", "show", run, "--json"]);
        let shown = parsed(&fixture.run(show));
        assert_eq!(shown["workspace"]["state"], "RECLAIMED");
        assert!(!Path::new(shown["workspace"]["managed_path"].as_str().unwrap()).exists());
        let mut artifacts = fixture.cli();
        artifacts.args(["runs", "artifacts", run, "--json"]);
        let files = parsed(&fixture.run(artifacts));
        let artifacts = files["artifacts"].as_array().unwrap();
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
            assert_eq!(artifact["sha256"], format!("{:x}", Sha256::digest(&bytes)));
            if artifact["relative_path"]
                .as_str()
                .unwrap()
                .ends_with("tracked.patch")
            {
                assert!(String::from_utf8_lossy(&bytes).contains(&format!("task-{i}")));
            }
        }
    }
    assert_eq!(fs::read(fixture.repo.join("tracked")).unwrap(), b"original");
    assert_eq!(
        fs::read(Path::new(fixture.base["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        b"original"
    );
}

#[test]
fn results_expose_verified_artifacts_readable_from_parent_cwd_and_reject_tampering() {
    let mut fixture = Fixture::new();
    let parent = fixture.path.join("unrelated-parent");
    fs::create_dir(&parent).unwrap();
    let request = fixture.request(&[0, 1], 2, None);
    let accepted = fixture.accept(&request, None);
    let batch = accepted["batch_id"].as_str().unwrap();
    for i in 0..2 {
        fixture.started(i);
    }
    for task in fixture.query("results", batch)["tasks"].as_array().unwrap() {
        assert!(task["artifacts"].as_array().unwrap().is_empty());
    }
    for i in 0..2 {
        fixture.release(i);
    }
    fixture.terminal(batch);
    let mut command = fixture.cli();
    command
        .current_dir(&parent)
        .args(["workers", "results", batch, "--json"]);
    let results = parsed(&fixture.run(command));
    assert_eq!(results["succeeded"], true);
    let mut patches = Vec::new();
    for (i, task) in results["tasks"].as_array().unwrap().iter().enumerate() {
        assert_eq!(task["run_id"], accepted["run_ids"][i]);
        let artifact = task["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["kind"] == "tracked_patch")
            .unwrap();
        let path = PathBuf::from(artifact["local_read_path"].as_str().unwrap());
        assert!(path.is_absolute());
        assert_eq!(
            path,
            fixture
                .path
                .join("storage")
                .join(artifact["relative_path"].as_str().unwrap())
        );
        // The consumer uses only the returned reference, with no storage-root resolution.
        let output = Command::new("/bin/cat")
            .current_dir(&parent)
            .arg(&path)
            .output()
            .unwrap();
        checked(&output);
        assert_eq!(artifact["size_bytes"], output.stdout.len() as u64);
        assert_eq!(
            artifact["sha256"],
            format!("{:x}", Sha256::digest(&output.stdout))
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("+task-{i}")));
        patches.push((path, output.stdout));
    }
    assert_ne!(patches[0].0, patches[1].0);
    assert_ne!(patches[0].1, patches[1].1);
    let denied = || {
        let mut command = fixture.cli();
        command
            .current_dir(&parent)
            .args(["results", batch, "--json"]);
        let output = fixture.run(command);
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
    };
    let (path, original) = &patches[0];
    let directory = path.parent().unwrap();
    let outside = parent.join("outside.patch");
    fs::write(&outside, original).unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(path).unwrap();
    std::os::unix::fs::symlink(&outside, path).unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o500)).unwrap();
    denied();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(path).unwrap();
    let mut altered = original.clone();
    altered[0] ^= 1; // Same length, different immutable content.
    fs::write(path, altered).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o400)).unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o500)).unwrap();
    denied();
    // Inspection remains persisted metadata only, even when content verification fails.
    let mut inspect = fixture.cli();
    inspect.args([
        "runs",
        "artifacts",
        accepted["run_ids"][0].as_str().unwrap(),
        "--json",
    ]);
    let inventory = parsed(&fixture.run(inspect));
    assert!(
        inventory["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file.get("local_read_path").is_none())
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(path, original).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o400)).unwrap();
    // Isolated fixture corruption; production metadata remains immutable.
    let db = rusqlite::Connection::open(fixture.path.join("storage/fabric.db")).unwrap();
    db.execute_batch("DROP TRIGGER immutable_artifact_update;")
        .unwrap();
    db.execute(
        "UPDATE artifacts SET relative_path = '../unrelated-parent/outside.patch'
         WHERE run_id = ?1 AND kind = 'tracked_patch'",
        [accepted["run_ids"][0].as_str().unwrap()],
    )
    .unwrap();
    denied();
}

#[test]
fn partial_failure_keeps_healthy_siblings_running_and_preserves_each_result() {
    let mut fixture = Fixture::new();
    let request = fixture.request(&[0, 1, 2], 3, Some(0));
    let accepted = fixture.accept(&request, None);
    let batch = accepted["batch_id"].as_str().unwrap();
    for i in 0..3 {
        fixture.started(i);
    }
    fixture.release(0);
    let failed = accepted["run_ids"][0].as_str().unwrap();
    assert_eq!(fixture.terminal(failed)["state"], "FAILED");
    assert_eq!(fixture.query("status", batch)["terminal"], false);
    for i in 1..3 {
        assert!(!fixture.path.join(format!("signalled.{i}")).exists());
        fixture.release(i);
    }
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["succeeded"], false);
    assert_eq!(results["tasks"][0]["state"], "FAILED");
    assert_eq!(results["tasks"][0]["result"]["exit_code"], 7);
    for task in &results["tasks"].as_array().unwrap()[1..] {
        assert_eq!(task["state"], "SUCCEEDED");
        assert_eq!(task["result"]["exit_code"], 0);
        assert!(!task["result"]["artifact_manifest_id"].is_null());
    }
}

#[test]
fn owned_run_and_batch_cancellation_never_launches_cancelled_queued_tasks() {
    let mut fixture = Fixture::new();
    let request = fixture.request(&[0, 1], 2, None);
    let accepted = fixture.accept(&request, None);
    for i in 0..2 {
        fixture.started(i);
    }
    let cancelled = accepted["run_ids"][0].as_str().unwrap();
    let acknowledgement = fixture.query("cancel", cancelled);
    assert_eq!(acknowledgement["id"], cancelled);
    assert_eq!(acknowledgement["acknowledged"], true);
    assert_eq!(acknowledgement["outcome"], "cancelled");
    assert_eq!(fixture.terminal(cancelled)["state"], "CANCELLED");
    assert!(!fixture.path.join("signalled.1").exists());
    assert_eq!(
        fixture.query("status", accepted["run_ids"][1].as_str().unwrap())["terminal"],
        false
    );
    fixture.release(1);
    let batch = accepted["batch_id"].as_str().unwrap();
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["tasks"][0]["state"], "CANCELLED");
    assert_eq!(results["tasks"][1]["state"], "SUCCEEDED");
    assert_eq!(
        fixture.query("cancel", cancelled)["outcome"],
        "already_terminal"
    );

    let request = fixture.request(&[10, 11, 12], 1, None);
    let accepted = fixture.accept(&request, None);
    fixture.started(10);
    let batch = accepted["batch_id"].as_str().unwrap();
    assert_eq!(fixture.query("cancel", batch)["outcome"], "cancelled");
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["succeeded"], false);
    for task in results["tasks"].as_array().unwrap() {
        assert_eq!(task["state"], "CANCELLED");
    }
    assert!(!fixture.path.join("started.11").exists());
    assert!(!fixture.path.join("started.12").exists());
    assert_eq!(
        fixture.query("cancel", batch)["outcome"],
        "already_terminal"
    );
}

#[test]
fn concurrent_batches_keep_independent_bounded_worker_pools() {
    let mut fixture = Fixture::new();
    let first_request = fixture.request(&[0, 1], 1, None);
    let first = fixture.accept(&first_request, None);
    fixture.started(0);
    let next_request = fixture.request(&[10, 11, 12], 2, None);
    let next = fixture.accept(&next_request, None);
    for label in [10, 11] {
        fixture.started(label);
    }
    assert!(!fixture.path.join("started.1").exists());
    assert!(!fixture.path.join("started.12").exists());
    assert_eq!(
        fixture.query("status", first["batch_id"].as_str().unwrap())["tasks"][1]["state"],
        "QUEUED"
    );
    let next_id = next["batch_id"].as_str().unwrap();
    assert_eq!(
        fixture.query("status", next_id)["tasks"][2]["state"],
        "QUEUED"
    );
    fixture.release(10);
    fixture.terminal(next["run_ids"][0].as_str().unwrap());
    fixture.started(12);
    assert!(!fixture.path.join("started.1").exists());
    for label in [11, 12] {
        fixture.release(label);
    }
    fixture.terminal(next_id);
    assert_eq!(fixture.query("results", next_id)["succeeded"], true);
    assert_eq!(
        fixture.query("status", first["batch_id"].as_str().unwrap())["terminal"],
        false
    );
    fixture.release(0);
    fixture.started(1);
    fixture.release(1);
    fixture.terminal(first["batch_id"].as_str().unwrap());
    assert_eq!(
        fixture.query("results", first["batch_id"].as_str().unwrap())["succeeded"],
        true
    );
}

fn cursor(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .expect("event cursor must be an ordered nonnegative integer")
}

fn check_status(status: &Value) {
    let state = status["state"].as_str().unwrap();
    assert!(!state.is_empty());
    assert_eq!(state, state.to_ascii_uppercase());
    assert!(status["terminal"].is_boolean());
    let events = status["events"].as_array().unwrap();
    assert!(events.len() <= 1000, "status must bound event delivery");
    let mut previous = None;
    for event in events {
        let current = cursor(&event["cursor"]);
        if let Some(previous) = previous {
            assert!(current > previous);
        }
        previous = Some(current);
        assert!(event["state"].is_string());
    }
    if let Some(previous) = previous {
        assert!(cursor(&status["next_cursor"]) >= previous);
    }
}

#[test]
fn wait_is_bounded_and_fresh_clients_resume_ordered_events_after_cursor() {
    let mut fixture = Fixture::new();
    let request = fixture.request(&[0], 1, None);
    let accepted = fixture.accept(&request, None);
    fixture.started(0);
    let batch = accepted["batch_id"].as_str().unwrap();
    let initial = fixture.query("status", batch);
    check_status(&initial);
    assert!(!initial["events"].as_array().unwrap().is_empty());
    let mut wait = fixture.cli();
    wait.args(["wait", batch, "--timeout-seconds", "1", "--json"]);
    let start = Instant::now();
    let waiting = parsed(&fixture.run(wait));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "bounded wait exceeded timeout grace"
    );
    assert_eq!(waiting["terminal"], false);
    check_status(&waiting);
    let after = waiting["next_cursor"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| waiting["next_cursor"].to_string());
    fixture.release(0);
    fixture.terminal(batch);
    let mut status = fixture.cli();
    status.args(["status", batch, "--json", "--after", &after]);
    let resumed = parsed(&fixture.run(status));
    check_status(&resumed);
    assert_eq!(resumed["terminal"], true);
    assert!(!resumed["events"].as_array().unwrap().is_empty());
    for event in resumed["events"].as_array().unwrap() {
        assert!(cursor(&event["cursor"]) > cursor(&waiting["next_cursor"]));
    }
    let run = fixture.query("status", accepted["run_ids"][0].as_str().unwrap());
    check_status(&run);
    assert_eq!(run["state"], "SUCCEEDED");
}

#[test]
fn idempotency_and_concurrent_clients_preserve_unique_durable_handles() {
    let mut fixture = Fixture::new();
    let request = fixture.request(&[0, 1], 2, None);
    let mut first = fixture.spawn(fixture.submit_command(&request, Some("same-key")));
    let mut second = fixture.spawn(fixture.submit_command(&request, Some("same-key")));
    let one = parsed(&first.output());
    let two = parsed(&second.output());
    fixture
        .batches
        .push(one["batch_id"].as_str().unwrap().to_owned());
    fixture
        .batches
        .push(two["batch_id"].as_str().unwrap().to_owned());
    assert_eq!(one["accepted"], true);
    assert_eq!(one["batch_id"], two["batch_id"]);
    assert_eq!(one["run_ids"], two["run_ids"]);
    let duplicate_path = fixture.request(&[0, 1], 2, None);
    let duplicate = fixture.accept(&duplicate_path, Some("same-key"));
    assert_eq!(
        one["batch_id"], duplicate["batch_id"],
        "path is not part of logical identity"
    );
    assert_eq!(one["run_ids"], duplicate["run_ids"]);
    let different = fixture.request(&[0, 1], 2, Some(1));
    let rejected = fixture.run(fixture.submit_command(&different, Some("same-key")));
    assert!(
        !rejected.status.success(),
        "idempotency conflict was accepted: {rejected:?}"
    );

    let left_request = fixture.request(&[10, 11], 2, None);
    let right_request = fixture.request(&[20, 21], 2, None);
    let mut left = fixture.spawn(fixture.submit_command(&left_request, None));
    let mut right = fixture.spawn(fixture.submit_command(&right_request, None));
    let left = parsed(&left.output());
    let right = parsed(&right.output());
    fixture
        .batches
        .push(left["batch_id"].as_str().unwrap().to_owned());
    fixture
        .batches
        .push(right["batch_id"].as_str().unwrap().to_owned());
    assert_ne!(left["batch_id"], right["batch_id"]);
    let mut run_ids = std::collections::HashSet::new();
    for accepted in [&one, &left, &right] {
        for run in accepted["run_ids"].as_array().unwrap() {
            assert!(run_ids.insert(run.as_str().unwrap()));
        }
    }
    fs::write(fixture.path.join("release-all"), "").unwrap();
    for accepted in [&one, &left, &right] {
        let batch = accepted["batch_id"].as_str().unwrap();
        fixture.terminal(batch);
        assert_eq!(fixture.query("results", batch)["succeeded"], true);
    }
    for i in [0, 1, 10, 11, 20, 21] {
        assert_eq!(
            fs::read_to_string(fixture.path.join(format!("started.{i}"))).unwrap(),
            "start\n"
        );
    }
}

#[test]
fn manager_restart_replays_only_queued_work_once_and_retains_uncertain_workspace() {
    let mut fixture = Fixture::new();
    fixture.start_manager();
    let request = fixture.request(&[0, 1, 2, 3], 1, None);
    let accepted = fixture.accept(&request, None);
    fixture.started(0);
    for i in 1..4 {
        assert!(!fixture.path.join(format!("started.{i}")).exists());
    }
    fixture.crash_manager();
    fixture.start_manager();
    let first = accepted["run_ids"][0].as_str().unwrap();
    let status = fixture.query("status", first);
    assert_eq!(status["state"], "NEEDS_ATTENTION");
    assert_eq!(fixture.query("cancel", first)["outcome"], "not_owned");
    let mut show = fixture.cli();
    show.args(["runs", "show", first, "--json"]);
    let shown = parsed(&fixture.run(show));
    let workspace = PathBuf::from(shown["workspace"]["managed_path"].as_str().unwrap());
    assert!(
        workspace.exists(),
        "uncertain active worker workspace was reclaimed"
    );
    assert_ne!(shown["workspace"]["state"], "RECLAIMED");
    fs::write(fixture.path.join("release-all"), "").unwrap();
    let batch = accepted["batch_id"].as_str().unwrap();
    fixture.terminal(batch);
    let results = fixture.query("results", batch);
    assert_eq!(results["succeeded"], false);
    assert_eq!(results["tasks"][0]["state"], "NEEDS_ATTENTION");
    assert!(results["tasks"][0]["result"].is_null());
    for task in &results["tasks"].as_array().unwrap()[1..] {
        assert_eq!(task["state"], "SUCCEEDED");
    }
    for i in 0..4 {
        assert_eq!(
            fs::read_to_string(fixture.path.join(format!("started.{i}"))).unwrap(),
            "start\n"
        );
    }
    eventually(|| fixture.path.join("stopped.0").exists());
    assert!(workspace.exists());
    // A second restart cannot replay either completed work or the uncertain first run.
    let mut stop = fixture.cli();
    stop.args(["manager", "stop", "--json"]);
    checked(&fixture.run(stop));
    fixture.manager.take();
    fixture.start_manager();
    assert_eq!(fixture.query("results", batch), results);
}

#[test]
fn running_manager_crash_never_signals_a_worker_from_persisted_pid_metadata() {
    let mut fixture = Fixture::new();
    fixture.start_manager();
    let request = fixture.request(&[0], 1, None);
    let accepted = fixture.accept(&request, None);
    fixture.started(0);
    let heartbeat = fixture.path.join("heartbeat.0");
    eventually(|| fs::metadata(&heartbeat).is_ok_and(|m| m.len() > 1));
    fixture.crash_manager();
    fixture.start_manager();
    let run = accepted["run_ids"][0].as_str().unwrap();
    assert_eq!(fixture.query("status", run)["state"], "NEEDS_ATTENTION");
    let cancellation = fixture.query("cancel", run);
    assert_eq!(cancellation["outcome"], "not_owned");
    assert_eq!(cancellation["acknowledged"], false);
    let before = fs::metadata(&heartbeat).unwrap().len();
    eventually(|| fs::metadata(&heartbeat).is_ok_and(|m| m.len() > before));
    assert!(
        !fixture.path.join("signalled.0").exists(),
        "recovery signalled an unowned worker"
    );
    let mut show = fixture.cli();
    show.args(["runs", "show", run, "--json"]);
    let shown = parsed(&fixture.run(show));
    let workspace = PathBuf::from(shown["workspace"]["managed_path"].as_str().unwrap());
    assert!(workspace.exists());
    assert_ne!(shown["workspace"]["state"], "RECLAIMED");
    let results = fixture.query("results", run);
    assert_eq!(results["succeeded"], false);
    assert_eq!(results["tasks"][0]["state"], "NEEDS_ATTENTION");
    assert!(results["tasks"][0]["result"].is_null());
    fixture.release(0);
    eventually(|| fixture.path.join("stopped.0").exists());
    assert_eq!(fixture.query("status", run)["state"], "NEEDS_ATTENTION");
    assert_eq!(
        fs::read_to_string(fixture.path.join("started.0")).unwrap(),
        "start\n"
    );
    assert!(
        workspace.exists(),
        "uncertainty must retain original workspace protection"
    );
}

#[test]
fn live_logs_survive_truncation_viewer_interrupt_and_owned_cancellation() {
    let mut fixture = Fixture::new();
    let wrapper = fixture.path.join("gated-fake");
    let script = fs::read_to_string(&wrapper).unwrap().replace(
        "  sleep 0.02",
        "  printf 'PRIVATE-RAW-CONTENT-0123456789\\n'\n  sleep 0.02",
    );
    fs::write(wrapper, script).unwrap();
    let request = fixture.request(&[0], 1, None);
    let mut value: Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
    value["options"]["timeout_seconds"] = json!(7200);
    value["options"]["max_log_bytes"] = json!(1024);
    fs::write(&request, serde_json::to_vec(&value).unwrap()).unwrap();
    let accepted = fixture.accept(&request, Some("live-observations"));
    let run = accepted["run_ids"][0].as_str().unwrap();
    let repeat = fixture.accept(&request, Some("live-observations"));
    assert_eq!(accepted["run_ids"], repeat["run_ids"]);
    value["options"]["timeout_seconds"] = json!(7201);
    fs::write(&request, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        !fixture
            .run(fixture.submit_command(&request, Some("live-observations")))
            .status
            .success()
    );
    fixture.started(0);
    let logs = |args: &[&str]| {
        let mut command = fixture.cli();
        command.args(["runs", "logs", run, "--json"]).args(args);
        fixture.run(command)
    };
    let mut live = Value::Null;
    eventually(|| {
        live = parsed(&logs(&[]));
        live["progress"]["activity"]["capture_truncated"] == true
    });
    assert_eq!(live["state"], "RUNNING");
    assert_eq!(live["progress"]["timeout_seconds"], 7200);
    assert_eq!(live["batch_id"], accepted["batch_id"]);
    assert_eq!(live["verified_progress"], "not_assessed");
    assert_eq!(live["process_observation"], "observed_alive_at_timestamp");
    assert!(!live["configured_deadline_at_ms"].is_null());
    assert!(!live.to_string().contains("PRIVATE-RAW"));
    let observed = live["progress"]["activity"]["stdout_bytes"]
        .as_u64()
        .unwrap();
    let cursor = live["next_cursor"].as_u64().unwrap().to_string();
    eventually(|| {
        let report = parsed(&logs(&["--after", &cursor]));
        report["changed"] == true
            && report["progress"]["activity"]["stdout_bytes"]
                .as_u64()
                .unwrap()
                > observed
    });
    let first = logs(&["--raw", "stdout", "--limit-bytes", "7"]);
    assert!(String::from_utf8_lossy(&first.stderr).contains("SENSITIVE LOCAL RAW LOGS"));
    let first = parsed(&first);
    assert_eq!(first["raw"], "PRIVATE");
    assert_eq!(first["next_cursor"], 7);
    let next = parsed(&logs(&[
        "--raw",
        "stdout",
        "--limit-bytes",
        "4",
        "--after",
        "7",
    ]));
    assert_eq!(next["raw"], "-RAW");
    for args in [
        vec!["--limit-bytes", "0"],
        vec!["--limit-bytes", "65537"],
        vec!["--after", "18446744073709551615"],
        vec!["--raw", "stdout", "--after", "9999999"],
        vec!["--raw", "../../foreign"],
    ] {
        assert!(!logs(&args).status.success(), "{args:?}");
    }
    let mut show = fixture.cli();
    show.args(["runs", "show", run, "--json"]);
    let shown = parsed(&fixture.run(show));
    let tree = PathBuf::from(shown["workspace"]["managed_path"].as_str().unwrap());
    let container = tree.parent().unwrap();
    let stderr = container.join("stderr.log");
    let backup = container.join("stderr.saved");
    fs::rename(&stderr, &backup).unwrap();
    std::os::unix::fs::symlink(fixture.path.join("heartbeat.0"), &stderr).unwrap();
    assert!(!logs(&["--raw", "stderr"]).status.success());
    fs::remove_file(&stderr).unwrap();
    fs::create_dir(&stderr).unwrap();
    assert!(!logs(&["--raw", "stderr"]).status.success());
    fs::remove_dir(&stderr).unwrap();
    fs::rename(backup, stderr).unwrap();
    let marker = container.join(".fabric-owner");
    let original = fs::read(&marker).unwrap();
    fs::write(&marker, b"foreign ownership").unwrap();
    assert!(!logs(&["--raw", "stdout"]).status.success());
    fs::write(marker, original).unwrap();
    let mut follow = fixture.cli();
    follow.args([
        "runs",
        "logs",
        run,
        "--raw",
        "stdout",
        "--follow",
        "--limit-bytes",
        "32",
    ]);
    let mut viewer = fixture.spawn(follow);
    eventually(|| fs::metadata(&viewer.stdout).is_ok_and(|metadata| metadata.len() > 0));
    // Signal only the viewer Child owned by this fixture, not a persisted worker PID.
    assert_eq!(
        unsafe { libc::kill(viewer.child.id() as i32, libc::SIGINT) },
        0
    );
    checked(&viewer.output());
    assert_eq!(fixture.query("status", run)["terminal"], false);
    assert!(!fixture.path.join("signalled.0").exists());
    assert_eq!(fixture.query("cancel", run)["acknowledged"], true);
    assert_eq!(fixture.terminal(run)["state"], "CANCELLED");
    eventually(|| fixture.path.join("signalled.0").exists());
    let retained = parsed(&logs(&[]));
    assert_eq!(retained["workspace_state"], "RECLAIMED");
    assert_eq!(retained["process_observation"], "observed_exited");
    assert!(!retained["progress"]["cleanup_observed_at_ms"].is_null());
    assert!(
        retained["progress"]["activity"]["persisted_bytes"]
            .as_u64()
            .unwrap()
            <= 1024
    );
    assert!(!tree.exists());
    let raw = parsed(&logs(&["--raw", "stdout", "--limit-bytes", "65536"]));
    assert!(raw["raw"].as_str().unwrap().contains("PRIVATE-RAW"));
    assert_eq!(raw["raw_eof"], true);
}
