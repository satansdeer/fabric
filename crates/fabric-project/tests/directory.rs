use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use fabric_project::prepare::{prepare, prepare_with_admission, validate_ready};
use fabric_project::recipe::Recipe;
use fabric_project::{is_directory_source, register, resolve_source};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    source: PathBuf,
    root: PathBuf,
    recipe: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "fabric-directory-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let source = base.join("source");
        fs::create_dir(&source).unwrap();
        let recipe = base.join("recipe.toml");
        fs::write(
            &recipe,
            "version = 1\n[[steps]]\nargv = [\"/usr/bin/true\"]\n",
        )
        .unwrap();
        Self {
            root: base.join("storage"),
            source,
            recipe,
            base,
        }
    }
    fn register(&self) {
        register(&self.root, "directory", &self.source).unwrap();
    }
    fn prepare(&self) -> fabric_store::PreparedState {
        prepare(&self.root, "directory", &self.recipe).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fn writable(path: &Path) {
            if let Ok(meta) = fs::symlink_metadata(path) {
                if meta.is_dir() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
                    if let Ok(entries) = fs::read_dir(path) {
                        for entry in entries.flatten() {
                            writable(&entry.path());
                        }
                    }
                }
            }
        }
        writable(&self.base);
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn empty_directory_registers_prepares_and_reuses_without_git() {
    let f = Fixture::new();
    let identity = resolve_source(&f.source).unwrap();
    assert!(is_directory_source(&identity.commit_sha));
    let registered = register(&f.root, "directory", &f.source).unwrap();
    assert_eq!(registered.project.default_ref, identity.commit_sha);
    let state = f.prepare();
    assert_eq!(state.source_commit_sha, identity.commit_sha);
    assert_eq!(state.size_bytes, 0);
    assert_eq!(state, f.prepare());
    validate_ready(&f.root, &state).unwrap();
    assert!(fs::read_dir(&f.source).unwrap().next().is_none());
    assert!(fs::read_dir(&state.prepared_path).unwrap().next().is_none());
    assert!(!f.source.join(".git").exists());
}

#[test]
fn complete_content_modes_empty_directories_and_safe_links_are_snapshotted() {
    let f = Fixture::new();
    fs::write(f.source.join(".gitignore"), "ignored\n").unwrap();
    fs::write(f.source.join("ignored"), "included").unwrap();
    fs::write(f.source.join("executable"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(
        f.source.join("executable"),
        fs::Permissions::from_mode(0o751),
    )
    .unwrap();
    fs::create_dir(f.source.join("empty")).unwrap();
    fs::create_dir(f.source.join("truly-empty")).unwrap();
    symlink("../ignored", f.source.join("empty/link")).unwrap();
    let identity = resolve_source(&f.source).unwrap().commit_sha;
    let equivalent = f.base.join("equivalent");
    fs::create_dir(&equivalent).unwrap();
    fs::set_permissions(&equivalent, fs::metadata(&f.source).unwrap().permissions()).unwrap();
    for name in [".gitignore", "ignored", "executable"] {
        fs::copy(f.source.join(name), equivalent.join(name)).unwrap();
    }
    fs::create_dir(equivalent.join("empty")).unwrap();
    fs::set_permissions(
        equivalent.join("empty"),
        fs::metadata(f.source.join("empty")).unwrap().permissions(),
    )
    .unwrap();
    symlink("../ignored", equivalent.join("empty/link")).unwrap();
    fs::create_dir(equivalent.join("truly-empty")).unwrap();
    fs::set_permissions(
        equivalent.join("truly-empty"),
        fs::metadata(f.source.join("truly-empty"))
            .unwrap()
            .permissions(),
    )
    .unwrap();
    assert_eq!(resolve_source(&equivalent).unwrap().commit_sha, identity);
    f.register();
    // The recipe observes exact source permissions before READY seals write access.
    fs::write(&f.recipe, "version = 1\n[[steps]]\nargv = [\"/bin/sh\", \"-c\", \"test -x executable && test -d empty && test -L empty/link && test ! -e .git\"]\n").unwrap();
    let first = f.prepare();
    let tree = Path::new(&first.prepared_path);
    assert_eq!(
        fs::read_to_string(tree.join("ignored")).unwrap(),
        "included"
    );
    assert_eq!(
        fs::read_link(tree.join("empty/link")).unwrap(),
        Path::new("../ignored")
    );
    assert!(
        fs::read_dir(tree.join("truly-empty"))
            .unwrap()
            .next()
            .is_none()
    );
    assert_ne!(
        fs::metadata(tree.join("ignored")).unwrap().ino(),
        fs::metadata(f.source.join("ignored")).unwrap().ino()
    );
    assert_eq!(
        fs::metadata(f.source.join("executable")).unwrap().mode() & 0o777,
        0o751
    );
    assert_eq!(resolve_source(&f.source).unwrap().commit_sha, identity);
    fs::set_permissions(
        f.source.join("executable"),
        fs::Permissions::from_mode(0o740),
    )
    .unwrap();
    assert_ne!(resolve_source(&f.source).unwrap().commit_sha, identity);
    let mode_identity = resolve_source(&f.source).unwrap().commit_sha;
    fs::write(f.source.join("ignored"), "changed").unwrap();
    assert_ne!(resolve_source(&f.source).unwrap().commit_sha, mode_identity);
    let second = f.prepare();
    assert_ne!(second.preparation_key, first.preparation_key);
    assert_eq!(
        fs::read_to_string(tree.join("ignored")).unwrap(),
        "included"
    );
    validate_ready(&f.root, &first).unwrap();
}

#[test]
fn changed_bytes_and_permissions_cannot_be_reused_as_ready() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), "before").unwrap();
    f.register();
    let state = f.prepare();
    let file = Path::new(&state.prepared_path).join("file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(validate_ready(&f.root, &state).is_err());
    fs::write(&file, "after!").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).unwrap();
    assert!(validate_ready(&f.root, &state).is_err());
    assert!(prepare(&f.root, "directory", &f.recipe).is_err());
}

#[test]
fn unsafe_links_hardlinks_special_files_and_recursive_storage_are_rejected() {
    let f = Fixture::new();
    let outside = f.base.join("outside");
    fs::write(&outside, "private ambient bytes").unwrap();
    for target in [
        outside.as_path(),
        Path::new("../outside"),
        Path::new("missing"),
        Path::new("escape"),
    ] {
        symlink(target, f.source.join("escape")).unwrap();
        assert!(resolve_source(&f.source).is_err());
        fs::remove_file(f.source.join("escape")).unwrap();
    }
    fs::hard_link(&outside, f.source.join("hardlink")).unwrap();
    assert!(resolve_source(&f.source).is_err());
    fs::remove_file(f.source.join("hardlink")).unwrap();
    let fifo = f.source.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a valid NUL-terminated path; no FIFO reader is opened.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(resolve_source(&f.source).is_err());
    fs::remove_file(fifo).unwrap();
    assert!(register(&f.source.join("fabric"), "recursive", &f.source).is_err());
    assert!(!f.source.join("fabric").exists());
    assert!(register(&f.source, "recursive", &f.source).is_err());
    assert_eq!(
        fs::read_to_string(outside).unwrap(),
        "private ambient bytes"
    );
}

#[test]
fn admission_mutation_and_source_kind_drift_fail_closed() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), "original").unwrap();
    f.register();
    assert!(
        prepare_with_admission(&f.root, "directory", &f.recipe, || {
            fs::write(f.source.join("file"), "changed").unwrap();
            Ok(())
        })
        .is_err()
    );
    fs::create_dir(f.source.join(".git")).unwrap();
    assert!(prepare(&f.root, "directory", &f.recipe).is_err());
}

#[test]
fn tagged_identities_are_not_git_commits_and_must_be_canonical() {
    let recipe = Recipe::parse("version = 1\n[[steps]]\nargv = [\"true\"]").unwrap();
    let identity = format!("directory-sha256:{}", "a".repeat(64));
    assert_eq!(
        recipe
            .fingerprint(&identity, &Default::default())
            .unwrap()
            .source_commit_sha,
        identity
    );
    for bad in [
        format!("directory-sha256:{}", "A".repeat(64)),
        format!("directory-sha256:{}", "a".repeat(63)),
        "directory-sha256:HEAD".to_owned(),
    ] {
        assert!(!is_directory_source(&bad));
        assert!(recipe.fingerprint(&bad, &Default::default()).is_err());
    }
    assert!(!is_directory_source(&"a".repeat(64)));
    assert!(
        recipe
            .fingerprint(&"a".repeat(64), &Default::default())
            .is_ok()
    );
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

#[test]
fn git_sources_still_require_commits_and_prepare_committed_content_only() {
    let f = Fixture::new();
    git(&f.source, &["init", "--quiet"]);
    assert!(resolve_source(&f.source).is_err());
    fs::write(f.source.join("tracked"), "committed").unwrap();
    git(&f.source, &["add", "tracked"]);
    git(
        &f.source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );
    let commit = git(&f.source, &["rev-parse", "HEAD"]);
    f.register();
    fs::write(f.source.join("tracked"), "dirty").unwrap();
    fs::write(f.source.join("untracked"), "not imported").unwrap();
    let state = f.prepare();
    assert_eq!(state.source_commit_sha, commit);
    assert_eq!(
        fs::read_to_string(Path::new(&state.prepared_path).join("tracked")).unwrap(),
        "committed"
    );
    assert!(!Path::new(&state.prepared_path).join("untracked").exists());
    validate_ready(&f.root, &state).unwrap();
    fs::remove_dir_all(f.source.join(".git")).unwrap();
    assert!(prepare(&f.root, "directory", &f.recipe).is_err());
    fs::create_dir(f.source.join(".git")).unwrap();
    assert!(resolve_source(&f.source).is_err());
    fs::remove_dir(f.source.join(".git")).unwrap();
    git(&f.source, &["init", "--bare", "--quiet"]);
    assert!(resolve_source(&f.source).is_err());
}

#[test]
fn source_root_validation_precedes_initialization_and_preparation_allocation() {
    let f = Fixture::new();
    let source = resolve_source(&f.source).unwrap();
    let nested = f.source.join("storage");
    assert!(fabric_project::validate_source_root(&nested, &source).is_err());
    assert!(!nested.exists());
    assert!(fabric_project::validate_source_root(&f.root, &source).is_ok());
    f.register();
    // Existing storage can be relocated by the user; it must not become part of a snapshot.
    fs::rename(&f.root, &nested).unwrap();
    assert!(prepare(&nested, "directory", &f.recipe).is_err());
    assert!(!nested.join("prepared").exists());
    assert!(!nested.join("preparation-locks").exists());
    assert_eq!(fs::read_dir(&f.source).unwrap().count(), 1);
}

#[test]
fn symlink_validation_is_anchored_to_the_open_root_not_a_replaced_path() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), "selected").unwrap();
    fs::create_dir(f.source.join("sub")).unwrap();
    symlink("../file", f.source.join("sub/link")).unwrap();
    symlink("sub/link", f.source.join("chain")).unwrap();
    let root = fs::File::open(&f.source).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    fabric_project::prepare::validate_symlink_at(&root, Path::new("chain"), deadline).unwrap();
    assert!(
        fabric_project::prepare::validate_symlink_at(&root, Path::new("../outside"), deadline)
            .is_err()
    );
    fs::rename(&f.source, f.base.join("moved-source")).unwrap();
    let outside = f.base.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file"), "ambient").unwrap();
    symlink(&outside, &f.source).unwrap();
    fabric_project::prepare::validate_symlink_at(&root, Path::new("chain"), deadline).unwrap();
    assert!(
        fabric_project::prepare::validate_symlink(&f.source, &f.source.join("file"), deadline)
            .is_err()
    );
    assert_eq!(fs::read_to_string(outside.join("file")).unwrap(), "ambient");
}
