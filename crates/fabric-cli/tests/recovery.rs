#![cfg(unix)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
#[path = "../../fabric-runner/tests/support/local_target.rs"]
mod local_target;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Process {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Process {
    fn output(&mut self, timeout: Duration) -> Option<Output> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().ok()? {
                return Some(Output {
                    status,
                    stdout: fs::read(&self.stdout).ok()?,
                    stderr: fs::read(&self.stderr).ok()?,
                });
            }
            if Instant::now() >= deadline {
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

struct Fixture {
    path: PathBuf,
    repo: PathBuf,
    prepared: Value,
    manager: Option<Process>,
    batches: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fabric-recovery-{}-{}-{}",
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
        local_target::install(&path, Path::new(env!("CARGO_BIN_EXE_fabric-local-target")));
        let mut fixture = Self {
            repo: path.join("repo ; literal"),
            path,
            prepared: Value::Null,
            manager: None,
            batches: Vec::new(),
        };
        fs::create_dir(&fixture.repo).unwrap();
        fixture.git(&fixture.repo, &["init", "--quiet"]);
        fs::write(fixture.repo.join("tracked"), b"original\n").unwrap();
        fs::write(fixture.repo.join(".gitignore"), b".deps/\n").unwrap();
        fixture.git(&fixture.repo, &["add", "."]);
        fixture.git(&fixture.repo, &["commit", "--quiet", "-m", "initial"]);
        checked(
            &fixture
                .cli()
                .args(["project", "add", "--name", "test", "--path"])
                .arg(&fixture.repo)
                .output()
                .unwrap(),
        );
        let recipe = fixture.path.join("recipe.toml");
        fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n").unwrap();
        fixture.prepared = parsed(
            &fixture
                .cli()
                .args(["project", "prepare", "test", "--json", "--recipe"])
                .arg(recipe)
                .output()
                .unwrap(),
        );
        fixture
    }

    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fabric"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.path)
            .env("FABRIC_PLUGIN_REGISTRY", self.path.join("plugin-registry"))
            .arg("--root")
            .arg(self.path.join("storage"));
        command
    }

    fn query(&self, args: &[&str]) -> Value {
        parsed(&self.cli().args(args).arg("--json").output().unwrap())
    }

    fn git(&self, repo: &Path, args: &[&str]) {
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
                .arg(repo)
                .args(args)
                .output()
                .unwrap(),
        );
    }

    fn capture_failure(&self) -> Value {
        let staging = self.path.join("storage/artifact-staging");
        fs::create_dir(&staging).unwrap();
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
        let wrapper = self.path.join("capture-interrupted-fake");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nset -eu\nif [ \"$1\" != --version ]; then chmod 500 '{}'; fi\nexec '{}' \"$@\"\n",
                staging.display(),
                env!("CARGO_BIN_EXE_fabric-fake-harness"),
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        let prompt = self.path.join("prompt.json");
        fs::write(
            &prompt,
            serde_json::to_vec(&json!({
                "required_files":[".deps/ready"],
                "writes":[{"path":"tracked","content":"recovered tracked\n"},
                          {"path":"new source ; literal","content":"recovered untracked\n"}],
                "events":[{"type":"fixture","value":42}],
                "exit_code":7
            }))
            .unwrap(),
        )
        .unwrap();
        // A real supervised fake process exits 7. A deterministic permission fault
        // interrupts artifact capture before staging exists; this is NOT a SIGKILL.
        let output = self
            .cli()
            .args(["run", "test", "--harness", "fake", "--fake-binary"])
            .arg(wrapper)
            .arg("--prompt-file")
            .arg(prompt)
            .args([
                "--timeout-seconds",
                "10",
                "--copy-limit-bytes",
                "10485760",
                "--json",
            ])
            .output()
            .unwrap();
        // Restore only the injected I/O permission, never ownership or durable evidence.
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!output.status.success(), "{output:?}");
        let runs = self.query(&["runs", "list"]);
        let runs = runs.as_array().unwrap();
        assert_eq!(runs.len(), 1);
        let id = runs[0]["run_id"].as_str().unwrap();
        let shown = self.query(&["runs", "show", id]);
        assert_eq!(shown["run"]["state"], "NEEDS_ATTENTION");
        assert_eq!(shown["run"]["exit_code"], 7);
        assert_eq!(shown["run"]["error_kind"], "artifact_capture");
        assert!(shown["run"]["capture"].is_object());
        assert!(shown["run"]["artifact_manifest_id"].is_null());
        assert_eq!(shown["workspace"]["state"], "RECOVERY_REQUIRED");
        assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
        shown
    }

    fn spawn(&self, mut command: Command) -> Process {
        let next = NEXT.fetch_add(1, Ordering::Relaxed);
        let stdout = self.path.join(format!("command-{next}.stdout"));
        let stderr = self.path.join(format!("command-{next}.stderr"));
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

    fn start_manager(&mut self) {
        let mut command = self.cli();
        command.args(["manager", "serve", "--json"]);
        let mut process = self.spawn(command);
        eventually(|| {
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "manager exited"
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

    fn submit(&mut self, labels: &[usize], fault: &str, exit: i32) -> Value {
        let tasks: Vec<_> = labels
            .iter()
            .map(|label| {
                json!({
                    "id":format!("task-{label}"),
                    "prompt":json!({
                        "required_files":[".deps/ready"],
                        "writes":[{"path":"tracked","content":format!("task-{label}")},
                                  {"path":"new-source","content":format!("new-{label}")}],
                        "events":[{"type":"fixture","task":label}],
                        "exit_code":exit
                    }).to_string()
                })
            })
            .collect();
        let request = self.path.join(format!("request-{}.json", labels[0]));
        fs::write(
            &request,
            serde_json::to_vec(&json!({
                "version":1,"op":"spawn_many","project":"test","default_harness":"fake",
                "concurrency":4,"tasks":tasks,
                "options":{"copy_limit_bytes":10485760,"timeout_seconds":45,
                    "max_artifact_bytes":if fault == "partial" {1024} else {10485760}}
            }))
            .unwrap(),
        )
        .unwrap();
        let wrapper = self.path.join(format!("fake-{}", labels[0]));
        fs::write(
            &wrapper,
            format!(
                r#"#!/bin/sh
set -eu
if [ "$1" = --version ]; then exec '{fake}' "$@"; fi
test "$1" = --prompt-file
prompt=$(cat "$2")
found=no
for id in {labels}; do
  case "$prompt" in *\"content\":\"task-$id\"*) found=yes; break;; esac
done
test "$found" = yes
root='{root}'
trap 'touch "$root/stopped.$id"' EXIT
trap 'exit 143' TERM INT
set +e
'{fake}' "$@"
code=$?
set -e
printf '%s\n%s\n' "$$" "$PWD" > "$root/started.$id.tmp"
mv "$root/started.$id.tmp" "$root/started.$id"
i=0
while [ ! -f "$root/release.$id" ] && [ ! -f "$root/release-all" ]; do
  i=$((i+1)); test "$i" -lt 2000
  sleep 0.02
done
case '{fault}' in
  capture) chmod 500 "$root/storage/artifact-staging";;
  cleanup) chmod 500 "$root/storage/fork-staging";;
esac
exit "$code"
"#,
                fake = env!("CARGO_BIN_EXE_fabric-fake-harness"),
                root = self.path.display(),
                labels = labels
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        if fault == "capture" {
            let stage = self.path.join("storage/artifact-staging");
            if !stage.exists() {
                fs::create_dir(&stage).unwrap();
            }
            fs::set_permissions(stage, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let accepted = parsed(
            &self
                .cli()
                .args(["submit", "--request"])
                .arg(request)
                .arg("--fake-binary")
                .arg(wrapper)
                .arg("--json")
                .output()
                .unwrap(),
        );
        assert_eq!(accepted["accepted"], true);
        self.batches
            .push(accepted["batch_id"].as_str().unwrap().to_owned());
        accepted
    }

    fn started(&self, label: usize) {
        eventually(|| self.path.join(format!("started.{label}")).exists());
    }

    fn release(&self, label: usize) {
        fs::write(self.path.join(format!("release.{label}")), b"").unwrap();
    }

    fn terminal(&self, batch: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.query(&["wait", batch, "--timeout-seconds", "1"]);
            if status["terminal"] == true {
                return status;
            }
            assert!(Instant::now() < deadline, "batch did not finish: {status}");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::write(self.path.join("release-all"), b"");
        for batch in &self.batches {
            let mut cancel = self.cli();
            cancel.args(["cancel", batch, "--json"]);
            let _ = self.spawn(cancel).output(Duration::from_secs(2));
        }
        if self.manager.is_some() {
            let mut stop = self.cli();
            stop.args(["manager", "stop", "--json"]);
            let _ = self.spawn(stop).output(Duration::from_secs(5));
            self.manager.take();
            // If a supervisor died, gates release its orphan without signalling a
            // PID reconstructed from storage. Never delete a still-owned fixture.
            let deadline = Instant::now() + Duration::from_secs(45);
            while (0..20).any(|label| {
                self.path.join(format!("started.{label}")).exists()
                    && !self.path.join(format!("stopped.{label}")).exists()
            }) {
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn writable(path: &Path) {
            if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
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
    let deadline = Instant::now() + Duration::from_secs(20);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "fixture gate did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn recover_inspects_captureable_failure_then_seals_and_reclaims_without_inventing_success() {
    let fixture = Fixture::new();
    let before = fixture.capture_failure();
    let id = before["run"]["run_id"].as_str().unwrap();
    let workspace_id = before["workspace"]["workspace_id"].as_str().unwrap();
    let tree = Path::new(before["workspace"]["managed_path"].as_str().unwrap());
    let marker = fs::read(tree.parent().unwrap().join(".fabric-owner")).unwrap();
    let sentinel = fixture.path.join("outside-root-sentinel");
    fs::write(&sentinel, b"never delete outside managed root\n").unwrap();

    // An unavailable pinned target cannot convert exit 7 into success or authorize deletion.
    let target = fixture.path.join("plugin-registry/local-target/plugin");
    let held = target.with_extension("unavailable");
    fs::rename(&target, &held).unwrap();
    let unavailable = fixture.query(&["recover", id, "--apply"]);
    assert_eq!(unavailable["action"], "retained");
    assert_eq!(unavailable["reason"], "target_unavailable");
    assert_eq!(fixture.query(&["runs", "show", id]), before);
    assert_eq!(
        fs::read(tree.parent().unwrap().join(".fabric-owner")).unwrap(),
        marker
    );
    fs::rename(held, target).unwrap();
    let inspection = fixture.query(&["recover", id]);
    assert_eq!(inspection["run_id"], id);
    assert_eq!(inspection["workspace_id"], workspace_id);
    assert_eq!(inspection["action"], "would_recover");
    assert_eq!(fixture.query(&["runs", "show", id]), before);
    assert_eq!(
        fs::read(tree.parent().unwrap().join(".fabric-owner")).unwrap(),
        marker
    );
    assert_eq!(
        fs::read(tree.join("tracked")).unwrap(),
        b"recovered tracked\n"
    );
    assert!(
        fixture.query(&["runs", "artifacts", id])["artifacts"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let storage = fixture.query(&["storage", "status"]);
    assert_eq!(
        storage["accounting"],
        "logical_bytes_best_effort_not_exclusive"
    );
    let retained = storage["workspaces"].as_array().unwrap();
    let item = retained.iter().find(|item| item["run_id"] == id).unwrap();
    assert_eq!(item["id"], workspace_id);
    assert!(item["usage_bytes"].as_u64().unwrap() > 0);
    assert!(!item["retention_reason"].as_str().unwrap().is_empty());
    assert!(item["action"].as_str().unwrap().contains(id));

    assert_eq!(
        fixture.query(&["recover", id, "--apply"])["action"],
        "reclaimed"
    );
    let recovered = fixture.query(&["runs", "show", id]);
    assert_ne!(recovered["run"]["state"], "SUCCEEDED");
    for field in [
        "exit_code",
        "exit_signal",
        "started_at",
        "finished_at",
        "prompt_digest",
        "base_commit_sha",
        "prepared_state_id",
        "capture",
    ] {
        assert_eq!(recovered["run"][field], before["run"][field], "{field}");
    }
    assert_eq!(recovered["worker_attempts"], before["worker_attempts"]);
    assert_eq!(recovered["workspace"]["state"], "RECLAIMED");
    assert!(!tree.parent().unwrap().exists());
    let artifacts = fixture.query(&["runs", "artifacts", id]);
    assert!(artifacts["artifact_manifest_id"].is_string());
    let files = artifacts["artifacts"].as_array().unwrap();
    for kind in [
        "stdout",
        "stderr",
        "events",
        "prompt",
        "tracked_patch",
        "untracked_archive",
        "manifest",
    ] {
        assert_eq!(
            files.iter().filter(|file| file["kind"] == kind).count(),
            1,
            "{kind}"
        );
    }
    for file in files {
        let bytes = fs::read(
            fixture
                .path
                .join("storage")
                .join(file["relative_path"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(file["size_bytes"], bytes.len() as u64);
        assert_eq!(file["sha256"], format!("{:x}", Sha256::digest(&bytes)));
    }
    let manifest_file = files
        .iter()
        .find(|file| file["kind"] == "manifest")
        .unwrap();
    let manifest: Value = serde_json::from_slice(
        &fs::read(
            fixture
                .path
                .join("storage")
                .join(manifest_file["relative_path"].as_str().unwrap()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["run_id"], id);
    assert_eq!(manifest["workspace_id"], workspace_id);
    assert_eq!(manifest["exit_code"], 7);
    assert_eq!(manifest["prompt_digest"], before["run"]["prompt_digest"]);
    assert_eq!(
        manifest["profile"]["digest"],
        before["worker_attempts"][0]["profile_digest"]
    );
    assert_eq!(manifest["capture"], before["run"]["capture"]);

    let restored = fixture.path.join("restored");
    fixture.git(
        &fixture.repo,
        &[
            "clone",
            "--quiet",
            "--no-hardlinks",
            fixture.repo.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    let artifact_path = |kind: &str| {
        fixture.path.join("storage").join(
            files.iter().find(|file| file["kind"] == kind).unwrap()["relative_path"]
                .as_str()
                .unwrap(),
        )
    };
    fixture.git(
        &restored,
        &[
            "apply",
            "--binary",
            artifact_path("tracked_patch").to_str().unwrap(),
        ],
    );
    checked(
        &Command::new("/usr/bin/tar")
            .arg("-xpf")
            .arg(artifact_path("untracked_archive"))
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read(restored.join("tracked")).unwrap(),
        b"recovered tracked\n"
    );
    assert_eq!(
        fs::read(restored.join("new source ; literal")).unwrap(),
        b"recovered untracked\n"
    );
    assert_eq!(
        fs::read(fixture.repo.join("tracked")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        fs::read(Path::new(fixture.prepared["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        b"original\n"
    );
    assert_eq!(
        fixture.query(&["recover", id, "--apply"])["action"],
        "already_reclaimed"
    );
    fixture.query(&["storage", "gc", "--apply"]);
    fixture.query(&["storage", "gc", "--apply"]);
    assert_eq!(fixture.query(&["runs", "show", id]), recovered);
    assert_eq!(fixture.query(&["runs", "artifacts", id]), artifacts);
    assert_eq!(
        fs::read(sentinel).unwrap(),
        b"never delete outside managed root\n"
    );
}

fn verified_artifacts(fixture: &Fixture, id: &str, label: usize) -> Value {
    let shown = fixture.query(&["runs", "show", id]);
    let report = fixture.query(&["runs", "artifacts", id]);
    let manifest_id = report["artifact_manifest_id"].as_str().unwrap();
    let files = report["artifacts"].as_array().unwrap();
    let mut manifest = Value::Null;
    for file in files {
        let relative = file["relative_path"].as_str().unwrap();
        assert!(relative.starts_with(&format!("artifacts/{manifest_id}/")));
        assert!(
            !Path::new(relative)
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        );
        let path = fixture.path.join("storage").join(relative);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(file["size_bytes"], bytes.len() as u64);
        assert_eq!(file["sha256"], format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o400
        );
        if file["kind"] == "manifest" {
            manifest = serde_json::from_slice(&bytes).unwrap();
        }
    }
    assert_eq!(manifest["run_id"], id);
    assert_eq!(manifest["workspace_id"], shown["workspace"]["workspace_id"]);
    assert_eq!(
        manifest["prepared_state_id"],
        shown["run"]["prepared_state_id"]
    );
    assert_eq!(manifest["base_commit_sha"], shown["run"]["base_commit_sha"]);
    assert_eq!(manifest["prompt_digest"], shown["run"]["prompt_digest"]);
    assert_eq!(manifest["capture"], shown["run"]["capture"]);
    assert_eq!(manifest["exit_code"], shown["run"]["exit_code"]);
    assert_eq!(manifest["exit_signal"], shown["run"]["exit_signal"]);
    assert_eq!(
        manifest["profile"]["digest"],
        shown["worker_attempts"][0]["profile_digest"]
    );
    for kind in [
        "stdout",
        "stderr",
        "events",
        "prompt",
        "tracked_patch",
        "untracked_archive",
        "manifest",
    ] {
        let matching: Vec<_> = files.iter().filter(|file| file["kind"] == kind).collect();
        assert_eq!(matching.len(), 1, "{kind}");
        if ["stdout", "stderr", "events"].contains(&kind) {
            assert_eq!(matching[0]["sha256"], manifest["capture"][kind]["sha256"]);
            assert_eq!(
                matching[0]["size_bytes"],
                manifest["capture"][kind]["size_bytes"]
            );
        }
    }
    let artifact_path = |kind: &str| {
        fixture.path.join("storage").join(
            files.iter().find(|file| file["kind"] == kind).unwrap()["relative_path"]
                .as_str()
                .unwrap(),
        )
    };
    let prompt = fs::read(artifact_path("prompt")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&prompt)),
        shown["run"]["prompt_digest"]
    );
    let restored = fixture.path.join(format!("restored-{label}"));
    fixture.git(
        &fixture.repo,
        &[
            "clone",
            "--quiet",
            "--no-hardlinks",
            fixture.repo.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    fixture.git(
        &restored,
        &[
            "apply",
            "--binary",
            artifact_path("tracked_patch").to_str().unwrap(),
        ],
    );
    checked(
        &Command::new("/usr/bin/tar")
            .arg("-xpf")
            .arg(artifact_path("untracked_archive"))
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read(restored.join("tracked")).unwrap(),
        format!("task-{label}").as_bytes()
    );
    assert_eq!(
        fs::read(restored.join("new-source")).unwrap(),
        format!("new-{label}").as_bytes()
    );
    report
}

fn durable_intent(fixture: &Fixture, workspace: &Value) -> String {
    let store =
        fabric_store::Store::open_existing_read_only(fixture.path.join("storage/fabric.db"))
            .unwrap();
    let id = workspace["workspace_id"].as_str().unwrap().parse().unwrap();
    store
        .cleanup_intent(&id)
        .unwrap()
        .expect("real cleanup must have persisted its deletion authority")
}

fn retained_item<'a>(storage: &'a Value, id: &str) -> &'a Value {
    let item = storage["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["run_id"] == id)
        .unwrap();
    assert!(item["id"].is_string());
    assert!(!item["retention_reason"].as_str().unwrap().is_empty());
    assert!(item["action"].as_str().unwrap().contains(id));
    item
}

#[cfg(target_os = "macos")]
fn crashed_owner(fixture: &Fixture, label: usize) -> String {
    use std::os::fd::{AsRawFd, FromRawFd};
    let wrapper = fixture.path.join(format!("crash-fake-{label}"));
    fs::write(
        &wrapper,
        format!(
            r#"#!/bin/sh
set -eu
if [ "$1" = --version ]; then exec '{fake}' "$@"; fi
root='{root}'
id={label}
trap 'touch "$root/stopped.$id"' EXIT
'{fake}' "$@"
printf '%s\n%s\n' "$$" "$PWD" > "$root/started.$id.tmp"
mv "$root/started.$id.tmp" "$root/started.$id"
i=0
while [ ! -f "$root/release.$id" ] && [ ! -f "$root/release-all" ]; do
  i=$((i+1)); test "$i" -lt 2000
  sleep 0.02
done
"#,
            fake = env!("CARGO_BIN_EXE_fabric-fake-harness"),
            root = fixture.path.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let prompt = fixture.path.join(format!("crash-prompt-{label}"));
    fs::write(
        &prompt,
        serde_json::to_vec(&json!({
            "required_files":[".deps/ready"],
            "writes":[{"path":"tracked","content":format!("task-{label}")},
                      {"path":"new-source","content":format!("new-{label}")}],
            "events":[{"type":"fixture","task":label}]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut command = fixture.cli();
    command
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(wrapper)
        .arg("--prompt-file")
        .arg(prompt)
        .args([
            "--copy-limit-bytes",
            "10485760",
            "--timeout-seconds",
            "45",
            "--json",
        ]);
    let mut supervisor = fixture.spawn(command);
    fixture.started(label);
    let ready = fs::read_to_string(fixture.path.join(format!("started.{label}"))).unwrap();
    let mut lines = ready.lines();
    let worker: usize = lines.next().unwrap().parse().unwrap();
    let tree = PathBuf::from(lines.next().unwrap());
    let workspace_id = tree
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    let runs = fixture.query(&["runs", "list"]);
    let id = runs
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["workspace_id"].as_str().unwrap() == workspace_id)
        .unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_owned();
    eventually(|| fixture.query(&["runs", "show", &id])["run"]["state"] == "RUNNING");
    let queue = unsafe { libc::kqueue() };
    assert!(queue >= 0);
    let queue = unsafe { fs::File::from_raw_fd(queue) };
    let mut event = libc::kevent {
        ident: worker as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    assert_eq!(
        unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        },
        0
    );
    let before = fixture.query(&["runs", "show", &id]);
    // This is an actual crash of our owned supervisor, not an injected DB state.
    supervisor.child.kill().unwrap();
    supervisor.child.wait().unwrap();
    assert_eq!(
        fixture.query(&["recover", &id, "--apply"])["action"],
        "retained"
    );
    fixture.query(&["storage", "gc", "--apply"]);
    assert_eq!(fixture.query(&["runs", "show", &id]), before);
    let active = fixture.query(&["storage", "status"]);
    assert!(retained_item(&active, &id)["usage_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        fs::read(tree.join("tracked")).unwrap(),
        format!("task-{label}").as_bytes()
    );
    fixture.release(label);
    // NOTE_EXIT was registered against the live worker before release. A kernel
    // event, not a sleep or PID lookup, proves the orphan really exited.
    let timeout = libc::timespec {
        tv_sec: 10,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        },
        1
    );
    assert_eq!(event.fflags & libc::NOTE_EXIT, libc::NOTE_EXIT);
    fixture.query(&["storage", "gc", "--apply"]);
    assert_eq!(
        fixture.query(&["recover", &id, "--apply"])["action"],
        "retained"
    );
    let after = fixture.query(&["runs", "show", &id]);
    assert_eq!(after["run"]["state"], "NEEDS_ATTENTION");
    for field in [
        "exit_code",
        "exit_signal",
        "finished_at",
        "capture",
        "artifact_manifest_id",
    ] {
        assert!(
            after["run"][field].is_null(),
            "unknown {field} must remain unknown"
        );
    }
    assert_eq!(after["run"]["started_at"], before["run"]["started_at"]);
    assert_eq!(after["worker_attempts"], before["worker_attempts"]);
    assert_eq!(
        fs::read(tree.join("new-source")).unwrap(),
        format!("new-{label}").as_bytes()
    );
    id
}

#[cfg(target_os = "macos")]
#[test]
fn twenty_real_workers_recover_only_proven_captureable_inactive_work_and_explain_retention() {
    use std::os::unix::fs::symlink;
    let mut fixture = Fixture::new();
    fixture.start_manager();
    let sentinel = fixture.path.join("outside-root");
    fs::create_dir(&sentinel).unwrap();
    fs::write(sentinel.join("sentinel"), b"outside-root-evidence").unwrap();
    let mut completed = Vec::new();
    let mut retained = Vec::new();
    let mut recovered = Vec::new();
    let mut preserved = Vec::new();
    let mut substituted = None;
    // Eight workers exercise real concurrent success and observed nonzero exit.
    for (labels, exit) in [(vec![0, 1, 2, 3, 4, 5], 0), (vec![6, 7], 7)] {
        let accepted = fixture.submit(&labels, "", exit);
        for (index, chunk) in labels.chunks(4).enumerate() {
            for label in chunk {
                fixture.started(*label);
            }
            for id in accepted["run_ids"]
                .as_array()
                .unwrap()
                .iter()
                .skip(index * 4)
                .take(chunk.len())
            {
                let id = id.as_str().unwrap();
                let report = fixture.query(&["runs", "show", id]);
                assert_eq!(report["run"]["state"], "RUNNING");
                assert_eq!(
                    fixture.query(&["recover", id, "--apply"])["action"],
                    "retained"
                );
                fixture.query(&["storage", "gc", "--apply"]);
                assert_eq!(
                    fixture.query(&["runs", "show", id])["run"]["state"],
                    "RUNNING"
                );
                assert!(Path::new(report["workspace"]["managed_path"].as_str().unwrap()).exists());
                assert!(
                    fixture.query(&["runs", "artifacts", id])["artifacts"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            }
            for label in chunk {
                fixture.release(*label);
            }
        }
        fixture.terminal(accepted["batch_id"].as_str().unwrap());
        for (label, id) in labels.iter().zip(accepted["run_ids"].as_array().unwrap()) {
            let id = id.as_str().unwrap();
            let shown = fixture.query(&["runs", "show", id]);
            assert_eq!(
                shown["run"]["state"],
                if exit == 0 { "SUCCEEDED" } else { "FAILED" }
            );
            assert_eq!(shown["run"]["exit_code"], exit);
            assert_eq!(shown["workspace"]["state"], "RECLAIMED");
            completed.push((id.to_owned(), verified_artifacts(&fixture, id, *label)));
        }
    }
    // Two genuine running workers are cancelled through the durable public API.
    let cancelled = fixture.submit(&[8, 9], "", 0);
    fixture.started(8);
    fixture.started(9);
    fixture.query(&["cancel", cancelled["batch_id"].as_str().unwrap()]);
    fixture.terminal(cancelled["batch_id"].as_str().unwrap());
    for (label, id) in [8, 9]
        .into_iter()
        .zip(cancelled["run_ids"].as_array().unwrap())
    {
        let id = id.as_str().unwrap();
        let shown = fixture.query(&["runs", "show", id]);
        assert_eq!(shown["run"]["state"], "CANCELLED");
        assert_eq!(shown["workspace"]["state"], "RECLAIMED");
        completed.push((id.to_owned(), verified_artifacts(&fixture, id, label)));
    }
    // Four real capture failures before stage creation. Marker corruption and
    // pathname substitution are hostile fixtures, never claimed as real crashes.
    for label in 10..14 {
        let accepted = fixture.submit(&[label], "capture", 7);
        fixture.started(label);
        fixture.release(label);
        let batch = accepted["batch_id"].as_str().unwrap();
        fixture.terminal(batch);
        fs::set_permissions(
            fixture.path.join("storage/artifact-staging"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let id = accepted["run_ids"][0].as_str().unwrap();
        let before = fixture.query(&["runs", "show", id]);
        assert_eq!(before["run"]["state"], "NEEDS_ATTENTION");
        assert_eq!(before["run"]["exit_code"], 7);
        assert!(before["run"]["capture"].is_object());
        let tree = PathBuf::from(before["workspace"]["managed_path"].as_str().unwrap());
        let accepted_results = fixture.query(&["results", batch]);
        assert!(
            accepted_results["tasks"][0]["result"].is_null()
                || accepted_results["tasks"][0]["result"]["artifact_manifest_id"].is_null()
        );
        if label == 12 {
            fs::write(
                tree.parent().unwrap().join(".fabric-owner"),
                b"corrupt-marker-evidence",
            )
            .unwrap();
        } else if label == 13 {
            fs::rename(
                &tree,
                tree.parent().unwrap().join("tree-preserved-evidence"),
            )
            .unwrap();
            symlink(&sentinel, &tree).unwrap();
        }
        let storage = fixture.query(&["storage", "status"]);
        let item = retained_item(&storage, id);
        if label == 13 {
            assert!(
                item["usage_bytes"].is_null(),
                "substituted symlink is not owned logical usage"
            );
            assert!(!item["diagnostic"].as_str().unwrap().is_empty());
        } else {
            assert!(item["usage_bytes"].as_u64().unwrap() > 0);
        }
        let inspection = fixture.query(&["recover", id]);
        assert_eq!(fixture.query(&["runs", "show", id]), before);
        if label < 12 {
            assert_eq!(inspection["action"], "would_recover");
            // Cover explicit recovery and GC-integrated recovery independently.
            if label == 10 {
                fixture.query(&["recover", id, "--apply"]);
            } else {
                fixture.query(&["storage", "gc", "--apply"]);
            }
            let after = fixture.query(&["runs", "show", id]);
            assert_ne!(after["run"]["state"], "SUCCEEDED");
            assert_eq!(after["workspace"]["state"], "RECLAIMED");
            for field in [
                "exit_code",
                "exit_signal",
                "started_at",
                "finished_at",
                "base_commit_sha",
                "prompt_digest",
                "prepared_state_id",
                "capture",
            ] {
                assert_eq!(after["run"][field], before["run"][field], "{field}");
            }
            assert_eq!(after["worker_attempts"], before["worker_attempts"]);
            let artifacts = verified_artifacts(&fixture, id, label);
            let result = fixture.query(&["results", batch]);
            let task = &result["tasks"][0];
            assert_eq!(
                task["state"], "NEEDS_ATTENTION",
                "never replay terminal queue ownership"
            );
            assert_eq!(
                task["result"]["artifact_manifest_id"],
                after["run"]["artifact_manifest_id"]
            );
            assert_eq!(task["result"]["cleanup"]["action"], "reclaimed");
            assert_eq!(task["result"]["exit_code"], 7);
            recovered.push((batch.to_owned(), id.to_owned(), result));
            completed.push((id.to_owned(), artifacts));
        } else {
            assert_eq!(inspection["action"], "retained");
            assert_eq!(
                fixture.query(&["recover", id, "--apply"])["action"],
                "retained"
            );
            fixture.query(&["storage", "gc", "--apply"]);
            assert_eq!(fixture.query(&["runs", "show", id]), before);
            if label == 12 {
                assert_eq!(
                    fs::read(tree.parent().unwrap().join(".fabric-owner")).unwrap(),
                    b"corrupt-marker-evidence"
                );
                assert_eq!(
                    fs::read(tree.join("tracked")).unwrap(),
                    format!("task-{label}").as_bytes()
                );
                let container = tree.parent().unwrap().to_path_buf();
                preserved.push((container.clone(), tree_evidence(&container)));
            } else {
                assert_eq!(fs::read_link(&tree).unwrap(), sentinel);
                assert_eq!(
                    fs::read(
                        tree.parent()
                            .unwrap()
                            .join("tree-preserved-evidence/tracked")
                    )
                    .unwrap(),
                    format!("task-{label}").as_bytes()
                );
                let held = tree.parent().unwrap().join("tree-preserved-evidence");
                preserved.push((held.clone(), tree_evidence(&held)));
                substituted = Some(tree.clone());
            }
            retained.push(id.to_owned());
        }
    }
    // Two deterministic simulated capture-crash boundaries: artifact limits
    // leave real partial staging files. Never remove those files to force retry.
    for label in 14..16 {
        let accepted = fixture.submit(&[label], "partial", 0);
        fixture.started(label);
        fixture.release(label);
        fixture.terminal(accepted["batch_id"].as_str().unwrap());
        let id = accepted["run_ids"][0].as_str().unwrap();
        let before = fixture.query(&["runs", "show", id]);
        assert_eq!(before["run"]["state"], "NEEDS_ATTENTION");
        let stage = fixture
            .path
            .join("storage/artifact-staging")
            .join(format!("a-{id}"));
        let evidence = tree_evidence(&stage);
        assert!(
            !evidence.is_empty(),
            "fixture must really reach partial durable capture"
        );
        assert_eq!(
            fixture.query(&["recover", id, "--apply"])["action"],
            "retained"
        );
        fixture.query(&["storage", "gc", "--apply"]);
        assert_eq!(tree_evidence(&stage), evidence);
        preserved.push((stage, evidence));
        assert_eq!(fixture.query(&["runs", "show", id]), before);
        let storage = fixture.query(&["storage", "status"]);
        assert!(retained_item(&storage, id)["usage_bytes"].as_u64().unwrap() > 0);
        retained.push(id.to_owned());
    }
    // Two simulated cleanup-crash boundaries reached by real cleanup: the
    // staging parent is nonwritable, so final unlink fails AFTER intent and
    // tree/marker deletion. Restoring permission does not forge any authority.
    for label in 16..18 {
        let accepted = fixture.submit(&[label], "cleanup", 0);
        fixture.started(label);
        fixture.release(label);
        fixture.terminal(accepted["batch_id"].as_str().unwrap());
        let id = accepted["run_ids"][0].as_str().unwrap();
        let before = fixture.query(&["runs", "show", id]);
        assert_eq!(before["run"]["state"], "SUCCEEDED");
        assert_eq!(before["workspace"]["state"], "CLEANUP_PENDING");
        let tree = Path::new(before["workspace"]["managed_path"].as_str().unwrap());
        assert!(!tree.exists());
        assert!(tree.parent().unwrap().is_dir());
        assert!(!tree.parent().unwrap().join(".fabric-owner").exists());
        let intent = durable_intent(&fixture, &before["workspace"]);
        let artifacts = verified_artifacts(&fixture, id, label);
        assert_eq!(durable_intent(&fixture, &before["workspace"]), intent);
        fs::set_permissions(
            fixture.path.join("storage/fork-staging"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fixture.query(&["storage", "gc", "--apply"]);
        let after = fixture.query(&["runs", "show", id]);
        assert_eq!(after["run"], before["run"]);
        assert_eq!(after["workspace"]["state"], "RECLAIMED");
        assert!(!tree.parent().unwrap().exists());
        assert_eq!(fixture.query(&["runs", "artifacts", id]), artifacts);
        completed.push((id.to_owned(), artifacts));
    }
    // The last two are true supervisor crashes with live orphan protection,
    // followed by kernel-observed exit. Unsealed output is correctly retained.
    for label in 18..20 {
        retained.push(crashed_owner(&fixture, label));
    }
    let runs = fixture.query(&["runs", "list"]);
    assert_eq!(runs.as_array().unwrap().len(), 20);
    assert_eq!(completed.len(), 14);
    assert_eq!(retained.len(), 6);
    let snapshot: Vec<_> = runs
        .as_array()
        .unwrap()
        .iter()
        .map(|run| {
            let id = run["run_id"].as_str().unwrap();
            (id.to_owned(), fixture.query(&["runs", "show", id]))
        })
        .collect();
    let storage = fixture.query(&["storage", "status"]);
    let measured: Vec<_> = storage["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["usage_bytes"].is_u64() && item["usage_bytes"] != 0)
        .map(|item| item["usage_bytes"].as_u64().unwrap())
        .collect();
    assert!(
        measured.windows(2).all(|pair| pair[0] >= pair[1]),
        "largest retained workspaces first"
    );
    for id in &retained {
        retained_item(&storage, id);
        assert_eq!(
            fixture.query(&["recover", id, "--apply"])["action"],
            "retained"
        );
    }
    for _ in 0..2 {
        fixture.query(&["storage", "gc", "--apply"]);
    }
    for (path, evidence) in preserved {
        assert_eq!(tree_evidence(&path), evidence);
    }
    assert_eq!(fs::read_link(substituted.unwrap()).unwrap(), sentinel);
    for (id, before) in snapshot {
        assert_eq!(fixture.query(&["runs", "show", &id]), before);
    }
    for (id, artifacts) in completed {
        assert_eq!(
            fixture.query(&["recover", &id, "--apply"])["action"],
            "already_reclaimed"
        );
        assert_eq!(fixture.query(&["runs", "artifacts", &id]), artifacts);
    }
    for (batch, id, result) in recovered {
        assert_eq!(fixture.query(&["results", &batch]), result);
        assert_eq!(fixture.query(&["status", &id])["terminal"], true);
    }
    for label in 0..20 {
        assert_eq!(
            fs::read_to_string(fixture.path.join(format!("started.{label}")))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
    assert_eq!(
        fs::read(sentinel.join("sentinel")).unwrap(),
        b"outside-root-evidence"
    );
    assert_eq!(
        fs::read(fixture.repo.join("tracked")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        fs::read(Path::new(fixture.prepared["prepared_path"].as_str().unwrap()).join("tracked"))
            .unwrap(),
        b"original\n"
    );
}

fn tree_evidence(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(!metadata.file_type().is_symlink());
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), files);
            }
        } else {
            files.push((
                path.strip_prefix(root).unwrap().to_path_buf(),
                fs::read(path).unwrap(),
            ));
        }
    }
    let mut files = Vec::new();
    visit(path, path, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}
