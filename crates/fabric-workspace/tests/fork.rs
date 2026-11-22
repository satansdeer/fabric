use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use fabric_domain::{PreparedStateStatus, WorkspaceId};
use fabric_store::{PreparedState, Store};
use fabric_workspace::{ForkStrategy, LocalWorkspaceBackend, WorkspaceBackend};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    path: PathBuf,
    root: PathBuf,
    state: PreparedState,
}
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fabric-fork-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let repo = path.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        fs::write(repo.join("tracked"), "base\n").unwrap();
        fs::write(repo.join(".gitignore"), ".deps/\n").unwrap();
        fs::write(repo.join("tool"), "#!/bin/sh\nprintf executable").unwrap();
        fs::set_permissions(repo.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("tracked", repo.join("source-link")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "initial"]);
        let root = path.join("storage");
        fabric_project::register(&root, "test", &repo).unwrap();
        let recipe = path.join("recipe.toml");
        fs::write(&recipe, format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf x >> \"{}\"; mkdir -p .deps/bin; printf ready > .deps/ready; cp tool .deps/tool; ln -s ../tool .deps/bin/tool; ln -s .deps/bin alias']\n",
            path.join("setup-count").display()
        )).unwrap();
        let state = fabric_project::prepare::prepare(&root, "test", &recipe).unwrap();
        Self { path, root, state }
    }
    fn backend(&self, strategy: ForkStrategy) -> LocalWorkspaceBackend {
        LocalWorkspaceBackend::new(&self.root, strategy).unwrap()
    }
    fn id(value: &str) -> WorkspaceId {
        WorkspaceId::try_from(value.to_owned()).unwrap()
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
        writable(&self.path);
        fs::remove_dir_all(&self.path).unwrap();
    }
}
fn git(path: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn assert_tree(source: &Path, destination: &Path) {
    let original = fs::symlink_metadata(source).unwrap();
    let copied = fs::symlink_metadata(destination).unwrap();
    assert_eq!(original.file_type(), copied.file_type());
    if original.file_type().is_symlink() {
        assert_eq!(
            fs::read_link(source).unwrap(),
            fs::read_link(destination).unwrap()
        );
    } else if original.is_dir() {
        assert_eq!(copied.mode() & 0o777, 0o700);
        let entries: Vec<_> = fs::read_dir(source)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), fs::read_dir(destination).unwrap().count());
        for name in entries {
            assert_tree(&source.join(&name), &destination.join(&name));
        }
    } else {
        assert_eq!(fs::read(source).unwrap(), fs::read(destination).unwrap());
        assert_ne!(
            (original.dev(), original.ino()),
            (copied.dev(), copied.ino())
        );
        assert_eq!(copied.nlink(), 1);
        assert_eq!(copied.mode() & 0o777, 0o600 | (original.mode() & 0o100));
        assert_eq!(original.mode() & 0o277, 0);
    }
}

#[test]
fn admitted_copy_preserves_complete_prepared_tree_without_setup_and_isolates_siblings() {
    let fixture = Fixture::new();
    let backend = fixture.backend(ForkStrategy::Copy);
    let a = backend
        .fork(&fixture.state, &Fixture::id("a"), fixture.state.size_bytes)
        .unwrap();
    let b = backend
        .fork(&fixture.state, &Fixture::id("b"), fixture.state.size_bytes)
        .unwrap();
    let base = Path::new(&fixture.state.prepared_path);
    assert_eq!(a.size_bytes, fixture.state.size_bytes);
    assert_eq!(a.cloned_files, 0);
    assert!(a.copied_files > 0);
    assert_eq!(
        fs::read_to_string(fixture.path.join("setup-count")).unwrap(),
        "x"
    );
    for fork in [&a, &b] {
        assert_tree(base, &fork.path);
        assert_eq!(fs::read(fork.path.join(".deps/ready")).unwrap(), b"ready");
        assert_eq!(
            git(&fork.path, &["rev-parse", "HEAD"]),
            fixture.state.source_commit_sha
        );
        assert_eq!(
            git(&fork.path, &["status", "--porcelain"]),
            git(base, &["status", "--porcelain"])
        );
        assert_eq!(
            fs::read_link(fork.path.join("source-link")).unwrap(),
            Path::new("tracked")
        );
        assert_eq!(
            fs::read_link(fork.path.join(".deps/bin/tool")).unwrap(),
            Path::new("../tool")
        );
        assert_eq!(
            Command::new(fork.path.join("alias/tool"))
                .output()
                .unwrap()
                .stdout,
            b"executable"
        );
        assert!(!fork.path.parent().unwrap().join(".fabric-owner").exists());
    }
    assert_ne!(
        fs::metadata(a.path.join("tracked")).unwrap().ino(),
        fs::metadata(b.path.join("tracked")).unwrap().ino()
    );
    std::thread::scope(|scope| {
        scope.spawn(|| fs::write(a.path.join("source-link"), "a\n").unwrap());
        scope.spawn(|| fs::write(b.path.join("tracked"), "b\n").unwrap());
    });
    fs::write(a.path.join(".deps/ready"), "changed").unwrap();
    fs::write(a.path.join(".git/HEAD"), "private edit").unwrap();
    assert_eq!(fs::read(base.join("tracked")).unwrap(), b"base\n");
    assert_eq!(fs::read(a.path.join("tracked")).unwrap(), b"a\n");
    assert_eq!(fs::read(b.path.join("tracked")).unwrap(), b"b\n");
    assert_eq!(fs::read(base.join(".deps/ready")).unwrap(), b"ready");
    assert_eq!(fs::read(b.path.join(".deps/ready")).unwrap(), b"ready");
    assert_eq!(
        git(base, &["rev-parse", "HEAD"]),
        fixture.state.source_commit_sha
    );
    assert_eq!(
        git(&b.path, &["rev-parse", "HEAD"]),
        fixture.state.source_commit_sha
    );
    let store = Store::open(fixture.root.join("fabric.db")).unwrap();
    assert_eq!(
        store
            .prepared_state(&fixture.state.state_id)
            .unwrap()
            .unwrap(),
        fixture.state
    );
}

#[test]
fn copy_admission_precedes_destination_and_existing_forks_are_never_overwritten() {
    let fixture = Fixture::new();
    let backend = fixture.backend(ForkStrategy::Copy);
    let id = Fixture::id("limited");
    let error = backend
        .fork(&fixture.state, &id, fixture.state.size_bytes - 1)
        .unwrap_err();
    assert!(error.message.contains("admission"), "{error}");
    assert!(!fixture.root.join("fork-staging/limited").exists());
    let fork = backend
        .fork(&fixture.state, &id, fixture.state.size_bytes)
        .unwrap();
    fs::write(fork.path.join("sentinel"), "keep").unwrap();
    assert!(
        backend
            .fork(&fixture.state, &id, fixture.state.size_bytes)
            .is_err()
    );
    assert_eq!(fs::read(fork.path.join("sentinel")).unwrap(), b"keep");
}

#[test]
fn refuses_unready_or_forged_state_and_destination_symlink_or_permissions() {
    let fixture = Fixture::new();
    let backend = fixture.backend(ForkStrategy::Copy);
    let mut state = fixture.state.clone();
    state.status = PreparedStateStatus::Preparing;
    assert!(
        backend
            .fork(&state, &Fixture::id("unready"), u64::MAX)
            .is_err()
    );
    state = fixture.state.clone();
    state.prepared_path = fixture.path.to_str().unwrap().to_owned();
    assert!(
        backend
            .fork(&state, &Fixture::id("outside"), u64::MAX)
            .is_err()
    );
    state = fixture.state.clone();
    state.size_bytes = 0;
    assert!(
        backend
            .fork(&state, &Fixture::id("forged"), u64::MAX)
            .is_err()
    );
    let outside = fixture.path.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), "keep").unwrap();
    symlink(&outside, fixture.root.join("fork-staging")).unwrap();
    assert!(
        backend
            .fork(&fixture.state, &Fixture::id("symlink"), u64::MAX)
            .is_err()
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    fs::remove_file(fixture.root.join("fork-staging")).unwrap();
    fs::create_dir(fixture.root.join("fork-staging")).unwrap();
    fs::set_permissions(
        fixture.root.join("fork-staging"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(
        backend
            .fork(&fixture.state, &Fixture::id("permissive"), u64::MAX)
            .is_err()
    );
}

#[test]
fn tampered_base_links_special_files_and_markers_are_refused_without_following() {
    let fixture = Fixture::new();
    let backend = fixture.backend(ForkStrategy::Copy);
    let base = Path::new(&fixture.state.prepared_path);
    let outside = fixture.path.join("sentinel");
    fs::write(&outside, "untouched").unwrap();
    fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    fs::set_permissions(base, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        backend
            .fork(&fixture.state, &Fixture::id("absolute"), u64::MAX)
            .is_err()
    );
    fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(base.join("escape")).unwrap();
    symlink("../../../sentinel", base.join("escape")).unwrap();
    fs::set_permissions(base, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        backend
            .fork(&fixture.state, &Fixture::id("relative"), u64::MAX)
            .is_err()
    );
    fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(base.join("escape")).unwrap();
    let output = Command::new("/usr/bin/mkfifo")
        .arg(base.join("fifo"))
        .output()
        .unwrap();
    assert!(output.status.success());
    fs::set_permissions(base, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        backend
            .fork(&fixture.state, &Fixture::id("special"), u64::MAX)
            .is_err()
    );
    fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(base.join("fifo")).unwrap();
    fs::hard_link(base.join("tracked"), base.join("hardlink")).unwrap();
    fs::set_permissions(base, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        backend
            .fork(&fixture.state, &Fixture::id("hardlinked"), u64::MAX)
            .is_err()
    );
    fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(base.join("hardlink")).unwrap();
    fs::set_permissions(base, fs::Permissions::from_mode(0o500)).unwrap();
    let marker = base.parent().unwrap().join(".fabric-owner");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&marker, "wrong").unwrap();
    assert!(
        backend
            .fork(&fixture.state, &Fixture::id("marker"), u64::MAX)
            .is_err()
    );
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    assert_eq!(fs::metadata(&outside).unwrap().mode() & 0o200, 0o200);
    assert!(!fixture.root.join("fork-staging").exists());
}

#[test]
fn automatic_strategy_preserves_independence_and_reports_actual_file_strategy() {
    let fixture = Fixture::new();
    let fork = fixture
        .backend(ForkStrategy::CloneOrCopy)
        .fork(
            &fixture.state,
            &Fixture::id("automatic"),
            fixture.state.size_bytes,
        )
        .unwrap();
    assert!(fork.cloned_files + fork.copied_files > 0);
    let base = Path::new(&fixture.state.prepared_path);
    assert_ne!(
        fs::metadata(base.join("tracked")).unwrap().ino(),
        fs::metadata(fork.path.join("tracked")).unwrap().ino()
    );
    fs::write(fork.path.join("tracked"), "private").unwrap();
    assert_eq!(fs::read(base.join("tracked")).unwrap(), b"base\n");
    assert_eq!(fs::read(fork.path.join(".deps/ready")).unwrap(), b"ready");
}

#[test]
fn automatic_strategy_never_copies_without_whole_tree_admission() {
    let fixture = Fixture::new();
    let result =
        fixture
            .backend(ForkStrategy::CloneOrCopy)
            .fork(&fixture.state, &Fixture::id("no-copy"), 0);
    match result {
        Ok(fork) => {
            assert_eq!(fork.copied_files, 0);
            assert!(fork.cloned_files > 0);
            assert_tree(Path::new(&fixture.state.prepared_path), &fork.path);
        }
        Err(error) => assert!(error.message.contains("admission"), "{error}"),
    }
}
