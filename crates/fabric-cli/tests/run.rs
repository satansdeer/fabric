use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fabric-run-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fabric"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.0)
            .env("UNSELECTED_SECRET", "must-not-leak")
            .arg("--root")
            .arg(self.0.join("storage"));
        command
    }
    fn git(&self, repo: &Path, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        success(&out);
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn setup(&self) -> (PathBuf, serde_json::Value) {
        let repo = self.0.join("repo ; literal");
        fs::create_dir(&repo).unwrap();
        self.git(&repo, &["init", "--quiet"]);
        fs::write(repo.join("tracked"), "original").unwrap();
        fs::write(repo.join(".gitignore"), ".deps/\n").unwrap();
        self.git(&repo, &["add", "."]);
        self.git(&repo, &["commit", "--quiet", "-m", "initial"]);
        success(
            &self
                .cli()
                .args(["project", "add", "--name", "test", "--path"])
                .arg(&repo)
                .output()
                .unwrap(),
        );
        (repo, self.prepare())
    }
    fn prepare(&self) -> serde_json::Value {
        let recipe = self.0.join("recipe.toml");
        fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n").unwrap();
        json(
            &self
                .cli()
                .args(["project", "prepare", "test", "--json", "--recipe"])
                .arg(recipe)
                .output()
                .unwrap(),
        )
    }
    fn run(&self, exit: i32) -> Output {
        let prompt = self.0.join("prompt ; $(touch INJECTED).json");
        fs::write(&prompt, serde_json::to_vec(&serde_json::json!({
            "required_files": [".deps/ready"],
            "writes": [{"path":"tracked", "content":"worker tracked"}, {"path":"new source ; literal", "content":"worker untracked"}],
            "events": [{"type":"fixture", "value":42, "payload":"x".repeat(128 * 1024)}], "exit_code":exit,
            "absent_environment": ["HOME", "UNSELECTED_SECRET"],
        })).unwrap()).unwrap();
        self.cli()
            .args(["run", "test", "--harness", "fake", "--prompt-file"])
            .arg(prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap()
    }
    fn script_fake(&self, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let executable = self.0.join("fake-script");
        fs::write(&executable, format!(
            "#!/bin/sh\nset -eu\numask 077\nif [ \"$1\" = --version ]; then printf 'fabric-fake-harness 1\\n'; exit 0; fi\n{body}\n"
        )).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        executable
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fn writable(path: &Path) {
            use std::os::unix::fs::PermissionsExt;
            if fs::symlink_metadata(path).unwrap().is_dir() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
                for entry in fs::read_dir(path).unwrap() {
                    writable(&entry.unwrap().path());
                }
            }
        }
        writable(&self.0);
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn success(out: &Output) {
    assert!(out.status.success(), "{out:?}");
}
fn json(out: &Output) -> serde_json::Value {
    success(out);
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn wall_timeout_stops_fake_parent_and_term_ignoring_child_and_persists_reason() {
    use fabric_domain::{RunId, RunState};
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("timeout.json");
    fs::write(&prompt, r#"{"process_tree":{"ignore_child_term":true}}"#).unwrap();
    let start = std::time::Instant::now();
    let out = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--prompt-file"])
        .arg(prompt)
        .args([
            "--timeout-seconds",
            "1",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("timeout must return durable run report: {out:?}"));
    assert_eq!(report["state"], "FAILED");
    assert_eq!(report["error_kind"], "timeout");
    assert!(report["exit_code"].is_null());
    assert!(report["exit_signal"].as_i64().is_some());
    assert!(start.elapsed() < std::time::Duration::from_secs(6));
    let workspace = Path::new(report["workspace_path"].as_str().unwrap());
    assert_tree_stopped(workspace);
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let run = store
        .run(&RunId::try_from(report["run_id"].as_str().unwrap().to_owned()).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::Failed);
    assert_eq!(run.error_kind.as_deref(), Some("timeout"));
    assert_eq!(run.exit_code, None);
    assert_eq!(
        verified_manifest(&fixture, &report)["stop_reason"],
        "timeout"
    );
    assert_eq!(
        run.exit_signal,
        report["exit_signal"].as_i64().map(|signal| signal as i32)
    );
}

fn verified_manifest(fixture: &Fixture, report: &serde_json::Value) -> serde_json::Value {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let root = fixture.0.join("storage");
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    let id = report["run_id"].as_str().unwrap().parse().unwrap();
    let run = store.run(&id).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&run.artifact_manifest_id).unwrap(),
        report["artifact_manifest_id"]
    );
    let files = store.artifacts(&id).unwrap();
    assert_eq!(files.len(), 7);
    let mut manifest = None;
    for file in files {
        let path = root.join(&file.relative_path);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, file.size_bytes);
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), file.sha256);
        let metadata = fs::symlink_metadata(path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o400);
        assert_eq!(metadata.nlink(), 1);
        if file.kind == "manifest" {
            manifest = Some(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
        }
    }
    let manifest = manifest.unwrap();
    assert_eq!(manifest["run_id"], report["run_id"]);
    assert_eq!(manifest["workspace_id"], report["workspace_id"]);
    assert_eq!(manifest["base_commit_sha"], report["base_commit_sha"]);
    assert_eq!(manifest["prompt_digest"], report["prompt_digest"]);
    assert_eq!(manifest["harness_version"], report["harness_version"]);
    assert_eq!(manifest["exit_code"], report["exit_code"]);
    assert_eq!(manifest["exit_signal"], report["exit_signal"]);
    assert_eq!(manifest["capture"], report["capture"]);
    assert_eq!(manifest["source_scope"], "tracked_only");
    assert_eq!(manifest["recipe_digest"].as_str().unwrap().len(), 64);
    manifest
}

fn assert_tree_stopped(workspace: &Path) {
    let paths = [
        workspace.join("parent.heartbeat"),
        workspace.join("child.heartbeat"),
    ];
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    assert!(before.iter().all(|bytes| !bytes.is_empty()));
    std::thread::sleep(std::time::Duration::from_millis(200));
    for (path, expected) in paths.iter().zip(before) {
        assert_eq!(
            fs::read(path).unwrap(),
            expected,
            "writer survived: {path:?}"
        );
    }
}

fn wait_for_tree(fixture: &Fixture) -> PathBuf {
    let start = std::time::Instant::now();
    loop {
        if let Ok(entries) = fs::read_dir(fixture.0.join("storage/fork-staging")) {
            for entry in entries {
                let tree = entry.unwrap().path().join("tree");
                if fs::metadata(tree.join("parent.heartbeat")).is_ok_and(|meta| meta.len() > 0) {
                    return tree;
                }
            }
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "worker was not ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn ctrl_c_cancels_live_worker_and_preserves_observed_exit() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("cancel.json");
    fs::write(&prompt, r#"{"process_tree":{"ignore_child_term":true}}"#).unwrap();
    let child = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--prompt-file"])
        .arg(prompt)
        .args([
            "--timeout-seconds",
            "10",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let tree = wait_for_tree(&fixture);
    for _ in 0..2 {
        success(
            &Command::new("/bin/kill")
                .args(["-INT", &child.id().to_string()])
                .output()
                .unwrap(),
        );
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["state"], "CANCELLED");
    assert_eq!(report["error_kind"], "cancelled");
    assert_eq!(report["exit_signal"], 15);
    assert!(report["exit_code"].is_null());
    assert_eq!(
        verified_manifest(&fixture, &report)["stop_reason"],
        "cancelled"
    );
    assert_tree_stopped(&tree);
    let run = fabric_store::Store::open(fixture.0.join("storage/fabric.db"))
        .unwrap()
        .run(&report["run_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, fabric_domain::RunState::Cancelled);
    assert_eq!(run.error_kind.as_deref(), Some("cancelled"));
    assert_eq!(run.exit_signal, Some(15));
}

#[test]
fn api_cancellation_is_idempotent_and_escalates_term_ignoring_child() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("api-cancel.json");
    fs::write(&prompt, r#"{"process_tree":{"ignore_child_term":true}}"#).unwrap();
    let options = fabric_core::run::RunOptions {
        copy_limit_bytes: 10485760,
        limits: fabric_runner::ExecutionLimits {
            timeout: std::time::Duration::from_secs(10),
            term_grace: std::time::Duration::from_millis(600),
        },
        ..fabric_core::run::RunOptions::default()
    };
    let harness =
        fabric_runner::FakeHarness::new(PathBuf::from(env!("CARGO_BIN_EXE_fabric-fake-harness")))
            .unwrap();
    let report = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            fabric_core::run::foreground(
                &fixture.0.join("storage"),
                "test",
                &prompt,
                &harness,
                &options,
            )
        });
        let tree = wait_for_tree(&fixture);
        options.cancellation.cancel();
        options.cancellation.cancel();
        let before = fs::metadata(tree.join("child.heartbeat")).unwrap().len();
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            fs::metadata(tree.join("child.heartbeat")).unwrap().len() > before,
            "fixture did not survive TERM to exercise escalation"
        );
        worker.join().unwrap().unwrap()
    });
    options.cancellation.cancel(); // Completed tokens never target a stored/reused PID.
    assert_eq!(report.state, fabric_domain::RunState::Cancelled);
    assert_eq!(report.error_kind.as_deref(), Some("cancelled"));
    assert_eq!(report.exit_signal, Some(15));
    assert_eq!(
        verified_manifest(&fixture, &serde_json::to_value(&report).unwrap())["stop_reason"],
        "cancelled"
    );
    assert_tree_stopped(Path::new(&report.workspace_path));
}

#[test]
fn natural_leader_exit_cleans_background_descendant_and_does_not_claim_timeout() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("background.json");
    fs::write(
        &prompt,
        r#"{"process_tree":{"ignore_child_term":true,"leader_exits":true}}"#,
    )
    .unwrap();
    let out = fixture
        .cli()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .args(["run", "test", "--harness", "fake", "--prompt-file"])
        .arg(prompt)
        .args([
            "--timeout-seconds",
            "2",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .spawn()
        .unwrap();
    let tree = wait_for_tree(&fixture);
    // Bound actual child cleanup, not the subsequent durable fsync/publication work.
    let start = std::time::Instant::now();
    loop {
        let before = fs::read(tree.join("child.heartbeat")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        if fs::read(tree.join("child.heartbeat")).unwrap() == before {
            break;
        }
    }
    let out = out.wait_with_output().unwrap();
    let report = json(&out);
    assert_eq!(report["state"], "SUCCEEDED");
    assert_eq!(report["exit_code"], 0);
    assert!(report["exit_signal"].is_null());
    assert!(report["error_kind"].is_null());
    assert!(verified_manifest(&fixture, &report)["stop_reason"].is_null());
    assert_tree_stopped(Path::new(report["workspace_path"].as_str().unwrap()));
}

#[test]
fn batch_ctrl_c_stops_queued_launches_and_keeps_completed_durable_outcomes() {
    let fixture = Fixture::new();
    fixture.setup();
    fs::write(fixture.0.join("done.json"), "{}").unwrap();
    fs::write(
        fixture.0.join("live.json"),
        r#"{"process_tree":{"ignore_child_term":true}}"#,
    )
    .unwrap();
    let manifest = fixture.0.join("cancel-batch.json");
    fs::write(&manifest, r#"[{"id":"done","prompt_file":"done.json"},{"id":"live","prompt_file":"live.json"},{"id":"queued","prompt_file":"live.json"},{"id":"also-queued","prompt_file":"live.json"}]"#).unwrap();
    let child = batch(&fixture, &manifest, "1")
        .args(["--timeout-seconds", "10"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let tree = wait_for_tree(&fixture);
    success(
        &Command::new("/bin/kill")
            .args(["-INT", &child.id().to_string()])
            .output()
            .unwrap(),
    );
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["tasks"][0]["result"]["state"], "SUCCEEDED");
    assert_eq!(report["tasks"][1]["result"]["state"], "CANCELLED");
    assert_eq!(report["succeeded"], false);
    assert_tree_stopped(&tree);
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let id = report["batch_id"].as_str().unwrap().parse().unwrap();
    let tasks = store.batch_tasks(&id).unwrap();
    assert_eq!(store.batch_succeeded(&id).unwrap(), Some(false));
    for (i, saved) in tasks.iter().enumerate() {
        if i < 2 {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(saved.result.as_ref().unwrap()).unwrap(),
                report["tasks"][i]["result"]
            );
        } else {
            assert!(
                saved
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("Cancelled before launch")
            );
            assert_eq!(report["tasks"][i]["error"].as_str(), saved.error.as_deref());
            assert!(
                store.run(&saved.run_id).unwrap().is_none(),
                "queued task was launched"
            );
        }
    }
}

#[test]
fn batch_api_applies_wall_timeout_per_worker_without_discarding_other_results() {
    let fixture = Fixture::new();
    fixture.setup();
    fs::write(
        fixture.0.join("slow.json"),
        r#"{"process_tree":{"ignore_child_term":true}}"#,
    )
    .unwrap();
    fs::write(fixture.0.join("done.json"), "{}").unwrap();
    let manifest = fixture.0.join("api-batch.json");
    fs::write(&manifest, r#"[{"id":"timeout","prompt_file":"slow.json"},{"id":"success","prompt_file":"done.json"}]"#).unwrap();
    let options = fabric_core::run::RunOptions {
        copy_limit_bytes: 10485760,
        limits: fabric_runner::ExecutionLimits {
            timeout: std::time::Duration::from_millis(400),
            ..fabric_runner::ExecutionLimits::default()
        },
        ..fabric_core::run::RunOptions::default()
    };
    let harness =
        fabric_runner::FakeHarness::new(PathBuf::from(env!("CARGO_BIN_EXE_fabric-fake-harness")))
            .unwrap();
    let report = fabric_core::run::run_many(
        &fixture.0.join("storage"),
        "test",
        &manifest,
        1,
        &harness,
        &options,
    )
    .unwrap();
    assert!(!report.succeeded);
    let timed = report.tasks[0].result.as_ref().unwrap();
    assert_eq!(timed.state, fabric_domain::RunState::Failed);
    assert_eq!(timed.error_kind.as_deref(), Some("timeout"));
    assert_tree_stopped(Path::new(&timed.workspace_path));
    assert_eq!(
        report.tasks[1].result.as_ref().unwrap().state,
        fabric_domain::RunState::Succeeded
    );
}

#[test]
fn cooperative_zero_exit_after_timeout_still_persists_timeout_failure() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    let executable = fixture.0.join("cooperative-fake");
    fs::write(&executable, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'fabric-fake-harness 1'; exit 0; fi\ntrap 'exit 0' TERM\nwhile :; do /bin/sleep 1; done\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let prompt = fixture.0.join("cooperative.json");
    fs::write(&prompt, "{}").unwrap();
    let out = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(executable)
        .arg("--prompt-file")
        .arg(prompt)
        .args([
            "--timeout-seconds",
            "1",
            "--json",
            "--copy-limit-bytes",
            "10485760",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["state"], "FAILED");
    assert_eq!(report["error_kind"], "timeout");
    assert_eq!(report["exit_code"], 0);
    assert!(report["exit_signal"].is_null());
    let run = fabric_store::Store::open(fixture.0.join("storage/fabric.db"))
        .unwrap()
        .run(&report["run_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, fabric_domain::RunState::Failed);
    assert_eq!(run.error_kind.as_deref(), Some("timeout"));
    assert_eq!(run.exit_code, Some(0));
}

#[test]
fn foreground_fake_process_records_provenance_exit_and_retains_changes() {
    use fabric_domain::{RunId, RunState, WorkspaceState};
    use fabric_store::Store;
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    for exit in [0, 7] {
        let out = fixture.run(exit);
        assert_eq!(out.status.success(), exit == 0, "{out:?}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let store = Store::open(fixture.0.join("storage/fabric.db")).unwrap();
        let run = store
            .run(&RunId::try_from(report["run_id"].as_str().unwrap().to_owned()).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            run.state,
            if exit == 0 {
                RunState::Succeeded
            } else {
                RunState::Failed
            }
        );
        assert_eq!(
            run.harness_version.as_deref(),
            Some("fabric-fake-harness 1")
        );
        assert_eq!(run.exit_code, Some(exit));
        assert_eq!(run.base_commit_sha, base["source_commit_sha"]);
        assert!(run.pid_metadata.is_some());
        assert!(run.created_at <= run.started_at.unwrap());
        assert!(run.started_at.unwrap() <= run.finished_at.unwrap());
        assert!(run.finished_at.unwrap() <= run.updated_at);
        assert_eq!(run.prompt_digest, report["prompt_digest"]);
        assert_eq!(run.prompt_digest.len(), 64);
        assert!(run.artifact_manifest_id.is_some());
        verified_manifest(&fixture, &report);
        let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
        assert_eq!(workspace.state, WorkspaceState::Retained);
        assert!(
            workspace
                .retention_reason
                .as_deref()
                .unwrap()
                .contains("capture")
        );
        let tree = Path::new(&workspace.managed_path);
        assert_eq!(
            fs::read_to_string(tree.join("tracked")).unwrap(),
            "worker tracked"
        );
        assert_eq!(
            fs::read_to_string(tree.join("new source ; literal")).unwrap(),
            "worker untracked"
        );
        assert!(!tree.join("INJECTED").exists());
        let container = tree.parent().unwrap();
        assert_eq!(
            fs::read(container.join("prompt.input")).unwrap(),
            fs::read(fixture.0.join("prompt ; $(touch INJECTED).json")).unwrap()
        );
        use sha2::{Digest, Sha256};
        assert_eq!(
            run.prompt_digest,
            format!(
                "{:x}",
                Sha256::digest(fs::read(container.join("prompt.input")).unwrap())
            )
        );
        let stdout = fs::read_to_string(container.join("stdout.log")).unwrap();
        assert!(stdout.len() > 64 * 1024);
        let events: Vec<serde_json::Value> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events[0]["type"], "started");
        assert_eq!(events[1]["value"], 42);
        assert_eq!(events.last().unwrap()["exit_code"], exit);
        assert!(fs::read(container.join("stderr.log")).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(tree.join(".deps/ready")).unwrap(),
            "ready"
        );
        let patch = fixture
            .0
            .join("storage/artifacts")
            .join(run.artifact_manifest_id.as_ref().unwrap().as_str())
            .join("tracked.patch");
        assert!(
            fs::read_to_string(&patch)
                .unwrap()
                .contains("worker tracked")
        );
        assert!(
            !fs::read_to_string(&patch)
                .unwrap()
                .contains("worker untracked")
        );
        // Promoted copies survive loss/corruption of the original output evidence.
        fs::write(container.join("stdout.log"), "changed after capture").unwrap();
        verified_manifest(&fixture, &report);
    }
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(Path::new(base["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        "original"
    );
}

#[test]
fn run_selects_prepared_current_head_and_refuses_unprepared_head() {
    let fixture = Fixture::new();
    let (repo, original) = fixture.setup();
    fs::write(repo.join("tracked"), "second commit").unwrap();
    fixture.git(&repo, &["commit", "--quiet", "-am", "second"]);
    let out = fixture.run(0);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("prepare"));
    let current = fixture.prepare();
    assert_ne!(original["state_id"], current["state_id"]);
    fs::write(repo.join("tracked"), "dirty not imported").unwrap();
    let report = json(&fixture.run(0));
    assert_eq!(report["prepared_state_id"], current["state_id"]);
    assert_eq!(report["base_commit_sha"], current["source_commit_sha"]);
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "dirty not imported"
    );
}

#[test]
fn omp_mock_receives_literal_prompt_parses_bounded_jsonl_and_records_observed_version() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    let bin = fixture.0.join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("omp");
    fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
test -n "$HOME"
test -z "${UNSELECTED_SECRET+x}"
test -z "${OPENAI_API_KEY+x}"
test -z "${AWS_SECRET_ACCESS_KEY+x}"
test -z "${BASH_ENV+x}"
test -z "${PI_CODING_AGENT_DIR+x}"
if [ "$#" = 1 ] && [ "$1" = --version ]; then
    printf 'omp/19.1.0-fixture+exact\n'
    exit 0
fi
test "$#" = 8
test "$1" = -p
test "$2" = --mode
test "$3" = json
test "$4" = --no-session
test "$5" = --no-title
test "$6" = --max-cost=0.25
test "$7" = --
test "$(cat .deps/ready)" = ready
printf '%s' "$8" > received.prompt
printf 'mock tracked change' > tracked
printf '{"type":"started","value":42}\r\nnot-json\n\377\n'
head -c 262144 /dev/zero | tr '\000' x
printf '\n{"type":"finished"}\n{"last":true}'
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    for prompt in [
        "--model evil ; $(touch INJECTED) 'quoted' \"double\" \\\nsecond line\n",
        "@literal-file-not-an-option",
        "",
    ] {
        let prompt_path = fixture.0.join("omp.prompt");
        fs::write(&prompt_path, prompt).unwrap();
        let out = fixture
            .cli()
            .env("PATH", &path)
            .env("OPENAI_API_KEY", "must-not-leak")
            .env("AWS_SECRET_ACCESS_KEY", "must-not-leak")
            .env("BASH_ENV", "/must/not/load")
            .env("PI_CODING_AGENT_DIR", "/must/not/load")
            .args(["run", "test", "--harness", "omp", "--prompt-file"])
            .arg(prompt_path)
            .args([
                "--json",
                "--copy-limit-bytes",
                "10485760",
                "--max-cost-usd",
                "0.25",
            ])
            .output()
            .unwrap();
        let report = json(&out);
        assert_eq!(report["harness"], "omp");
        assert_eq!(report["harness_version"], "omp/19.1.0-fixture+exact");
        assert_eq!(report["base_commit_sha"], base["source_commit_sha"]);
        assert_eq!(report["state"], "SUCCEEDED");
        assert_eq!(report["events"]["parsed"], 3);
        assert_eq!(report["events"]["invalid"], 2);
        assert_eq!(report["events"]["oversized"], 1);
        let tree = Path::new(report["workspace_path"].as_str().unwrap());
        let container = tree.parent().unwrap();
        let records: Vec<serde_json::Value> = fs::read_to_string(container.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 6);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record["line_number"], index + 1);
        }
        assert_eq!(records[0]["status"], "parsed");
        assert_eq!(records[0]["value"]["value"], 42);
        assert_eq!(records[1]["status"], "invalid");
        assert_eq!(records[1]["line_bytes"], 8);
        assert_eq!(records[2]["status"], "invalid");
        assert_eq!(records[2]["line_bytes"], 1);
        assert_eq!(records[3]["status"], "oversized");
        assert_eq!(records[3]["line_bytes"], 262144);
        assert!(records[3].get("value").is_none());
        assert_eq!(records[4]["value"]["type"], "finished");
        assert_eq!(records[5]["value"]["last"], true);
        assert_eq!(report["capture"]["truncated"], false);
        assert_eq!(
            fs::read(tree.join("received.prompt")).unwrap(),
            prompt.as_bytes()
        );
        assert!(!tree.join("INJECTED").exists());
        assert_eq!(
            fs::read_to_string(tree.join("tracked")).unwrap(),
            "mock tracked change"
        );
        let run = fabric_store::Store::open(fixture.0.join("storage/fabric.db"))
            .unwrap()
            .run(&report["run_id"].as_str().unwrap().parse().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(run.harness_version, Some("omp/19.1.0-fixture+exact".into()));
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(
            serde_json::to_value(run.capture.unwrap()).unwrap(),
            report["capture"]
        );
        assert_eq!(
            fs::read_to_string(repo.join("tracked")).unwrap(),
            "original"
        );
    }
}

#[test]
fn missing_omp_is_actionable_and_does_not_allocate_a_run() {
    let fixture = Fixture::new();
    fixture.setup();
    let prompt = fixture.0.join("omp.prompt");
    fs::write(&prompt, "no model call").unwrap();
    let bin = fixture.0.join("empty-bin");
    fs::create_dir(&bin).unwrap();
    let out = fixture
        .cli()
        .env("PATH", bin)
        .args(["run", "test", "--harness", "omp", "--prompt-file"])
        .arg(prompt)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let diagnostic: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert!(
        diagnostic["error"]["message"]
            .as_str()
            .unwrap()
            .contains("OMP executable not found on PATH")
    );
    assert!(!fixture.0.join("storage/fork-staging").exists());
}

#[test]
fn hundred_mib_concurrent_fake_output_has_one_durable_capture_budget() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    let binary = fixture.0.join("noisy-fake");
    fs::write(
        &binary,
        r#"#!/bin/sh
set -eu
if [ "$1" = --version ]; then printf 'fabric-fake-harness 1\n'; exit 0; fi
printf '{"type":"before","value":42}\nnot-json\n'
head -c 52428800 /dev/zero >&2 &
head -c 52428800 /dev/zero
printf '\n{"type":"after"}\n'
wait
"#,
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let prompt = fixture.0.join("noise.prompt");
    fs::write(&prompt, "{}").unwrap();
    let report = json(
        &fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(binary)
            .arg("--prompt-file")
            .arg(prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap(),
    );
    let container = Path::new(report["workspace_path"].as_str().unwrap())
        .parent()
        .unwrap();
    let total: u64 = ["stdout.log", "stderr.log", "events.jsonl"]
        .iter()
        .filter_map(|name| fs::metadata(container.join(name)).ok())
        .map(|metadata| metadata.len())
        .sum();
    assert!(total <= 10 * 1024 * 1024, "persisted {total} capture bytes");
    let capture = &report["capture"];
    assert_eq!(capture["limit_bytes"], 10 * 1024 * 1024);
    assert_eq!(capture["persisted_bytes"], total);
    assert_eq!(capture["truncated"], true);
    assert_eq!(
        verified_manifest(&fixture, &report)["capture"]["truncated"],
        true
    );
    assert!(capture["stdout_bytes_seen"].as_u64().unwrap() >= 50 * 1024 * 1024);
    assert_eq!(capture["stderr_bytes_seen"], 50 * 1024 * 1024);
    assert_eq!(report["events"]["parsed"], 2);
    assert_eq!(report["events"]["invalid"], 1);
    assert_eq!(report["events"]["oversized"], 1);
    assert!(
        fs::read(container.join("stdout.log"))
            .unwrap()
            .ends_with(b"\n[FABRIC CAPTURE TRUNCATED]\n")
    );
    for (field, name) in [
        ("stdout", "stdout.log"),
        ("stderr", "stderr.log"),
        ("events", "events.jsonl"),
    ] {
        let bytes = fs::read(container.join(name)).unwrap();
        assert_eq!(capture[field]["size_bytes"], bytes.len());
        assert_eq!(
            capture[field]["sha256"],
            format!("{:x}", Sha256::digest(&bytes))
        );
    }
    // Open a separate connection after the foreground command has exited.
    let reopened = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let run = reopened
        .run(&report["run_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, fabric_domain::RunState::Succeeded);
    assert_eq!(
        serde_json::to_value(run.capture.unwrap()).unwrap(),
        *capture
    );
}

fn batch(fixture: &Fixture, manifest: &Path, concurrency: &str) -> Command {
    let mut command = fixture.cli();
    command
        .args(["run-many", "test", "--harness", "fake", "--tasks"])
        .arg(manifest)
        .args([
            "--concurrency",
            concurrency,
            "--copy-limit-bytes",
            "10485760",
            "--json",
        ]);
    command
}

#[test]
fn run_many_caps_actual_external_workers_and_durably_associates_results() {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    let bin = fixture.0.join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("omp");
    let journal = fixture.0.join("activity");
    fs::write(
        &executable,
        format!(
            r#"#!/bin/sh
set -eu
lock='{}/lock'
journal='{}'
if [ "$1" = --version ]; then
  printf 'probe\n' >> "$journal.probes"; printf 'omp/fixture\n'; exit 0
fi
test "$(cat .deps/ready)" = ready
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
printf 'start %s\n' "$$" >> "$journal"
rmdir "$lock"
i=0
while [ "$(grep -c '^start ' "$journal")" -lt 2 ]; do
  i=$((i+1)); test "$i" -lt 500; sleep 0.01
done
sleep 0.1
printf '%s' "$7" > tracked
printf '{{"result":"%s"}}\n' "$7"
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
printf 'end %s\n' "$$" >> "$journal"
rmdir "$lock"
"#,
            fixture.0.display(),
            journal.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let prompts = fixture.0.join("manifests");
    fs::create_dir(&prompts).unwrap();
    let tasks: Vec<_> = (0..4)
        .map(|i| {
            fs::write(prompts.join(format!("{i}.txt")), format!("task-{i}")).unwrap();
            serde_json::json!({"id":format!("task-{i}"), "prompt_file":format!("{i}.txt")})
        })
        .collect();
    let manifest = prompts.join("tasks.json");
    fs::write(&manifest, serde_json::to_vec(&tasks).unwrap()).unwrap();
    let out = fixture
        .cli()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["run-many", "test", "--harness", "omp", "--tasks"])
        .arg(&manifest)
        .args([
            "--concurrency",
            "2",
            "--copy-limit-bytes",
            "10485760",
            "--json",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        fs::read_to_string(journal.with_extension("probes")).unwrap(),
        "probe\n"
    );
    let report = json(&out);
    assert_eq!(report["succeeded"], true);
    let mut active = HashSet::new();
    let mut peak = 0;
    let mut starts = 0;
    for line in fs::read_to_string(journal).unwrap().lines() {
        let (event, pid) = line.split_once(' ').unwrap();
        if event == "start" {
            assert!(active.insert(pid.to_owned()));
            starts += 1;
            peak = peak.max(active.len());
        } else {
            assert!(active.remove(pid));
        }
        assert!(active.len() <= 2);
    }
    assert_eq!(starts, 4);
    assert_eq!(peak, 2);
    assert!(active.is_empty());
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let batch_id =
        fabric_domain::BatchId::try_from(report["batch_id"].as_str().unwrap().to_owned()).unwrap();
    let durable = store.batch_tasks(&batch_id).unwrap();
    assert_eq!(store.batch_succeeded(&batch_id).unwrap(), Some(true));
    assert_eq!(durable.len(), 4);
    let mut runs = HashSet::new();
    let mut workspaces = HashSet::new();
    for (i, task) in report["tasks"].as_array().unwrap().iter().enumerate() {
        assert_eq!(task["id"], format!("task-{i}"));
        let result = &task["result"];
        verified_manifest(&fixture, result);
        let run_id =
            fabric_domain::RunId::try_from(task["run_id"].as_str().unwrap().to_owned()).unwrap();
        assert!(runs.insert(run_id.clone()));
        assert_eq!(result["run_id"], task["run_id"]);
        let run = store.run(&run_id).unwrap().unwrap();
        assert_eq!(run.batch_id.as_ref(), Some(&batch_id));
        assert_eq!(run.prepared_state_id.as_str(), base["state_id"]);
        assert_eq!(run.state, fabric_domain::RunState::Succeeded);
        assert!(workspaces.insert(run.workspace_id.clone()));
        let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(Path::new(&workspace.managed_path).join("tracked")).unwrap(),
            format!("task-{i}")
        );
        assert_eq!(
            fs::read_to_string(Path::new(&workspace.managed_path).join(".deps/ready")).unwrap(),
            "ready"
        );
        assert_eq!(durable[i].task_id, task["id"]);
        assert_eq!(durable[i].run_id, run_id);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(durable[i].result.as_ref().unwrap()).unwrap(),
            *result
        );
        assert!(durable[i].error.is_none());
    }
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(Path::new(base["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        "original"
    );
    assert_eq!(fixture.prepare()["state_id"], base["state_id"]);
}

#[test]
fn run_many_preserves_mixed_exit_and_prelaunch_failure_and_rejects_invalid_input() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.setup();
    let manifest = fixture.0.join("tasks.json");
    for exit in [0, 7] {
        fs::write(
            fixture.0.join(format!("{exit}.json")),
            serde_json::to_vec(&serde_json::json!({
                "required_files":[".deps/ready"], "exit_code":exit,
                "writes":[{"path":"tracked","content":format!("exit-{exit}")}]
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let probe_marker = fixture.0.join("unexpected-probe");
    let probe = fixture.0.join("probe");
    fs::write(
        &probe,
        format!("#!/bin/sh\ntouch '{}'\nexit 99\n", probe_marker.display()),
    )
    .unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o700)).unwrap();
    for (input, concurrency) in [
        ("[]", "2"),
        ("not json", "2"),
        (
            r#"[{"id":"a","prompt_file":"0.json"},{"id":"a","prompt_file":"7.json"}]"#,
            "2",
        ),
        (r#"[{"id":"","prompt_file":"0.json"}]"#, "2"),
        (r#"[{"id":" ","prompt_file":"0.json"}]"#, "2"),
        (r#"[{"id":"a","prompt_file":""}]"#, "2"),
        (r#"[{"prompt_file":"0.json"}]"#, "2"),
        (r#"[{"id":"a","prompt_file":7}]"#, "2"),
        (
            r#"[{"id":"a","prompt_file":"0.json","unexpected":true}]"#,
            "2",
        ),
        (r#"[{"id":"a","prompt_file":"0.json"}]"#, "0"),
        (r#"[{"id":"a","prompt_file":"0.json"}]"#, "-1"),
    ] {
        fs::write(&manifest, input).unwrap();
        let out = batch(&fixture, &manifest, concurrency)
            .arg("--fake-binary")
            .arg(&probe)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{out:?}");
        assert!(!fixture.0.join("storage/fork-staging").exists());
        assert!(
            !probe_marker.exists(),
            "invalid input launched an external probe"
        );
    }
    fs::write(&manifest, r#"[{"id":"ok","prompt_file":"0.json"},{"id":"nonzero","prompt_file":"7.json"},{"id":"missing","prompt_file":"missing.json"}]"#).unwrap();
    let out = batch(&fixture, &manifest, "2").output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["succeeded"], false);
    let tasks = report["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0]["result"]["exit_code"], 0);
    assert_eq!(tasks[0]["result"]["state"], "SUCCEEDED");
    assert_eq!(tasks[1]["result"]["exit_code"], 7);
    assert_eq!(tasks[1]["result"]["state"], "FAILED");
    assert!(tasks[2]["result"].is_null());
    assert!(tasks[2]["error"].as_str().unwrap().contains("missing.json"));
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let batch_id =
        fabric_domain::BatchId::try_from(report["batch_id"].as_str().unwrap().to_owned()).unwrap();
    let durable = store.batch_tasks(&batch_id).unwrap();
    assert_eq!(store.batch_succeeded(&batch_id).unwrap(), Some(false));
    assert_eq!(durable.len(), 3);
    assert!(store.run(&durable[2].run_id).unwrap().is_none());
    for (task, saved) in tasks.iter().zip(durable) {
        assert_eq!(saved.run_id.as_str(), task["run_id"]);
        assert_eq!(saved.task_id, task["id"]);
        if task["error"].is_null() {
            verified_manifest(&fixture, &task["result"]);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&saved.result.unwrap()).unwrap(),
                task["result"]
            );
        } else {
            assert_eq!(saved.error.as_deref(), task["error"].as_str());
        }
    }
}

#[test]
fn tracked_binary_patch_round_trips_staged_and_unstaged_changes() {
    let fixture = Fixture::new();
    let (repo, _) = fixture.setup();
    fs::write(repo.join("binary"), b"\0original binary\xff").unwrap();
    fixture.git(&repo, &["add", "binary"]);
    fixture.git(&repo, &["commit", "--quiet", "-m", "binary baseline"]);
    fixture.prepare();
    let executable = fixture.script_fake(
        r#"
printf '\000staged\377' > binary
/usr/bin/git add binary
printf '\000unstaged final\376' > binary
printf '\000staged addition\375' > staged.bin
/usr/bin/git add staged.bin
printf 'unstaged text' > tracked
printf 'not captured' > untracked.txt
printf '{"captured":true}\n'
"#,
    );
    let prompt = fixture.0.join("binary-prompt");
    fs::write(&prompt, "exact prompt\n").unwrap();
    let report = json(
        &fixture
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(executable)
            .arg("--prompt-file")
            .arg(prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"])
            .output()
            .unwrap(),
    );
    verified_manifest(&fixture, &report);
    let patch = fixture
        .0
        .join("storage/artifacts")
        .join(report["artifact_manifest_id"].as_str().unwrap())
        .join("tracked.patch");
    let bytes = fs::read(&patch).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("GIT binary patch"));
    assert!(!String::from_utf8_lossy(&bytes).contains("untracked.txt"));
    let restored = fixture.0.join("restored");
    fixture.git(
        &repo,
        &[
            "clone",
            "--quiet",
            "--no-hardlinks",
            repo.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    fixture.git(&restored, &["apply", "--binary", patch.to_str().unwrap()]);
    assert_eq!(
        fs::read(restored.join("binary")).unwrap(),
        b"\0unstaged final\xfe"
    );
    assert_eq!(
        fs::read(restored.join("staged.bin")).unwrap(),
        b"\0staged addition\xfd"
    );
    assert_eq!(
        fs::read(restored.join("tracked")).unwrap(),
        b"unstaged text"
    );
    assert_eq!(
        fs::read(repo.join("binary")).unwrap(),
        b"\0original binary\xff"
    );
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    assert_eq!(
        fs::read(tree.join("untracked.txt")).unwrap(),
        b"not captured"
    );
    let root = fixture.0.join("storage");
    let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
    assert!(
        db.execute(
            "UPDATE runs SET artifact_manifest_id='replacement' WHERE run_id=?1",
            [report["run_id"].as_str().unwrap()]
        )
        .is_err()
    );
    assert!(
        db.execute("UPDATE artifacts SET sha256=?1", ["0".repeat(64)])
            .is_err()
    );
}

#[test]
fn capture_failures_durably_retain_workspace_and_never_claim_terminal_success() {
    use fabric_domain::{RunState, WorkspaceState};
    for failure in ["promotion", "registration", "limit", "tampered_output"] {
        let fixture = Fixture::new();
        fixture.setup();
        let root = fixture.0.join("storage");
        let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
        if failure == "registration" {
            db.execute_batch(
                "CREATE TRIGGER injected_artifact_failure BEFORE INSERT ON artifacts
                BEGIN SELECT RAISE(ABORT, 'injected registration failure'); END;",
            )
            .unwrap();
        }
        let extra = match failure {
            "promotion" => {
                r#"
run=$(/usr/bin/sed -n '3p' ../.fabric-owner)
/bin/mkdir -p "../../../artifacts/a-$run"
printf 'existing sentinel' > "../../../artifacts/a-$run/sentinel"
"#
            }
            "tampered_output" => r#"/bin/rm ../stdout.log; /bin/ln -s /etc/passwd ../stdout.log"#,
            _ => "",
        };
        let executable = fixture.script_fake(&format!(
            "printf 'retained edit' > tracked\nprintf 'uncaptured' > untracked.txt\nprintf '{{\"event\":true}}\\n'\n{extra}"
        ));
        let prompt = fixture.0.join("prompt");
        fs::write(&prompt, "retained exact prompt").unwrap();
        let mut command = fixture.cli();
        command
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(executable)
            .arg("--prompt-file")
            .arg(&prompt)
            .args(["--json", "--copy-limit-bytes", "10485760"]);
        if failure == "limit" {
            command.args(["--max-artifact-bytes", "32"]);
        }
        let out = command.output().unwrap();
        assert!(!out.status.success(), "{failure}: {out:?}");
        let id: String = db
            .query_row("SELECT run_id FROM runs", [], |row| row.get(0))
            .unwrap();
        drop(db);
        let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
        let id = id.parse().unwrap();
        let run = store.run(&id).unwrap().unwrap();
        assert_eq!(run.state, RunState::NeedsAttention, "{failure}: {out:?}");
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.exit_signal, None);
        assert_eq!(run.error_kind.as_deref(), Some("artifact_capture"));
        assert!(run.finished_at.is_some());
        assert!(run.artifact_manifest_id.is_none());
        assert!(run.capture.is_some());
        assert!(store.artifacts(&id).unwrap().is_empty());
        let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
        assert_eq!(workspace.state, WorkspaceState::RecoveryRequired);
        let tree = Path::new(&workspace.managed_path);
        assert_eq!(fs::read(tree.join("tracked")).unwrap(), b"retained edit");
        assert_eq!(fs::read(tree.join("untracked.txt")).unwrap(), b"uncaptured");
        assert_eq!(
            fs::read(tree.parent().unwrap().join("prompt.input")).unwrap(),
            fs::read(&prompt).unwrap()
        );
        assert!(tree.parent().unwrap().join("stderr.log").is_file());
        if failure == "registration" {
            let manifest = root.join(format!("artifacts/a-{id}/manifest.json"));
            assert!(
                manifest.is_file(),
                "promoted recovery artifact must survive DB rollback"
            );
        } else {
            assert!(root.join(format!("artifact-staging/a-{id}")).is_dir());
        }
        if failure == "promotion" {
            assert_eq!(
                fs::read(root.join(format!("artifacts/a-{id}/sentinel"))).unwrap(),
                b"existing sentinel"
            );
        }
    }
}
