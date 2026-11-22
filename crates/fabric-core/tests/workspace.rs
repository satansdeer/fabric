use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use fabric_core::workspace::{WorkspaceRequest, create_workspace, runner_workspace};
use fabric_domain::{ErrorKind, FabricError, RunId, RunState, WorkspaceId, WorkspaceState};
use fabric_store::{PreparedState, Store};
use fabric_workspace::{ForkStrategy, LocalWorkspaceBackend, PreparedFork, WorkspaceBackend};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    state: PreparedState,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        fs::write(repo.join("tracked"), "base\n").unwrap();
        fs::write(repo.join(".gitignore"), ".deps/\n").unwrap();
        symlink("tracked", repo.join("source-link")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "initial"]);
        let root = temp.path().join("storage");
        fabric_project::register(&root, "fixture", &repo).unwrap();
        let root = root.canonicalize().unwrap();
        let recipe = temp.path().join("recipe.toml");
        fs::write(&recipe, "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir -p .deps/bin; printf ready > .deps/ready; ln -s ../ready .deps/bin/ready']\n").unwrap();
        let state = fabric_project::prepare::prepare(&root, "fixture", &recipe).unwrap();
        Self { temp, root, state }
    }
    fn backend(&self) -> LocalWorkspaceBackend {
        LocalWorkspaceBackend::new(&self.root, ForkStrategy::Copy).unwrap()
    }
    fn request(&self, id: &str) -> WorkspaceRequest {
        WorkspaceRequest {
            run_id: format!("run-{id}").parse().unwrap(),
            workspace_id: format!("workspace-{id}").parse().unwrap(),
            prepared_state_id: self.state.state_id.clone(),
            harness: "fake".into(),
            prompt_digest: "prompt-digest".into(),
            created_at: self.state.created_at + 1,
            lease_expires_at: self.state.created_at + 301,
        }
    }
    fn store(&self) -> Store {
        Store::open(self.root.join("fabric.db")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fn writable(path: &Path) {
            let meta = fs::symlink_metadata(path).unwrap();
            if meta.is_dir() {
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
                for entry in fs::read_dir(path).unwrap() {
                    writable(&entry.unwrap().path());
                }
            }
        }
        writable(self.temp.path());
    }
}
fn git(path: &Path, args: &[&str]) {
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
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

struct ObservedBackend<'a> {
    fixture: &'a Fixture,
    request: &'a WorkspaceRequest,
    calls: AtomicUsize,
    fail: bool,
}
impl WorkspaceBackend for ObservedBackend<'_> {
    fn fork(
        &self,
        state: &PreparedState,
        id: &WorkspaceId,
        limit: u64,
    ) -> Result<PreparedFork, FabricError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Reopening from inside the backend proves ownership was committed, not
        // merely inserted into an uncommitted orchestration transaction.
        let store = self.fixture.store();
        let run = store.run(&self.request.run_id).unwrap().unwrap();
        let workspace = store.workspace(id).unwrap().unwrap();
        assert_eq!(run.state, RunState::Preparing);
        assert_eq!(run.workspace_id, *id);
        assert_eq!(workspace.run_id, run.run_id);
        assert_eq!(workspace.state_id, state.state_id);
        assert_eq!(
            workspace.lease_expires_at,
            Some(self.request.lease_expires_at)
        );
        assert!(workspace.retention_reason.is_some());
        assert!(runner_workspace(&self.fixture.root, &run.run_id).is_err());
        if self.fail {
            let container = Path::new(&workspace.managed_path).parent().unwrap();
            fs::create_dir_all(container).unwrap();
            fs::set_permissions(container, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(container.join("unique-diagnostic"), "must survive").unwrap();
            return Err(FabricError {
                kind: ErrorKind::Storage,
                message: "injected fork failure".into(),
            });
        }
        self.fixture.backend().fork(state, id, limit)
    }
}

#[test]
fn durable_pair_precedes_backend_and_owned_siblings_isolate_same_relative_writes() {
    let fixture = Fixture::new();
    let a = fixture.request("a");
    let backend = ObservedBackend {
        fixture: &fixture,
        request: &a,
        calls: AtomicUsize::new(0),
        fail: false,
    };
    let first = create_workspace(&fixture.root, &backend, &a, fixture.state.size_bytes).unwrap();
    let b = fixture.request("b");
    let second = create_workspace(
        &fixture.root,
        &fixture.backend(),
        &b,
        fixture.state.size_bytes,
    )
    .unwrap();
    assert_eq!(first.run_id(), &a.run_id);
    assert_eq!(second.run_id(), &b.run_id);
    assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        fixture
            .store()
            .workspace(&a.workspace_id)
            .unwrap()
            .unwrap()
            .usage_bytes,
        fixture.state.size_bytes
    );
    assert_eq!(
        fs::read_link(first.path().join(".deps/bin/ready")).unwrap(),
        Path::new("../ready")
    );
    std::thread::scope(|scope| {
        scope.spawn(|| fs::write(first.path().join("source-link"), "first\n").unwrap());
        scope.spawn(|| fs::write(second.path().join("tracked"), "second\n").unwrap());
    });
    fs::write(first.path().join(".deps/ready"), "private").unwrap();
    assert_eq!(fs::read(first.path().join("tracked")).unwrap(), b"first\n");
    assert_eq!(
        fs::read(second.path().join("tracked")).unwrap(),
        b"second\n"
    );
    assert_eq!(
        fs::read(second.path().join(".deps/ready")).unwrap(),
        b"ready"
    );
    assert_eq!(
        fs::read(Path::new(&fixture.state.prepared_path).join("tracked")).unwrap(),
        b"base\n"
    );
    assert_ne!(
        fs::metadata(first.path().join("tracked")).unwrap().ino(),
        fs::metadata(second.path().join("tracked")).unwrap().ino()
    );
    let path = first.path().to_owned();
    drop(first);
    assert_eq!(
        runner_workspace(&fixture.root, &a.run_id).unwrap().path(),
        path
    );
    let mut reused_owner = fixture.request("c");
    reused_owner.workspace_id = a.workspace_id.clone();
    assert!(
        create_workspace(
            &fixture.root,
            &fixture.backend(),
            &reused_owner,
            fixture.state.size_bytes
        )
        .is_err()
    );
    assert!(fixture.store().run(&reused_owner.run_id).unwrap().is_none());
    assert!(create_workspace(&fixture.root, &backend, &a, fixture.state.size_bytes).is_err());
    assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn failed_fork_retains_durable_diagnostic_owner_and_is_never_runner_ready() {
    let fixture = Fixture::new();
    let request = fixture.request("failure");
    let backend = ObservedBackend {
        fixture: &fixture,
        request: &request,
        calls: AtomicUsize::new(0),
        fail: true,
    };
    let error =
        create_workspace(&fixture.root, &backend, &request, fixture.state.size_bytes).unwrap_err();
    assert!(error.message.contains("injected fork failure"), "{error}");
    let store = fixture.store();
    let run = store.run(&request.run_id).unwrap().unwrap();
    let workspace = store.workspace(&request.workspace_id).unwrap().unwrap();
    assert_eq!(run.run_id, request.run_id);
    assert_eq!(run.state, RunState::NeedsAttention);
    assert_eq!(run.error_kind.as_deref(), Some("workspace_fork"));
    assert_eq!(workspace.state, WorkspaceState::RecoveryRequired);
    assert!(
        workspace
            .retention_reason
            .as_deref()
            .unwrap()
            .contains("injected fork failure")
    );
    assert_eq!(
        fs::read(
            Path::new(&workspace.managed_path)
                .parent()
                .unwrap()
                .join("unique-diagnostic")
        )
        .unwrap(),
        b"must survive"
    );
    assert!(runner_workspace(&fixture.root, &request.run_id).is_err());
    assert!(create_workspace(&fixture.root, &backend, &request, fixture.state.size_bytes).is_err());
    assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn unknown_root_run_and_unregistered_fork_are_not_runner_authority() {
    let fixture = Fixture::new();
    let unknown: RunId = "unknown".parse().unwrap();
    assert!(runner_workspace(&fixture.root, &unknown).is_err());
    let raw_id = "raw".parse().unwrap();
    fixture
        .backend()
        .fork(&fixture.state, &raw_id, fixture.state.size_bytes)
        .unwrap();
    assert!(runner_workspace(&fixture.root, &unknown).is_err());
    let unknown_root = fixture.temp.path().join("unknown-root");
    fs::create_dir(&unknown_root).unwrap();
    fs::set_permissions(&unknown_root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        create_workspace(
            &unknown_root,
            &fixture.backend(),
            &fixture.request("unknown"),
            fixture.state.size_bytes
        )
        .is_err()
    );
    assert!(!unknown_root.join("fabric.db").exists());
}

#[test]
fn tampered_root_database_path_and_marker_are_refused_without_touching_unique_files() {
    let fixture = Fixture::new();
    let request = fixture.request("tamper");
    let owned = create_workspace(
        &fixture.root,
        &fixture.backend(),
        &request,
        fixture.state.size_bytes,
    )
    .unwrap();
    let tree = owned.path();
    let marker = tree.parent().unwrap().join(".fabric-owner");
    let marker_bytes = fs::read(&marker).unwrap();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
    let sentinel = outside.join("sentinel");
    fs::write(&sentinel, "never modify or remove").unwrap();
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(tree.join("unique"), "keep workspace edits").unwrap();
    let refused = || assert!(runner_workspace(&fixture.root, &request.run_id).is_err());
    let connection = rusqlite::Connection::open(fixture.root.join("fabric.db")).unwrap();
    for path in [outside.to_owned(), tree.join("../tree")] {
        connection
            .execute(
                "UPDATE workspaces SET managed_path=?1 WHERE workspace_id=?2",
                rusqlite::params![path.to_str().unwrap(), request.workspace_id.as_str()],
            )
            .unwrap();
        refused();
    }
    connection
        .execute(
            "UPDATE workspaces SET managed_path=?1 WHERE workspace_id=?2",
            rusqlite::params![tree.to_str().unwrap(), request.workspace_id.as_str()],
        )
        .unwrap();
    for invalid in [
        b"not Fabric".as_slice(),
        b"fabric.workspace.v1\nforged-owner\n".as_slice(),
    ] {
        fs::write(&marker, invalid).unwrap();
        refused();
    }
    let mut wrong_owner = marker_bytes.clone();
    wrong_owner[0] = b'X';
    fs::write(&marker, wrong_owner).unwrap();
    refused();
    fs::remove_file(&marker).unwrap();
    refused();
    symlink(&sentinel, &marker).unwrap();
    refused();
    fs::remove_file(&marker).unwrap();
    fs::hard_link(&sentinel, &marker).unwrap();
    refused();
    fs::remove_file(&marker).unwrap();
    fs::write(&marker, &marker_bytes).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
    refused();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    for directory in [
        tree.to_owned(),
        tree.parent().unwrap().to_owned(),
        fixture.root.join("fork-staging"),
    ] {
        let parked = directory.with_extension("parked");
        fs::rename(&directory, &parked).unwrap();
        symlink(&outside, &directory).unwrap();
        refused();
        fs::remove_file(&directory).unwrap();
        fs::rename(&parked, &directory).unwrap();
    }
    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o755)).unwrap();
    refused();
    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = fixture.temp.path().join("root-alias");
    symlink(&fixture.root, &alias).unwrap();
    assert!(runner_workspace(&alias, &request.run_id).is_err());
    assert!(
        create_workspace(
            &alias,
            &fixture.backend(),
            &fixture.request("alias"),
            fixture.state.size_bytes
        )
        .is_err()
    );
    drop(connection);
    let database = fixture.root.join("fabric.db");
    let parked_db = fixture.root.join("parked.db");
    fs::rename(&database, &parked_db).unwrap();
    symlink(&parked_db, &database).unwrap();
    refused();
    fs::remove_file(&database).unwrap();
    fs::rename(&parked_db, &database).unwrap();
    let moved_root = fixture.temp.path().join("moved-root");
    fs::rename(&fixture.root, &moved_root).unwrap();
    assert!(runner_workspace(&moved_root, &request.run_id).is_err());
    fs::rename(&moved_root, &fixture.root).unwrap();
    assert_eq!(
        runner_workspace(&fixture.root, &request.run_id)
            .unwrap()
            .path(),
        tree
    );
    assert_eq!(fs::read(&sentinel).unwrap(), b"never modify or remove");
    assert_eq!(
        fs::read(tree.join("unique")).unwrap(),
        b"keep workspace edits"
    );
}

#[test]
fn leases_are_durable_startup_reservations_not_cleanup_or_reconciliation_authority() {
    let fixture = Fixture::new();
    let mut request = fixture.request("lease");
    request.lease_expires_at = request.created_at;
    assert!(
        create_workspace(
            &fixture.root,
            &fixture.backend(),
            &request,
            fixture.state.size_bytes
        )
        .is_err()
    );
    assert!(fixture.store().run(&request.run_id).unwrap().is_none());
    request.lease_expires_at += 300;
    let owned = create_workspace(
        &fixture.root,
        &fixture.backend(),
        &request,
        fixture.state.size_bytes,
    )
    .unwrap();
    let connection = rusqlite::Connection::open(fixture.root.join("fabric.db")).unwrap();
    for lease in [None, Some(0)] {
        connection
            .execute(
                "UPDATE workspaces SET lease_expires_at=?1 WHERE workspace_id=?2",
                rusqlite::params![lease, request.workspace_id.as_str()],
            )
            .unwrap();
        assert!(runner_workspace(&fixture.root, &request.run_id).is_err());
        assert!(owned.path().join("tracked").exists());
        assert_eq!(
            fixture.store().run(&request.run_id).unwrap().unwrap().state,
            RunState::Preparing
        );
    }
}

#[test]
fn real_copy_admission_failure_retains_pair_and_never_creates_a_runnable_workspace() {
    let fixture = Fixture::new();
    let request = fixture.request("unadmitted");
    let error = create_workspace(&fixture.root, &fixture.backend(), &request, 0).unwrap_err();
    assert!(error.message.contains("admission"), "{error}");
    let store = fixture.store();
    let workspace = store.workspace(&request.workspace_id).unwrap().unwrap();
    assert_eq!(
        store.run(&request.run_id).unwrap().unwrap().state,
        RunState::NeedsAttention
    );
    assert_eq!(workspace.state, WorkspaceState::RecoveryRequired);
    assert_eq!(workspace.lease_expires_at, Some(request.lease_expires_at));
    assert!(!Path::new(&workspace.managed_path).exists());
    assert!(runner_workspace(&fixture.root, &request.run_id).is_err());
}

struct WrongPathBackend<'a> {
    fixture: &'a Fixture,
    outside: PathBuf,
}
impl WorkspaceBackend for WrongPathBackend<'_> {
    fn fork(
        &self,
        state: &PreparedState,
        id: &WorkspaceId,
        limit: u64,
    ) -> Result<PreparedFork, FabricError> {
        let mut fork = self.fixture.backend().fork(state, id, limit)?;
        fork.path = self.outside.clone();
        Ok(fork)
    }
}

#[test]
fn backend_cannot_publish_an_outside_path_as_runner_ready() {
    let fixture = Fixture::new();
    let request = fixture.request("outside");
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), "keep").unwrap();
    let backend = WrongPathBackend {
        fixture: &fixture,
        outside: outside.clone(),
    };
    assert!(create_workspace(&fixture.root, &backend, &request, fixture.state.size_bytes).is_err());
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"keep");
    assert!(!outside.join(".fabric-owner").exists());
    assert!(!fixture.temp.path().join(".fabric-owner").exists());
    assert_eq!(
        fixture
            .store()
            .workspace(&request.workspace_id)
            .unwrap()
            .unwrap()
            .state,
        WorkspaceState::RecoveryRequired
    );
    assert!(runner_workspace(&fixture.root, &request.run_id).is_err());
}

#[test]
fn automatic_backend_keeps_the_existing_apfs_or_admitted_copy_path() {
    let fixture = Fixture::new();
    let request = fixture.request("automatic");
    let backend = LocalWorkspaceBackend::new(&fixture.root, ForkStrategy::CloneOrCopy).unwrap();
    let owned =
        create_workspace(&fixture.root, &backend, &request, fixture.state.size_bytes).unwrap();
    fs::write(owned.path().join("tracked"), "private").unwrap();
    assert_eq!(
        fs::read(Path::new(&fixture.state.prepared_path).join("tracked")).unwrap(),
        b"base\n"
    );
    assert_eq!(
        fs::read(owned.path().join(".deps/bin/ready")).unwrap(),
        b"ready"
    );
}

#[test]
fn launch_revalidates_ownership_after_adapter_command_and_preserves_failed_run() {
    use fabric_runner::{AgentSpec, CommandSpec, HarnessAdapter, HarnessInfo};
    struct Tamper<'a> {
        fixture: &'a Fixture,
        id: std::cell::RefCell<Option<RunId>>,
    }
    impl HarnessAdapter for Tamper<'_> {
        fn name(&self) -> &str {
            "fake"
        }
        fn probe(&self) -> Result<HarnessInfo, FabricError> {
            Ok(HarnessInfo {
                version: "tamper-test".into(),
            })
        }
        fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
            let marker = spec.prompt_file.parent().unwrap().join(".fabric-owner");
            let text = fs::read_to_string(&marker).unwrap();
            self.id
                .replace(Some(text.lines().nth(2).unwrap().parse().unwrap()));
            fs::remove_file(marker).unwrap();
            Ok(CommandSpec {
                program: "/usr/bin/touch".into(),
                args: vec![
                    self.fixture
                        .temp
                        .path()
                        .join("MUST_NOT_RUN")
                        .into_os_string(),
                ],
                environment: Vec::new(),
            })
        }
    }
    let fixture = Fixture::new();
    let harness = Tamper {
        fixture: &fixture,
        id: std::cell::RefCell::new(None),
    };
    let prompt = fixture.temp.path().join("prompt");
    fs::write(&prompt, "literal prompt").unwrap();
    assert!(
        fabric_core::run::foreground(
            &fixture.root,
            "fixture",
            &prompt,
            &harness,
            &fabric_core::run::RunOptions {
                copy_limit_bytes: fixture.state.size_bytes,
                ..fabric_core::run::RunOptions::default()
            },
        )
        .is_err()
    );
    assert!(!fixture.temp.path().join("MUST_NOT_RUN").exists());
    let store = fixture.store();
    let run = store
        .run(harness.id.borrow().as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(run.state, RunState::NeedsAttention);
    assert_eq!(run.harness_version.as_deref(), Some("tamper-test"));
    assert_eq!(run.exit_code, None);
    assert_eq!(run.started_at, None);
    assert_eq!(run.pid_metadata, None);
    let workspace = store.workspace(&run.workspace_id).unwrap().unwrap();
    assert_eq!(workspace.state, WorkspaceState::RecoveryRequired);
    assert!(Path::new(&workspace.managed_path).exists());
}

#[test]
fn process_lifecycle_is_durable_before_each_following_phase() {
    let fixture = Fixture::new();
    let request = fixture.request("lifecycle");
    create_workspace(
        &fixture.root,
        &fixture.backend(),
        &request,
        fixture.state.size_bytes,
    )
    .unwrap();
    let mut store = fixture.store();
    let time = request.created_at;
    store
        .start_run(&request.run_id, "observed-version", 123, time)
        .unwrap();
    let running = fixture.store().run(&request.run_id).unwrap().unwrap();
    assert_eq!(running.state, RunState::Running);
    assert_eq!(running.started_at, Some(time));
    assert_eq!(running.exit_code, None);
    assert!(
        store
            .start_run(&request.run_id, "duplicate", 456, time)
            .is_err()
    );
    store
        .observe_run_exit(&request.run_id, Some(7), None, None, time + 1)
        .unwrap();
    let finalizing = fixture.store().run(&request.run_id).unwrap().unwrap();
    assert_eq!(finalizing.state, RunState::Finalizing);
    assert_eq!(finalizing.exit_code, Some(7));
    assert_eq!(finalizing.finished_at, None);
    let empty = fabric_domain::CapturedFile {
        size_bytes: 0,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
    };
    let capture = fabric_domain::CaptureMetadata {
        limit_bytes: fabric_domain::StoragePolicy::default().max_log_bytes_per_run,
        persisted_bytes: 0,
        truncated: false,
        stdout_bytes_seen: 0,
        stderr_bytes_seen: 0,
        stdout: empty.clone(),
        stderr: empty.clone(),
        events: empty,
        event_counts: fabric_domain::EventCounts::default(),
    };
    let manifest = "manifest-lifecycle".parse().unwrap();
    assert!(
        store
            .finish_run(&request.run_id, &capture, &manifest, &[], time + 2)
            .is_err()
    );
    assert_eq!(
        fixture.store().run(&request.run_id).unwrap().unwrap(),
        finalizing
    );
    store.record_capture(&request.run_id, &capture).unwrap();
    let files: Vec<_> = [
        "stdout",
        "stderr",
        "events",
        "tracked_patch",
        "untracked_archive",
        "prompt",
        "manifest",
    ]
    .into_iter()
    .map(|kind| fabric_store::Artifact {
        kind: kind.into(),
        relative_path: format!(
            "artifacts/{manifest}/{}",
            if kind == "manifest" {
                "manifest.json"
            } else {
                kind
            }
        ),
        sha256: capture.stdout.sha256.clone(),
        size_bytes: 0,
    })
    .collect();
    store
        .finish_run(&request.run_id, &capture, &manifest, &files, time + 2)
        .unwrap();
    let done = fixture.store().run(&request.run_id).unwrap().unwrap();
    assert_eq!(done.state, RunState::Failed);
    assert_eq!(done.finished_at, Some(time + 2));
    assert_eq!(done.error_kind.as_deref(), Some("harness_exit"));
    assert_eq!(done.capture, Some(capture.clone()));
    assert_eq!(done.artifact_manifest_id.as_ref(), Some(&manifest));
    assert_eq!(fixture.store().artifacts(&request.run_id).unwrap().len(), 7);
    let db = rusqlite::Connection::open(fixture.root.join("fabric.db")).unwrap();
    assert!(
        db.execute(
            "UPDATE runs SET artifact_manifest_id = NULL WHERE run_id = ?1",
            [request.run_id.as_str()]
        )
        .is_err()
    );
    assert!(
        db.execute(
            "DELETE FROM artifacts WHERE run_id = ?1",
            [request.run_id.as_str()]
        )
        .is_err()
    );
    assert!(
        store
            .finish_run(&request.run_id, &capture, &manifest, &files, time + 3)
            .is_err()
    );
    assert!(
        store
            .observe_run_exit(&request.run_id, Some(0), None, None, time + 3)
            .is_err()
    );
    assert_eq!(fixture.store().run(&request.run_id).unwrap().unwrap(), done);
}
