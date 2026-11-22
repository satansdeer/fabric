use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fabric-prepare-{}-{}-{}",
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
    fn repo(&self) -> PathBuf {
        let path = self.0.join("repo with spaces;literal");
        fs::create_dir(&path).unwrap();
        git(&path, &["init", "--quiet"]);
        fs::write(path.join("tracked"), "committed\n").unwrap();
        fs::write(path.join(".gitignore"), ".deps/\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "--quiet", "-m", "initial"]);
        success(
            &self
                .cli()
                .args(["project", "add", "--name", "test", "--path"])
                .arg(&path)
                .output()
                .unwrap(),
        );
        path
    }
    fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fabric"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.0)
            .arg("--root")
            .arg(self.0.join("storage"));
        command
    }
    fn recipe(&self, content: &str) -> PathBuf {
        let path = self.0.join("recipe.toml");
        fs::write(&path, content).unwrap();
        path
    }
    fn prepare(&self, recipe: &Path) -> Output {
        self.cli()
            .args(["project", "prepare", "test", "--json", "--recipe"])
            .arg(recipe)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fn writable(path: &Path) {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = fs::symlink_metadata(path) {
                if meta.is_dir() {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
                    for entry in fs::read_dir(path).unwrap() {
                        writable(&entry.unwrap().path());
                    }
                }
            }
        }
        writable(&self.0);
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn git(path: &Path, args: &[&str]) -> String {
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
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    success(&out);
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn success(out: &Output) {
    assert!(out.status.success(), "{out:?}");
}
fn json(out: &Output) -> serde_json::Value {
    success(out);
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn prepare_uses_current_committed_head_private_git_and_preserves_ignored_dependencies() {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let registration = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("tracked"), "new commit\n").unwrap();
    git(&repo, &["commit", "--quiet", "-am", "second"]);
    let current = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("tracked"), "dirty\n").unwrap();
    fs::write(repo.join("untracked"), "never import").unwrap();
    fs::create_dir(repo.join(".deps")).unwrap();
    fs::write(repo.join(".deps/secret"), "never import").unwrap();
    let before = git(&repo, &["status", "--porcelain=v1"]);
    let recipe = fixture.recipe("version=1\nenvironment=['SELECTED']\n[[steps]]\nargv=['/bin/sh','-c','test \"$SELECTED\" = literal; test -z \"${UNDECLARED+x}\"; test -z \"${HOME+x}\"; mkdir .deps; printf ready > .deps/ready; printf changed > tracked']\n");
    let out = fixture
        .cli()
        .env("SELECTED", "literal")
        .env("UNDECLARED", "secret")
        .args(["project", "prepare", "test", "--json", "--recipe"])
        .arg(&recipe)
        .output()
        .unwrap();
    let report = json(&out);
    assert!(String::from_utf8_lossy(&out.stderr).contains("dirty"));
    assert_eq!(report["status"], "READY");
    assert_eq!(report["source_commit_sha"], current);
    assert!(report["size_bytes"].as_u64().unwrap() > 0);
    let tree = Path::new(report["prepared_path"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(tree.join(".deps/ready")).unwrap(),
        "ready"
    );
    assert!(!tree.join("untracked").exists());
    assert!(!tree.join(".deps/secret").exists());
    assert_eq!(git(tree, &["rev-parse", "HEAD"]), current);
    assert!(!tree.join(".git/objects/info/alternates").exists());
    assert_ne!(
        fs::metadata(tree.join(".git/HEAD")).unwrap().ino(),
        fs::metadata(repo.join(".git/HEAD")).unwrap().ino()
    );
    assert_eq!(fs::read_to_string(repo.join("tracked")).unwrap(), "dirty\n");
    assert_eq!(git(&repo, &["status", "--porcelain=v1"]), before);
    assert_eq!(
        json(
            &fixture
                .cli()
                .args(["project", "status", "test", "--json"])
                .output()
                .unwrap()
        )["source_commit_sha"],
        registration
    );
    assert_eq!(
        fs::metadata(tree.parent().unwrap()).unwrap().mode() & 0o077,
        0
    );
    assert_eq!(
        fs::metadata(tree.parent().unwrap().join(".fabric-owner"))
            .unwrap()
            .mode()
            & 0o077,
        0
    );
    fn measured(path: &Path) -> u64 {
        let meta = fs::symlink_metadata(path).unwrap();
        assert_eq!(meta.mode() & 0o277, 0);
        if meta.is_dir() {
            fs::read_dir(path)
                .unwrap()
                .map(|entry| measured(&entry.unwrap().path()))
                .sum()
        } else {
            assert_eq!(meta.nlink(), 1);
            meta.len()
        }
    }
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let state = store
        .prepared_state(&report["state_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(state.status, fabric_domain::PreparedStateStatus::Ready);
    assert!(state.verified_at.is_some());
    assert_eq!(state.size_bytes, measured(tree));
    assert_eq!(state.size_bytes, report["size_bytes"].as_u64().unwrap());
}

#[test]
fn invalid_recipe_starts_no_git_and_failure_is_retained_never_ready() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let bin = fixture.0.join("bin");
    fs::create_dir(&bin).unwrap();
    let sentinel = fixture.0.join("git-started");
    fs::write(
        bin.join("git"),
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", sentinel.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o700)).unwrap();
    let recipe = fixture.recipe("version=2\n");
    let out = fixture
        .cli()
        .env("PATH", &bin)
        .args(["project", "prepare", "test", "--json", "--recipe"])
        .arg(&recipe)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stderr).unwrap()["error"]["kind"],
        "invalid_recipe"
    );
    assert!(!sentinel.exists());
    let recipe = fixture.recipe(
        "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf retained > evidence; exit 7']\n",
    );
    let out = fixture.prepare(&recipe);
    assert!(!out.status.success());
    let states: Vec<_> = fs::read_dir(fixture.0.join("storage/prepared"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(states.len(), 1);
    assert_eq!(
        fs::read_to_string(states[0].join("tree/evidence")).unwrap(),
        "retained"
    );
    let id = fs::read_to_string(states[0].join(".fabric-owner")).unwrap();
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let state = store.prepared_state(&id.parse().unwrap()).unwrap().unwrap();
    assert_eq!(state.status, fabric_domain::PreparedStateStatus::Failed);
    assert!(state.verified_at.is_none());
    assert!(!repo.join("evidence").exists());
}

#[test]
fn total_timeout_kills_recipe_descendants_and_discards_large_output() {
    let fixture = Fixture::new();
    fixture.repo();
    let recipe = fixture.recipe("version=1\n[[steps]]\nargv=['/bin/sh','-c','(while :; do printf x >> heartbeat; sleep 0.03; done) & while :; do printf noisy; done']\n[limits]\nmax_duration_seconds=1\n");
    let start = Instant::now();
    let out = fixture.prepare(&recipe);
    assert!(!out.status.success());
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(out.stdout.len() + out.stderr.len() < 4096);
    let state = fs::read_dir(fixture.0.join("storage/prepared"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let heartbeat = state.join("tree/heartbeat");
    let length = fs::metadata(&heartbeat).unwrap().len();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(fs::metadata(heartbeat).unwrap().len(), length);
}

#[test]
fn recipe_cannot_publish_wrong_sha_external_symlinks_or_shared_git() {
    for script in [
        "git symbolic-ref HEAD refs/heads/missing",
        "ln -s /tmp outside",
        "ln -s missing dangling",
        "ln -s loop loop",
        "ln -s tracked target; ln -s target/child bad",
        "mkdir -p .git/objects/info; printf /tmp > .git/objects/info/alternates",
        "ln tracked shared",
        "printf tampered > ../.fabric-owner",
    ] {
        let fixture = Fixture::new();
        fixture.repo();
        let recipe = fixture.recipe(&format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c',{script:?}]\n"
        ));
        assert!(!fixture.prepare(&recipe).status.success());
    }
}

#[test]
fn new_commits_prepare_new_keys_without_changing_registration_identity() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let recipe = fixture.recipe(
        "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n",
    );
    let first = json(&fixture.prepare(&recipe));
    fs::write(repo.join("tracked"), "second\n").unwrap();
    git(&repo, &["commit", "--quiet", "-am", "second"]);
    let second = json(&fixture.prepare(&recipe));
    assert_ne!(first["preparation_key"], second["preparation_key"]);
    assert_ne!(first["state_id"], second["state_id"]);
    assert_eq!(
        second["source_commit_sha"],
        git(&repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        json(
            &fixture
                .cli()
                .args(["project", "status", "test", "--json"])
                .output()
                .unwrap()
        )["source_commit_sha"],
        first["source_commit_sha"]
    );
    assert_eq!(
        fs::read_to_string(Path::new(first["prepared_path"].as_str().unwrap()).join(".deps/ready"))
            .unwrap(),
        "ready"
    );
}

#[test]
fn global_templates_hooks_and_unexpected_managed_paths_are_refused_or_ignored() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let hooks = fixture.0.join("hooks");
    fs::create_dir(&hooks).unwrap();
    let sentinel = fixture.0.join("hook-ran");
    fs::write(
        hooks.join("post-checkout"),
        format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
    )
    .unwrap();
    fs::set_permissions(
        hooks.join("post-checkout"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(
        fixture.0.join(".gitconfig"),
        format!(
            "[core]\n hooksPath = {}\n[init]\n templateDir = {}\n",
            hooks.display(),
            hooks.display()
        ),
    )
    .unwrap();
    git(
        &repo,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let recipe = fixture.recipe(
        "version=1\n[[steps]]\nargv=['/bin/sh','-c','mkdir .deps; printf ready > .deps/ready']\n",
    );
    success(&fixture.prepare(&recipe));
    assert!(!sentinel.exists());

    let fixture = Fixture::new();
    fixture.repo();
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), "untouched").unwrap();
    symlink(&outside, fixture.0.join("storage/prepared")).unwrap();
    let recipe = fixture.recipe("version=1\n[[steps]]\nargv=['/usr/bin/true']\n");
    assert!(!fixture.prepare(&recipe).status.success());
    assert_eq!(
        fs::read_to_string(outside.join("sentinel")).unwrap(),
        "untouched"
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
}

#[test]
fn timeout_is_total_across_steps_and_success_cannot_leave_background_writers() {
    let fixture = Fixture::new();
    fixture.repo();
    let recipe = fixture.recipe("version=1\n[[steps]]\nargv=['/bin/sleep','0.65']\n[[steps]]\nargv=['/bin/sleep','0.65']\n[limits]\nmax_duration_seconds=1\n");
    assert!(!fixture.prepare(&recipe).status.success());

    let fixture = Fixture::new();
    fixture.repo();
    let recipe = fixture.recipe("version=1\n[[steps]]\nargv=['/bin/sh','-c','(sleep 0.2; printf bad > late) & printf ready > now']\n");
    let report = json(&fixture.prepare(&recipe));
    let tree = Path::new(report["prepared_path"].as_str().unwrap());
    std::thread::sleep(Duration::from_millis(300));
    assert!(!tree.join("late").exists());
    assert_eq!(fs::read_to_string(tree.join("now")).unwrap(), "ready");
}

#[test]
fn preparation_preserves_internal_source_and_ignored_dependency_symlinks_with_declared_path() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let repo = fixture.repo();
    symlink("tracked", repo.join("source-link")).unwrap();
    git(&repo, &["add", "source-link"]);
    git(&repo, &["commit", "--quiet", "-m", "source link"]);
    fs::write(repo.join("tracked"), "dirty source\n").unwrap();
    let before = git(&repo, &["status", "--porcelain=v1"]);
    let bin = fixture.0.join("tools");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("prepare-dependencies"),
        "#!/bin/sh\nmkdir -p .deps/bin\nprintf '#!/bin/sh\\nprintf ready\\n' > .deps/tool\nchmod 700 .deps/tool\nln -s ../tool .deps/bin/tool\nln -s bin/tool .deps/chain\n.deps/bin/tool > .deps/ready\n",
    )
    .unwrap();
    fs::set_permissions(
        bin.join("prepare-dependencies"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let recipe = fixture
        .recipe("version=1\nenvironment=['PATH']\n[[steps]]\nargv=['prepare-dependencies']\n");
    let out = fixture
        .cli()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["project", "prepare", "test", "--json", "--recipe"])
        .arg(recipe)
        .output()
        .unwrap();
    let report = json(&out);
    let tree = Path::new(report["prepared_path"].as_str().unwrap());
    assert_eq!(report["status"], "READY");
    assert_eq!(
        fs::read_link(tree.join(".deps/bin/tool")).unwrap(),
        Path::new("../tool")
    );
    assert_eq!(
        fs::read_link(tree.join("source-link")).unwrap(),
        Path::new("tracked")
    );
    assert_eq!(
        fs::read_to_string(tree.join(".deps/ready")).unwrap(),
        "ready"
    );
    assert_eq!(
        fs::read_to_string(tree.join(".deps/chain")).unwrap(),
        fs::read_to_string(tree.join(".deps/tool")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(tree.join("source-link")).unwrap(),
        "committed\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("tracked")).unwrap(),
        "dirty source\n"
    );
    assert_eq!(git(&repo, &["status", "--porcelain=v1"]), before);
    assert!(!repo.join(".deps").exists());
    fn measured(path: &Path) -> u64 {
        let meta = fs::symlink_metadata(path).unwrap();
        if meta.is_dir() {
            fs::read_dir(path)
                .unwrap()
                .map(|entry| measured(&entry.unwrap().path()))
                .sum()
        } else {
            meta.len()
        }
    }
    let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
    let state = store
        .prepared_state(&report["state_id"].as_str().unwrap().parse().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(state.status, fabric_domain::PreparedStateStatus::Ready);
    assert_eq!(state.size_bytes, measured(tree));
}

#[test]
fn escaping_symlinks_fail_without_mutating_external_target() {
    use std::os::unix::fs::MetadataExt;
    for relative in [false, true] {
        let fixture = Fixture::new();
        fixture.repo();
        let outside = fixture.0.join("outside");
        fs::write(&outside, "external evidence").unwrap();
        let before = fs::symlink_metadata(&outside).unwrap();
        let target = if relative {
            "../../../../outside"
        } else {
            outside.to_str().unwrap()
        };
        let script = format!("ln -s '{target}' escape");
        let recipe = fixture.recipe(&format!(
            "version=1\n[[steps]]\nargv=['/bin/sh','-c',{script:?}]\n"
        ));
        assert!(!fixture.prepare(&recipe).status.success());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "external evidence");
        let after = fs::symlink_metadata(&outside).unwrap();
        assert_eq!(after.mode(), before.mode());
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.nlink(), before.nlink());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        let container = fs::read_dir(fixture.0.join("storage/prepared"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let id = fs::read_to_string(container.join(".fabric-owner")).unwrap();
        let store = fabric_store::Store::open(fixture.0.join("storage/fabric.db")).unwrap();
        let state = store.prepared_state(&id.parse().unwrap()).unwrap().unwrap();
        assert_eq!(state.status, fabric_domain::PreparedStateStatus::Failed);
        assert!(state.verified_at.is_none());
    }
}
