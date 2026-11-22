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

fn artifact_container(fixture: &Fixture, report: &serde_json::Value) -> PathBuf {
    fixture
        .0
        .join("storage/artifacts")
        .join(report["artifact_manifest_id"].as_str().unwrap())
}

fn restored_workspace(fixture: &Fixture, report: &serde_json::Value) -> PathBuf {
    let artifacts = artifact_container(fixture, report);
    let repo = fixture.0.join("repo ; literal");
    let restored = fixture
        .0
        .join(format!("recovered-{}", report["run_id"].as_str().unwrap()));
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
    let patch = artifacts.join("tracked.patch");
    if fs::metadata(&patch).unwrap().len() != 0 {
        fixture.git(&restored, &["apply", "--binary", patch.to_str().unwrap()]);
    }
    success(
        &Command::new("/usr/bin/tar")
            .arg("-xpf")
            .arg(artifacts.join("untracked.tar"))
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap(),
    );
    restored
}

fn pending_workspace(fixture: &Fixture) -> serde_json::Value {
    let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER gc_test_lease AFTER UPDATE OF state ON workspaces
        WHEN NEW.state='CLEANUP_PENDING' BEGIN
        UPDATE workspaces SET lease_expires_at=9223372036854775807
        WHERE workspace_id=NEW.workspace_id; END;",
    )
    .unwrap();
    drop(db);
    let report = json(&fixture.run(0));
    assert_eq!(report["cleanup"]["reason"], "worker_lease_valid");
    let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
    db.execute_batch("DROP TRIGGER gc_test_lease; UPDATE workspaces SET lease_expires_at=NULL;")
        .unwrap();
    report
}

fn gc(fixture: &Fixture, mode: &str) -> serde_json::Value {
    json(
        &fixture
            .cli()
            .args(["storage", "gc", mode, "--json"])
            .output()
            .unwrap(),
    )
}

#[cfg(target_os = "macos")]
#[test]
fn reconciliation_preserves_expired_live_workers_then_recovers_crashed_owner() {
    reconcile_crashed_worker(false);
}

#[cfg(target_os = "macos")]
#[test]
fn reconciliation_protects_orphan_worker_that_closed_inherited_lease_descriptor() {
    reconcile_crashed_worker(true);
}

#[cfg(target_os = "macos")]
fn reconcile_crashed_worker(close_descriptors: bool) {
    use std::io::{BufRead, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::process::Stdio;

    let fixture = Fixture::new();
    fixture.setup();
    let ready = fixture.0.join("ready");
    let resume = fixture.0.join("resume");
    for path in [&ready, &resume] {
        let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }
    let close = if close_descriptors {
        "fd=3; while [ \"$fd\" -lt 64 ]; do eval \"exec $fd>&-\"; fd=$((fd+1)); done\n"
    } else {
        ""
    };
    let fake = fixture.script_fake(&format!(
        "{close}printf unique > interrupted-source\nprintf '%s\\n%s\\n' \"$$\" \"$PWD\" > '{}'\nread -r resume < '{}'\n",
        ready.display(), resume.display()
    ));
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, b"{}").unwrap();
    let mut supervisor = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(fake)
        .arg("--prompt-file")
        .arg(prompt)
        .args(["--copy-limit-bytes", "10485760", "--json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(fs::File::open(ready).unwrap()).lines();
    let worker: i32 = lines.next().unwrap().unwrap().parse().unwrap();
    let tree = PathBuf::from(lines.next().unwrap().unwrap());
    // Register against the observed live child before releasing its FIFO. NOTE_EXIT
    // is an actual kernel process event, not a sleep/PID-reuse assumption.
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
    let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
    db.execute("UPDATE workspaces SET lease_expires_at=0", [])
        .unwrap();
    db.execute("UPDATE worker_leases SET expires_at=0", [])
        .unwrap();
    let run_id: String = db
        .query_row("SELECT run_id FROM runs", [], |r| r.get(0))
        .unwrap();
    let snapshot = || {
        db.query_row(
            "SELECT state, exit_code, artifact_manifest_id FROM runs WHERE run_id=?1",
            [&run_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i32>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .unwrap()
    };
    // Wait for the durable launch callback, not PID timing or a guessed sleep.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while snapshot().0 != "RUNNING" {
        assert!(
            std::time::Instant::now() < deadline,
            "launch callback did not commit"
        );
        std::thread::yield_now();
    }
    let before = snapshot();
    gc(&fixture, "--apply");
    assert_eq!(snapshot(), before);
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    let lease = fs::File::open(fixture.0.join("storage/leases").join(&run_id)).unwrap();
    let locked = unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if close_descriptors {
        assert_eq!(locked, 0, "fixture closed the inherited lock");
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_UN) }, 0);
    } else {
        assert_ne!(locked, 0, "orphan still owns the inherited lock");
    }
    gc(&fixture, "--apply");
    assert_eq!(
        snapshot(),
        before,
        "an orphaned live worker is still protected"
    );
    assert_eq!(
        fs::read(tree.join("interrupted-source")).unwrap(),
        b"unique"
    );
    fs::OpenOptions::new()
        .write(true)
        .open(resume)
        .unwrap()
        .write_all(b"finish\n")
        .unwrap();
    let deadline = libc::timespec {
        tv_sec: 5,
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
                &deadline,
            )
        },
        1
    );
    assert_eq!(event.fflags & libc::NOTE_EXIT, libc::NOTE_EXIT);
    gc(&fixture, "--apply");
    assert_eq!(snapshot(), ("NEEDS_ATTENTION".into(), None, None));
    let workspace: (String, String) = db
        .query_row("SELECT state, retention_reason FROM workspaces", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(workspace.0, "RECOVERY_REQUIRED");
    assert!(workspace.1.contains("Interrupted"));
    assert_eq!(
        db.query_row("SELECT count(*) FROM storage_reservations", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        fs::read(tree.join("interrupted-source")).unwrap(),
        b"unique"
    );
    let stable = snapshot();
    gc(&fixture, "--apply");
    assert_eq!(snapshot(), stable);
    assert!(tree.exists());
}

#[test]
fn gc_dry_run_is_read_only_and_live_lease_is_not_cleanup_permission() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    fixture.setup();
    let report = pending_workspace(&fixture);
    let db_path = fixture.0.join("storage/fabric.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let committed_database = fs::read(&db_path).unwrap();
    db.execute(
        "UPDATE workspaces SET lease_expires_at=9223372036854775807",
        [],
    )
    .unwrap();
    assert_eq!(fs::read(&db_path).unwrap(), committed_database);
    let wal_path = fixture.0.join("storage/fabric.db-wal");
    let committed_wal = fs::read(&wal_path).unwrap();
    assert!(committed_wal.len() > 32);
    assert!(fixture.0.join("storage/fabric.db-shm").is_file());
    let artifacts = artifact_container(&fixture, &report);
    let artifact_bytes: std::collections::BTreeMap<_, _> = fs::read_dir(&artifacts)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect();
    assert_eq!(
        gc(&fixture, "--dry-run")["items"][0]["reason"],
        "worker_lease_valid"
    );
    assert_eq!(fs::read(&db_path).unwrap(), committed_database);
    assert_eq!(fs::read(&wal_path).unwrap(), committed_wal);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM cleanup_intents", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    assert_eq!(fs::read(tree.join("tracked")).unwrap(), b"worker tracked");
    assert_eq!(
        fs::read(tree.join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
    for (name, bytes) in &artifact_bytes {
        assert_eq!(&fs::read(artifacts.join(name)).unwrap(), bytes);
    }
    drop(db);
    assert_eq!(
        gc(&fixture, "--apply")["items"][0]["reason"],
        "worker_lease_valid"
    );
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("UPDATE workspaces SET lease_expires_at=NULL", [])
        .unwrap();
    drop(db);
    let before = fs::read(&db_path).unwrap();
    let root_entries: std::collections::BTreeSet<_> = fs::read_dir(fixture.0.join("storage"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let first = gc(&fixture, "--dry-run");
    assert_eq!(first["items"][0]["action"], "would_reclaim");
    assert_eq!(first, gc(&fixture, "--dry-run"));
    assert_eq!(fs::read(&db_path).unwrap(), before);
    let after_entries: std::collections::BTreeSet<_> = fs::read_dir(fixture.0.join("storage"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(root_entries.is_subset(&after_entries));
    for added in after_entries.difference(&root_entries) {
        assert!(added == "fabric.db-wal" || added == "fabric.db-shm");
        let metadata = fs::symlink_metadata(fixture.0.join("storage").join(added)).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), fs::metadata(&db_path).unwrap().uid());
        assert_eq!(metadata.mode() & 0o077, 0);
        assert_eq!(metadata.nlink(), 1);
    }
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    assert_eq!(fs::read(tree.join("tracked")).unwrap(), b"worker tracked");
    assert_eq!(
        fs::read(tree.join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
    for (name, bytes) in &artifact_bytes {
        assert_eq!(&fs::read(artifacts.join(name)).unwrap(), bytes);
    }
    assert_eq!(gc(&fixture, "--apply")["items"][0]["action"], "reclaimed");
    assert!(!tree.parent().unwrap().exists());
    assert_eq!(
        fs::read(restored_workspace(&fixture, &report).join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
}

#[test]
fn gc_refuses_unsafe_ownership_paths_manifests_and_noneligible_states() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for case in [
        "outside",
        "marker_missing",
        "marker_tamper",
        "container_link",
        "tree_link",
        "manifest_missing",
        "manifest_corrupt",
        "run_active",
        "provenance_tamper",
        "retention_reason",
        "active",
        "recovery",
        "retained",
    ] {
        let fixture = Fixture::new();
        fixture.setup();
        let report = pending_workspace(&fixture);
        let tree = PathBuf::from(report["workspace_path"].as_str().unwrap());
        let container = tree.parent().unwrap();
        let outside = fixture.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"outside must survive").unwrap();
        match case {
            "outside" => {
                let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
                db.execute(
                    "UPDATE workspaces SET managed_path=?1",
                    [outside.to_str().unwrap()],
                )
                .unwrap();
            }
            "marker_missing" => fs::remove_file(container.join(".fabric-owner")).unwrap(),
            "marker_tamper" => fs::write(container.join(".fabric-owner"), b"wrong owner").unwrap(),
            "container_link" => {
                fs::rename(container, fixture.0.join("saved-container")).unwrap();
                symlink(&outside, container).unwrap();
            }
            "tree_link" => {
                fs::rename(&tree, container.join("saved-tree")).unwrap();
                symlink(&outside, &tree).unwrap();
            }
            "manifest_missing" | "manifest_corrupt" => {
                let artifacts = artifact_container(&fixture, &report);
                fs::set_permissions(&artifacts, fs::Permissions::from_mode(0o700)).unwrap();
                if case == "manifest_missing" {
                    fs::remove_file(artifacts.join("manifest.json")).unwrap();
                } else {
                    fs::set_permissions(
                        artifacts.join("manifest.json"),
                        fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                    let mut bytes = fs::read(artifacts.join("manifest.json")).unwrap();
                    bytes[0] = b'X';
                    fs::write(artifacts.join("manifest.json"), bytes).unwrap();
                    fs::set_permissions(
                        artifacts.join("manifest.json"),
                        fs::Permissions::from_mode(0o400),
                    )
                    .unwrap();
                }
                fs::set_permissions(&artifacts, fs::Permissions::from_mode(0o500)).unwrap();
            }
            "run_active" | "provenance_tamper" | "retention_reason" => {
                let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
                let sql = match case {
                    "run_active" => "UPDATE runs SET state='RUNNING'",
                    "provenance_tamper" => "UPDATE runs SET harness_version='tampered'",
                    _ => "UPDATE workspaces SET retention_reason='keep for inspection'",
                };
                db.execute(sql, []).unwrap();
            }
            state => {
                let state = match state {
                    "active" => "ACTIVE",
                    "recovery" => "RECOVERY_REQUIRED",
                    _ => "RETAINED",
                };
                let db = rusqlite::Connection::open(fixture.0.join("storage/fabric.db")).unwrap();
                db.execute("UPDATE workspaces SET state=?1", [state])
                    .unwrap();
            }
        }
        let dry = gc(&fixture, "--dry-run");
        let applied = gc(&fixture, "--apply");
        for item in [&dry["items"][0], &applied["items"][0]] {
            assert_eq!(item["action"], "refused", "{case}: {item}");
            assert!(!item["reason"].as_str().unwrap().is_empty());
        }
        assert_eq!(
            fs::read(outside.join("sentinel")).unwrap(),
            b"outside must survive",
            "{case}"
        );
        let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
        assert!(
            store
                .cleanup_intent(&report["workspace_id"].as_str().unwrap().parse().unwrap())
                .unwrap()
                .is_none(),
            "{case}"
        );
        assert!(fs::symlink_metadata(container).is_ok(), "{case}");
    }
}

#[test]
fn gc_reports_unknown_paths_and_unlinks_internal_links_without_touching_targets() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    fixture.setup();
    let report = pending_workspace(&fixture);
    let outside = fixture.0.join("outside-sentinel");
    fs::write(&outside, b"not a cleanup target").unwrap();
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    symlink(&outside, tree.join("post-capture-link")).unwrap();
    let unknown = fixture.0.join("storage/fork-staging/unknown");
    fs::create_dir(&unknown).unwrap();
    fs::write(unknown.join("unknown-bytes"), b"unknown must survive").unwrap();
    let dry = gc(&fixture, "--dry-run");
    assert_eq!(dry["unknown_paths"][0]["reason"], "unknown_path");
    assert!(tree.join("post-capture-link").is_symlink());
    let applied = gc(&fixture, "--apply");
    assert_eq!(applied["items"][0]["action"], "reclaimed");
    assert_eq!(applied["unknown_paths"][0]["reason"], "unknown_path");
    assert_eq!(fs::read(outside).unwrap(), b"not a cleanup target");
    assert_eq!(
        fs::read(unknown.join("unknown-bytes")).unwrap(),
        b"unknown must survive"
    );
}

#[test]
fn gc_reclaims_only_captured_workspaces_and_preserves_recoverable_bytes() {
    let fixture = Fixture::new();
    let (repo, base) = fixture.setup();
    let report = json(&fixture.run(0));
    let tree = Path::new(report["workspace_path"].as_str().unwrap());
    assert!(
        !tree.parent().unwrap().exists(),
        "captured fork must be reclaimed"
    );
    let manifest = verified_manifest(&fixture, &report);
    let artifacts = fixture
        .0
        .join("storage/artifacts")
        .join(manifest["manifest_id"].as_str().unwrap());
    let restored = fixture.0.join("recovered");
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
    fixture.git(
        &restored,
        &[
            "apply",
            "--binary",
            artifacts.join("tracked.patch").to_str().unwrap(),
        ],
    );
    success(
        &Command::new("/usr/bin/tar")
            .arg("-xpf")
            .arg(artifacts.join("untracked.tar"))
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read(restored.join("tracked")).unwrap(),
        b"worker tracked"
    );
    assert_eq!(
        fs::read(restored.join("new source ; literal")).unwrap(),
        b"worker untracked"
    );
    assert!(!fs::read(artifacts.join("stdout.log")).unwrap().is_empty());
    assert_eq!(
        fs::read(Path::new(base["prepared_path"].as_str().unwrap()).join(".deps/ready")).unwrap(),
        b"ready"
    );
    let first = fixture
        .cli()
        .args(["storage", "gc", "--dry-run", "--json"])
        .output()
        .unwrap();
    let second = fixture
        .cli()
        .args(["storage", "gc", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(json(&first)["items"][0]["reason"], "already_reclaimed");
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
    assert_tree_stopped(&fixture, &report);
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
    let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
    assert_eq!(workspace.state, fabric_domain::WorkspaceState::Reclaimed);
    assert!(workspace.lease_expires_at.is_none());
    let total: u64 = files.iter().map(|file| file.size_bytes).sum();
    assert_eq!(files.len(), 14);
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
    assert_eq!(manifest["source_scope"], "tracked_and_nonignored_untracked");
    assert_eq!(manifest["format_version"], 2);
    assert_eq!(manifest["recipe_digest"].as_str().unwrap().len(), 64);
    assert!(total <= manifest["artifact_limit_bytes"].as_u64().unwrap());
    manifest
}

fn assert_tree_stopped(fixture: &Fixture, report: &serde_json::Value) {
    let workspace = restored_workspace(fixture, report);
    assert!(
        !Path::new(report["workspace_path"].as_str().unwrap())
            .parent()
            .unwrap()
            .exists()
    );
    for name in ["parent.heartbeat", "child.heartbeat"] {
        let bytes = fs::read(workspace.join(name)).unwrap();
        assert!(!bytes.is_empty());
        let pid = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("pid="));
        if let Some(pid) = pid {
            let state = Command::new("/bin/ps")
                .args(["-p", pid, "-o", "stat="])
                .output()
                .unwrap();
            let state = String::from_utf8(state.stdout).unwrap();
            assert!(
                state.trim().is_empty() || state.trim().starts_with('Z'),
                "writer {pid} survived cleanup: {state}"
            );
        }
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
    wait_for_tree(&fixture);
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
    assert_tree_stopped(&fixture, &report);
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
    assert_tree_stopped(&fixture, &serde_json::to_value(&report).unwrap());
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
    assert_tree_stopped(&fixture, &report);
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
    wait_for_tree(&fixture);
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
    assert_tree_stopped(&fixture, &report["tasks"][1]["result"]);
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
    assert_tree_stopped(&fixture, &serde_json::to_value(timed).unwrap());
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
fn foreground_fake_process_records_provenance_exit_and_recovers_changes_after_reclaim() {
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
        assert_eq!(workspace.state, WorkspaceState::Reclaimed);
        assert!(workspace.retention_reason.is_none());
        assert!(
            !Path::new(&workspace.managed_path)
                .parent()
                .unwrap()
                .exists()
        );
        let tree = restored_workspace(&fixture, &report);
        assert_eq!(
            fs::read_to_string(tree.join("tracked")).unwrap(),
            "worker tracked"
        );
        assert_eq!(
            fs::read_to_string(tree.join("new source ; literal")).unwrap(),
            "worker untracked"
        );
        assert!(!tree.join("INJECTED").exists());
        let container = artifact_container(&fixture, &report);
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
        assert!(
            !tree.join(".deps").exists(),
            "ignored dependencies are not source artifacts"
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
        // Durable evidence remains usable after the entire original container is gone.
        assert!(
            !Path::new(&workspace.managed_path)
                .parent()
                .unwrap()
                .exists()
        );
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
        let tree = restored_workspace(&fixture, &report);
        let container = artifact_container(&fixture, &report);
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
    let container = artifact_container(&fixture, &report);
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
        assert_eq!(workspace.state, fabric_domain::WorkspaceState::Reclaimed);
        assert!(!Path::new(&workspace.managed_path).exists());
        let restored = restored_workspace(&fixture, result);
        assert_eq!(
            fs::read_to_string(restored.join("tracked")).unwrap(),
            format!("task-{i}")
        );
        assert!(!restored.join(".deps").exists());
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
fn untracked_archive_restores_bytes_modes_links_and_excludes_ignored_dependencies() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let (repo, _) = fixture.setup();
    let executable = fixture.script_fake(
        r#"
mkdir 'source dir'
chmod 750 'source dir'
printf '\000original\377' > 'source dir/file
with newline'
chmod 751 'source dir/file
with newline'
ln -s 'file
with newline' 'source dir/link'
printf 'ignored unique bytes' > .deps/ignored
printf 'tracked result' > tracked
/usr/bin/mkfifo .deps/ignored-pipe
mkdir 'source dir/long path'
i=0; name='source dir/long path/'; while [ "$i" -lt 120 ]; do name="${name}x"; i=$((i+1)); done
printf 'long path bytes' > "$name"
"#,
    );
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "archive fixture").unwrap();
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
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let run = store
        .run(&report["run_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    let files = store.artifacts(&run.run_id).unwrap();
    let archive = files
        .iter()
        .find(|file| file.kind == "untracked_archive")
        .expect("complete source capture must publish a restorable untracked archive");
    let archive = fixture.0.join("storage").join(&archive.relative_path);
    let bytes = fs::read(&archive).unwrap();
    assert!(
        !bytes
            .windows(b"ignored unique bytes".len())
            .any(|part| part == b"ignored unique bytes")
    );
    verified_manifest(&fixture, &report);
    assert_eq!(
        store.workspace(&run.workspace_id).unwrap().unwrap().state,
        fabric_domain::WorkspaceState::Reclaimed
    );
    // Restore into a fresh committed checkout after the worker tree was reclaimed.
    let restored = fixture.0.join("restored archive");
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
    success(
        &Command::new("/usr/bin/tar")
            .arg("-xpf")
            .arg(&archive)
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read(restored.join("source dir/file\nwith newline")).unwrap(),
        b"\0original\xff"
    );
    assert_eq!(
        fs::metadata(restored.join("source dir/file\nwith newline"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    assert_eq!(
        fs::metadata(restored.join("source dir"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    assert_eq!(
        fs::read_link(restored.join("source dir/link")).unwrap(),
        Path::new("file\nwith newline")
    );
    let long = format!("source dir/long path/{}", "x".repeat(120));
    assert_eq!(fs::read(restored.join(long)).unwrap(), b"long path bytes");
    assert!(!restored.join(".deps").exists());
    assert_eq!(fs::read(repo.join("tracked")).unwrap(), b"original");
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
    let tree = restored_workspace(&fixture, &report);
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
    for failure in [
        "promotion",
        "registration",
        "limit",
        "tampered_output",
        "outside_link",
        "relative_outside_link",
        "untracked_limit",
        "metadata_limit",
        "special_file",
    ] {
        let fixture = Fixture::new();
        fixture.setup();
        let root = fixture.0.join("storage");
        let sentinel = fixture.0.join("outside-sentinel");
        fs::write(&sentinel, "outside secret target never captured").unwrap();
        let absolute_link = format!("/bin/ln -s '{}' outside-link", sentinel.display());
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
            "outside_link" => absolute_link.as_str(),
            "relative_outside_link" => {
                "/bin/mkdir links; /bin/ln -s ../../../../../outside-sentinel links/outside-link"
            }
            "untracked_limit" => {
                "printf 'unique untracked recovery' > unique-large.bin; /usr/bin/head -c 65536 /dev/zero >> unique-large.bin"
            }
            "metadata_limit" => {
                "i=0; while [ \"$i\" -lt 40 ]; do : > \"empty-$i\"; i=$((i+1)); done"
            }
            "special_file" => "/usr/bin/mkfifo untracked-fifo",
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
        if matches!(failure, "untracked_limit" | "metadata_limit") {
            command.args(["--max-artifact-bytes", "16384"]);
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
            fs::read(&sentinel).unwrap(),
            b"outside secret target never captured"
        );
        if failure == "untracked_limit" {
            let mut expected = b"unique untracked recovery".to_vec();
            expected.resize(expected.len() + 65536, 0);
            assert_eq!(fs::read(tree.join("unique-large.bin")).unwrap(), expected);
        }
        if matches!(failure, "untracked_limit" | "metadata_limit") {
            let stage = root.join(format!("artifact-staging/a-{id}"));
            let size: u64 = fs::read_dir(stage)
                .unwrap()
                .map(|entry| entry.unwrap().metadata().unwrap().len())
                .sum();
            assert!(
                size <= 16384,
                "tar metadata and partial output must respect the cap"
            );
        }
        if matches!(failure, "outside_link" | "relative_outside_link") {
            assert!(
                workspace
                    .retention_reason
                    .as_deref()
                    .unwrap()
                    .contains("symlink")
            );
            let stage = root.join(format!("artifact-staging/a-{id}"));
            for file in fs::read_dir(stage).unwrap() {
                let bytes = fs::read(file.unwrap().path()).unwrap();
                assert!(
                    !bytes
                        .windows(b"outside secret target never captured".len())
                        .any(|part| part == b"outside secret target never captured")
                );
            }
        }
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

#[test]
fn storage_admission_cli_denies_before_worker_and_reports_usage() {
    let fixture = Fixture::new();
    fixture.setup();
    let launched = fixture.0.join("launched");
    let fake = fixture.script_fake(&format!("touch '{}'", launched.display()));
    let prompt = fixture.0.join("storage-prompt");
    fs::write(&prompt, "{}").unwrap();
    let output = fixture
        .cli()
        .args(["run", "test", "--harness", "fake", "--fake-binary"])
        .arg(fake)
        .arg("--prompt-file")
        .arg(prompt)
        .args(["--min-free-bytes", "18446744073709551615", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let failure: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        failure["error"]["message"]
            .as_str()
            .unwrap()
            .contains("storage_admission"),
        "{failure}"
    );
    assert!(!launched.exists());
    assert!(!fixture.0.join("storage/fork-staging").exists());
    let status = json(
        &fixture
            .cli()
            .args(["storage", "status", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        status["accounting"],
        "logical_bytes_best_effort_not_exclusive"
    );
    assert!(status["managed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(status["caches"][0]["retention_reason"], "selected_state");
    let manifest = fixture.0.join("tasks.json");
    fs::write(&manifest, r#"[{"id":"one","prompt_file":"storage-prompt"},{"id":"two","prompt_file":"storage-prompt"}]"#).unwrap();
    let output = batch(&fixture, &manifest, "2")
        .arg("--fake-binary")
        .arg(fixture.0.join("fake-script"))
        .args([
            "--max-managed-bytes",
            "3145728",
            "--min-free-bytes",
            "1",
            "--max-workspace-bytes",
            "1048576",
            "--max-log-bytes",
            "1048576",
            "--max-artifact-bytes",
            "1048576",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|task| task["error"]
                .as_str()
                .is_some_and(|message| message.contains("storage_admission"))),
        "{report}"
    );
    assert!(!launched.exists());
    let recipe = fixture.0.join("denied-recipe.toml");
    fs::write(
        &recipe,
        format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c',\"touch '{}'\"]\n",
            launched.display()
        ),
    )
    .unwrap();
    let output = fixture
        .cli()
        .args(["project", "prepare", "test", "--recipe"])
        .arg(&recipe)
        .args(["--min-free-bytes", "18446744073709551615", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap()
            .contains("storage_admission")
    );
    assert!(!launched.exists());
}

struct FixedDisk(u64);
impl fabric_core::workspace::storage::DiskMeter for FixedDisk {
    fn free_bytes(&self, _: &Path) -> Result<u64, fabric_domain::FabricError> {
        Ok(self.0)
    }
}

fn storage_options() -> fabric_core::run::RunOptions {
    let mut options = fabric_core::run::RunOptions {
        copy_limit_bytes: 10 * 1024 * 1024,
        max_log_bytes: 1024 * 1024,
        max_artifact_bytes: 1024 * 1024,
        ..Default::default()
    };
    options.storage.policy.max_workspace_bytes = 128 * 1024;
    options.storage.policy.min_free_bytes = 1;
    options.storage.meter = std::sync::Arc::new(FixedDisk(u64::MAX));
    options
}

#[test]
fn storage_injected_low_free_and_managed_cap_never_launch() {
    let fixture = Fixture::new();
    fixture.setup();
    let marker = fixture.0.join("launched");
    let harness = fabric_runner::FakeHarness::new(
        fixture.script_fake(&format!("touch '{}'", marker.display())),
    )
    .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options();
    options.storage.meter = std::sync::Arc::new(FixedDisk(0));
    let denied = fabric_core::run::foreground(
        &fixture.0.join("storage"),
        "test",
        &prompt,
        &harness,
        &options,
    )
    .err()
    .unwrap();
    assert!(denied.message.contains("storage_admission"), "{denied}");
    let recipe = fixture.0.join("denied-prepare.toml");
    fs::write(
        &recipe,
        format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c',\"touch '{}'\"]\n",
            marker.display()
        ),
    )
    .unwrap();
    let denied = fabric_core::workspace::storage::prepare(
        &fixture.0.join("storage"),
        "test",
        &recipe,
        &options.storage,
    )
    .unwrap_err();
    assert!(denied.message.contains("storage_admission"));
    options.storage.meter = std::sync::Arc::new(FixedDisk(u64::MAX));
    options.storage.policy.max_managed_bytes = options.storage.policy.max_workspace_bytes
        + options.max_log_bytes
        + options.max_artifact_bytes;
    let denied = fabric_core::run::foreground(
        &fixture.0.join("storage"),
        "test",
        &prompt,
        &harness,
        &options,
    )
    .err()
    .unwrap();
    assert!(denied.message.contains("storage_admission"), "{denied}");
    assert!(!marker.exists());
    assert!(
        fabric_core::workspace::storage::status(&fixture.0.join("storage"), &options.storage)
            .unwrap()
            .workspaces
            .is_empty()
    );
}

#[test]
fn storage_concurrent_workers_share_atomic_admission() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let marker = fixture.0.join("launched");
    let harness = fabric_runner::FakeHarness::new(
        fixture.script_fake(&format!("echo launched >> '{}'; sleep 1", marker.display())),
    )
    .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options();
    let reservation = options.storage.policy.max_workspace_bytes
        + options.max_log_bytes
        + options.max_artifact_bytes;
    options.storage.policy.max_managed_bytes =
        fabric_core::workspace::storage::usage(&root).unwrap() + reservation + 512 * 1024;
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    fabric_core::run::foreground(&root, "test", &prompt, &harness, &options)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.message.contains("storage_admission"))
    );
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 1);
    assert_eq!(
        fabric_core::workspace::storage::status(&root, &options.storage)
            .unwrap()
            .reserved_bytes,
        0
    );
}

#[test]
fn storage_evicts_only_unreferenced_cache_and_same_key_rebuilds() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let recipe = fixture.0.join("large-recipe.toml");
    fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','dd if=/dev/zero of=cache bs=1048576 count=2 2>/dev/null']\n").unwrap();
    let old = fabric_core::workspace::storage::prepare(&root, "test", &recipe, &Default::default())
        .unwrap();
    // A new recipe for the same committed tree becomes the selected state.
    fs::write(
        &recipe,
        "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf selected > selected']\n",
    )
    .unwrap();
    let selected =
        fabric_core::workspace::storage::prepare(&root, "test", &recipe, &Default::default())
            .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let harness = fabric_runner::FakeHarness::new(fixture.script_fake("exit 0")).unwrap();
    let mut options = storage_options();
    let reservation = options.storage.policy.max_workspace_bytes
        + options.max_log_bytes
        + options.max_artifact_bytes;
    options.storage.policy.max_managed_bytes =
        fabric_core::workspace::storage::usage(&root).unwrap() + reservation - 1024 * 1024;
    let report = fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).unwrap();
    assert_eq!(report.state, fabric_domain::RunState::Succeeded);
    assert!(!Path::new(&old.prepared_path).exists());
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    assert!(store.prepared_state(&old.state_id).unwrap().is_none());
    assert!(Path::new(&selected.prepared_path).exists());
    assert_eq!(
        store
            .cache_retention(&selected.state_id)
            .unwrap()
            .as_deref(),
        Some("historical_reference")
    );
    fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','dd if=/dev/zero of=cache bs=1048576 count=2 2>/dev/null']\n").unwrap();
    let rebuilt =
        fabric_core::workspace::storage::prepare(&root, "test", &recipe, &Default::default())
            .unwrap();
    assert_eq!(rebuilt.preparation_key, old.preparation_key);
    assert_ne!(rebuilt.state_id, old.state_id);
    fabric_project::prepare::validate_ready(&root, &rebuilt).unwrap();
    options.storage.policy.max_managed_bytes = reservation;
    assert!(fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).is_err());
    assert!(Path::new(&rebuilt.prepared_path).exists());
    assert!(Path::new(&selected.prepared_path).exists());
}

#[test]
fn storage_prepare_reuses_validated_ready_identity_without_fresh_admission() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let recipe = fixture.0.join("counted-recipe.toml");
    let counter = fixture.0.join("setup-count");
    fs::write(
        &recipe,
        format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c',\"printf x >> '{}'; printf cached > cached\"]\n",
            counter.display()
        ),
    )
    .unwrap();
    let original =
        fabric_core::workspace::storage::prepare(&root, "test", &recipe, &Default::default())
            .unwrap();
    // A different selection makes the requested cache otherwise eligible for eviction.
    let selection = fixture.0.join("selection.toml");
    fs::write(
        &selection,
        "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf selected > selected']\n",
    )
    .unwrap();
    fabric_core::workspace::storage::prepare(&root, "test", &selection, &Default::default())
        .unwrap();
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    assert_eq!(store.cache_retention(&original.state_id).unwrap(), None);
    let identity = fs::metadata(&original.prepared_path).unwrap();
    for free in [0, u64::MAX] {
        let mut options = fabric_core::workspace::storage::StorageOptions {
            meter: std::sync::Arc::new(FixedDisk(free)),
            ..Default::default()
        };
        options.policy.max_managed_bytes =
            fabric_core::workspace::storage::usage(&root).unwrap() + 4096;
        options.policy.max_workspace_bytes = options.policy.max_managed_bytes - 2;
        options.policy.max_log_bytes_per_run = 1;
        options.policy.max_artifact_bytes_per_run = 1;
        let reused =
            fabric_core::workspace::storage::prepare(&root, "test", &recipe, &options).unwrap();
        assert_eq!(reused.state_id, original.state_id);
        assert_eq!(reused.prepared_path, original.prepared_path);
        let current = fs::metadata(&reused.prepared_path).unwrap();
        assert_eq!(
            (current.dev(), current.ino()),
            (identity.dev(), identity.ino())
        );
        assert_eq!(fs::read(&counter).unwrap(), b"x");
        fabric_project::prepare::validate_ready(&root, &reused).unwrap();
    }
}

#[test]
fn storage_workspace_monitor_terminates_owned_group_and_zero_exit_is_failure() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let child_pid = fixture.0.join("child-pid");
    let binary = fixture.script_fake(&format!("trap 'exit 0' TERM\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\ndd if=/dev/zero of=oversized bs=1048576 count=3 2>/dev/null\nwhile :; do sleep 1; done", child_pid.display()));
    let harness = fabric_runner::FakeHarness::new(binary).unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options();
    options.storage.policy.max_workspace_bytes = 1024 * 1024;
    options.max_artifact_bytes = 8 * 1024 * 1024;
    let started = std::time::Instant::now();
    let report = fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(report.state, fabric_domain::RunState::Failed);
    assert_eq!(report.error_kind.as_deref(), Some("workspace_limit"));
    assert_eq!(report.exit_code, Some(0));
    assert!(!options.cancellation.is_cancelled());
    let run = fabric_store::Store::open(root.join("fabric.db"))
        .unwrap()
        .run(&report.run_id.parse().unwrap())
        .unwrap()
        .unwrap();
    assert!(run.artifact_manifest_id.is_some());
    assert_eq!(run.error_kind.as_deref(), Some("workspace_limit"));
    let pid = fs::read_to_string(child_pid).unwrap();
    let state = Command::new("/bin/ps")
        .args(["-p", &pid, "-o", "stat="])
        .output()
        .unwrap();
    let state = String::from_utf8(state.stdout).unwrap();
    assert!(
        state.trim().is_empty() || state.trim().starts_with('Z'),
        "owned descendant survived: {state}"
    );
}

struct PressureDisk(PathBuf);
impl fabric_core::workspace::storage::DiskMeter for PressureDisk {
    fn free_bytes(&self, _: &Path) -> Result<u64, fabric_domain::FabricError> {
        Ok(if self.0.exists() { 0 } else { u64::MAX })
    }
}

#[test]
fn storage_pressure_keeps_its_reason_and_captures_evidence() {
    let fixture = Fixture::new();
    fixture.setup();
    let pressure = fixture.0.join("pressure");
    let harness = fabric_runner::FakeHarness::new(fixture.script_fake(&format!(
        "trap 'exit 0' TERM\nprintf evidence > tracked\ntouch '{}'\nwhile :; do sleep 1; done",
        pressure.display()
    )))
    .unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options();
    options.storage.meter = std::sync::Arc::new(PressureDisk(pressure));
    let report = fabric_core::run::foreground(
        &fixture.0.join("storage"),
        "test",
        &prompt,
        &harness,
        &options,
    )
    .unwrap();
    assert_eq!(report.state, fabric_domain::RunState::Failed);
    assert_eq!(report.error_kind.as_deref(), Some("storage_pressure"));
    assert_eq!(report.exit_code, Some(0));
    assert!(!options.cancellation.is_cancelled());
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    assert!(
        store
            .run(&report.run_id.parse().unwrap())
            .unwrap()
            .unwrap()
            .artifact_manifest_id
            .is_some()
    );
}

#[test]
fn storage_fast_zero_exit_overlimit_retains_reason_when_capture_exceeds_cap() {
    let fixture = Fixture::new();
    fixture.setup();
    let root = fixture.0.join("storage");
    let harness = fabric_runner::FakeHarness::new(fixture.script_fake(
        "printf 'unique recovery bytes' > tracked\ndd if=/dev/zero of=oversized bs=1048576 count=3 2>/dev/null\nexit 0"
    )).unwrap();
    let prompt = fixture.0.join("prompt");
    fs::write(&prompt, "{}").unwrap();
    let mut options = storage_options();
    options.storage.policy.max_workspace_bytes = 1024 * 1024;
    assert!(fabric_core::run::foreground(&root, "test", &prompt, &harness, &options).is_err());
    let store = fabric_store::Store::open(root.join("fabric.db")).unwrap();
    let id = store.workspace_ids().unwrap().pop().unwrap();
    let workspace = store.workspace(&id).unwrap().unwrap();
    let run = store.run(&workspace.run_id).unwrap().unwrap();
    assert_eq!(run.state, fabric_domain::RunState::NeedsAttention);
    assert_eq!(run.error_kind.as_deref(), Some("workspace_limit"));
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(
        workspace.state,
        fabric_domain::WorkspaceState::RecoveryRequired
    );
    assert_eq!(
        fs::read(Path::new(&workspace.managed_path).join("tracked")).unwrap(),
        b"unique recovery bytes"
    );
    let status = fabric_core::workspace::storage::status(&root, &options.storage).unwrap();
    assert!(status.workspaces[0].usage_bytes.unwrap() > 3 * 1024 * 1024);
    assert!(!status.workspaces[0].retention_reason.is_empty());
}
