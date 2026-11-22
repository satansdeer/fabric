use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fabric-project-{}-{}-{}",
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
        let path = self.0.join("-repo with spaces;$(touch NEVER)");
        fs::create_dir(&path).unwrap();
        git(&path, &["init", "--quiet"]);
        fs::write(path.join("tracked"), "committed\n").unwrap();
        fs::write(path.join(".gitignore"), "ignored\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "--quiet", "-m", "initial"]);
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

    fn add(&self, repo: &Path) -> Output {
        self.cli()
            .args(["project", "add", "--name", "my project", "--path"])
            .arg(repo)
            .output()
            .unwrap()
    }

    fn status(&self) -> Output {
        self.cli()
            .args(["project", "status", "my project", "--json"])
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fabric Test")
        .env("GIT_AUTHOR_EMAIL", "fabric@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fabric Test")
        .env("GIT_COMMITTER_EMAIL", "fabric@example.invalid")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn registration_persists_canonical_source_and_pinned_sha_across_head_changes() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let sha = git(&repo, &["rev-parse", "HEAD"]);
    let subdir = repo.join("subdir");
    fs::create_dir(&subdir).unwrap();
    let added = fixture.add(&subdir);
    success(&added);
    assert!(added.stderr.is_empty(), "{added:?}");
    let before = fixture.status();
    success(&before);
    assert!(before.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&before.stdout).unwrap();
    assert_eq!(value["name"], "my project");
    assert_eq!(value["source_commit_sha"], sha);
    assert_eq!(value["default_ref"], sha);
    assert_eq!(
        value["source_path"],
        repo.canonicalize().unwrap().to_str().unwrap()
    );
    fs::write(repo.join("tracked"), "next commit\n").unwrap();
    git(&repo, &["commit", "--quiet", "-am", "advance"]);
    assert_ne!(git(&repo, &["rev-parse", "HEAD"]), sha);
    let after = fixture.status();
    success(&after);
    assert_eq!(before.stdout, after.stdout);
    git(&repo, &["cat-file", "-e", &format!("{sha}^{{commit}}")]);
    let human = fixture
        .cli()
        .args(["project", "status", "my project"])
        .output()
        .unwrap();
    success(&human);
    assert!(String::from_utf8(human.stdout).unwrap().contains(&sha));
    let duplicate = fixture.add(&repo);
    assert!(!duplicate.status.success());
    assert!(
        String::from_utf8(duplicate.stderr)
            .unwrap()
            .contains("already registered")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(fixture.0.join("storage"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    fs::rename(&repo, fixture.0.join("moved-source")).unwrap();
    let missing_source = fixture.status();
    success(&missing_source);
    assert_eq!(before.stdout, missing_source.stdout);
}

#[test]
fn tracked_staged_and_untracked_edits_warn_without_importing_them() {
    for change in ["tracked", "staged", "untracked"] {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        let sha = git(&repo, &["rev-parse", "HEAD"]);
        match change {
            "tracked" => fs::write(repo.join("tracked"), "private edit\n").unwrap(),
            "staged" => {
                fs::write(repo.join("tracked"), "staged private edit\n").unwrap();
                git(&repo, &["add", "tracked"]);
            }
            _ => fs::write(repo.join("private-untracked"), "private data\n").unwrap(),
        }
        let original = git(&repo, &["status", "--porcelain=v1"]);
        let output = fixture.add(&repo);
        success(&output);
        let warning = String::from_utf8(output.stderr).unwrap();
        assert!(warning.contains("warning"), "{warning}");
        assert!(warning.contains("only committed HEAD"), "{warning}");
        assert!(
            warning.contains("uncommitted changes are not imported"),
            "{warning}"
        );
        let status = fixture.status();
        success(&status);
        let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(value["source_commit_sha"], sha);
        assert_eq!(git(&repo, &["status", "--porcelain=v1"]), original);
        assert_eq!(
            git(&repo, &["show", &format!("{sha}:tracked")]),
            "committed"
        );
        for entry in fs::read_dir(fixture.0.join("storage")).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_name().to_str().unwrap().starts_with("fabric.db"));
            assert!(entry.file_type().unwrap().is_file());
        }
    }
}

#[test]
fn ignored_files_and_detached_head_are_supported() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    git(&repo, &["checkout", "--quiet", "--detach"]);
    fs::write(repo.join("ignored"), "ignored private data").unwrap();
    let output = fixture.add(&repo);
    success(&output);
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn invalid_sources_and_unknown_projects_have_actionable_json_errors() {
    let fixture = Fixture::new();
    let plain = fixture.0.join("plain");
    fs::create_dir(&plain).unwrap();
    for path in [&plain, &fixture.0.join("missing")] {
        let output = fixture
            .cli()
            .args(["project", "add", "--name", "bad", "--json", "--path"])
            .arg(path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "source");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Git working tree")
        );
    }
    let bare = fixture.0.join("bare.git");
    fs::create_dir(&bare).unwrap();
    git(&bare, &["init", "--bare", "--quiet"]);
    let bare_result = fixture.add(&bare);
    assert!(!bare_result.status.success());
    assert!(
        String::from_utf8(bare_result.stderr)
            .unwrap()
            .contains("working tree")
    );
    assert!(!fixture.0.join("storage").exists());
    git(&plain, &["init", "--quiet"]);
    let unborn = fixture.add(&plain);
    assert!(!unborn.status.success());
    assert!(String::from_utf8(unborn.stderr).unwrap().contains("commit"));
    let unknown = fixture.status();
    assert!(!unknown.status.success());
    let error: serde_json::Value = serde_json::from_slice(&unknown.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "not_found");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("project add")
    );
}

#[test]
fn git_environment_and_fsmonitor_cannot_redirect_or_execute_discovery() {
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let sha = git(&repo, &["rev-parse", "HEAD"]);
    let marker = fixture.0.join("fsmonitor-ran");
    git(
        &repo,
        &[
            "config",
            "core.fsmonitor",
            &format!("touch {}", marker.display()),
        ],
    );
    let output = fixture
        .cli()
        .env("GIT_DIR", fixture.0.join("missing.git"))
        .env("GIT_WORK_TREE", &fixture.0)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_0", "touch SHOULD_NOT_RUN")
        .args(["project", "add", "--name", "my project", "--path"])
        .arg(&repo)
        .output()
        .unwrap();
    success(&output);
    assert!(!marker.exists());
    let value: serde_json::Value = serde_json::from_slice(&fixture.status().stdout).unwrap();
    assert_eq!(value["source_commit_sha"], sha);
}

#[test]
fn default_roots_use_xdg_then_home_and_doctor_never_creates_storage() {
    for xdg in [true, false] {
        let fixture = Fixture::new();
        let repo = fixture.repo();
        let mut command = Command::new(env!("CARGO_BIN_EXE_fabric"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &fixture.0);
        let root = if xdg {
            command.env("XDG_DATA_HOME", fixture.0.join("xdg"));
            fixture.0.join("xdg/fabric")
        } else {
            fixture.0.join(".local/share/fabric")
        };
        command
            .args(["project", "add", "--name", "default", "--path"])
            .arg(&repo);
        success(&command.output().unwrap());
        assert!(root.join("fabric.db").is_file());
        let doctor = fixture.cli().args(["doctor", "--json"]).output().unwrap();
        success(&doctor);
        assert!(!fixture.0.join("storage").exists());
    }
}

#[cfg(unix)]
#[test]
fn unsafe_existing_roots_are_refused_without_changing_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let repo = fixture.repo();
    let root = fixture.0.join("storage");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    let output = fixture.add(&repo);
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("0700"));
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!root.join("fabric.db").exists());
    fs::remove_dir(&root).unwrap();
    symlink(&repo, &root).unwrap();
    let output = fixture.add(&repo);
    assert!(!output.status.success());
    assert!(!repo.join("fabric.db").exists());
}
