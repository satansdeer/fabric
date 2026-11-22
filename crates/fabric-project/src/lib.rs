//! Local-only Git registration and preparation. Registration keeps its original
//! committed SHA; each preparation request separately resolves current committed HEAD.

pub mod prepare;
pub mod recipe;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fabric_domain::{ErrorKind, FabricError, ProjectId};
use fabric_store::{Project, Store};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct Source {
    pub path: String,
    pub commit_sha: String,
    pub dirty: bool,
}

pub struct Registration {
    pub project: Project,
    pub dirty: bool,
}

/// Inspect only local Git metadata. Never fetch, copy files, or refresh the index.
pub fn resolve_source(path: &Path) -> Result<Source, FabricError> {
    let path = path.canonicalize().map_err(|error| {
        source_error(format!(
            "Cannot open Git working tree {}: {error}. Use --path with an existing local Git working tree.",
            path.display()
        ))
    })?;
    if !path.is_dir() {
        return Err(source_error(format!(
            "{} is not a Git working tree directory; use --path with a local repository.",
            path.display()
        )));
    }
    let top = local_git(&path, &["rev-parse", "--show-toplevel"])?;
    if !top.status.success() {
        return Err(source_error(format!(
            "{} is not an accessible Git working tree; use --path with a local clone (not a bare repository), or run git init and commit your source first.",
            path.display()
        )));
    }
    let top = git_text(&top.stdout)?;
    let top = Path::new(top).canonicalize().map_err(|error| {
        source_error(format!(
            "Cannot resolve Git working tree directory: {error}"
        ))
    })?;
    let head = local_git(&top, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if !head.status.success() {
        return Err(source_error(
            "Cannot resolve local HEAD to a commit; create an initial commit or restore the missing local commit before registering.",
        ));
    }
    let sha = git_text(&head.stdout)?;
    if ![40, 64].contains(&sha.len()) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(source_error("Git returned an invalid HEAD commit SHA."));
    }
    let status = local_git(
        &top,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
    )?;
    if !status.status.success() {
        return Err(source_error(
            "Cannot inspect Git working tree changes; check repository permissions and Git metadata before registering.",
        ));
    }
    Ok(Source {
        path: top
            .to_str()
            .ok_or_else(|| {
                source_error("Git source paths must be valid UTF-8 for durable storage.")
            })?
            .to_owned(),
        commit_sha: sha.to_owned(),
        dirty: !status.stdout.is_empty(),
    })
}

fn local_git(path: &Path, args: &[&str]) -> Result<Output, FabricError> {
    let mut command = Command::new("git");
    // Do not inherit GIT_DIR, worktree/index overrides, injected config, credentials,
    // or global config. Disable fsmonitor hooks, lazy network fetches and index writes.
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "protocol.allow=never",
        ])
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .map_err(|error| {
            source_error(format!(
                "Cannot run local Git: {error}. Install Git and make it available on PATH."
            ))
        })
}

fn git_text(bytes: &[u8]) -> Result<&str, FabricError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| source_error("Git metadata must be valid UTF-8 for durable storage."))?;
    Ok(text.strip_suffix('\n').unwrap_or(text))
}

pub fn register(root: &Path, name: &str, path: &Path) -> Result<Registration, FabricError> {
    if name.trim().is_empty() {
        return Err(FabricError {
            kind: ErrorKind::InvalidArguments,
            message: "Project name must not be empty.".to_owned(),
        });
    }
    // Validate source before creating any Fabric storage.
    let source = resolve_source(path)?;
    let store = open_store(root, true)?;
    if store
        .project_by_name(name)
        .map_err(storage_error)?
        .is_some()
    {
        return Err(storage_error(format!(
            "Project {name:?} is already registered; use project status or choose a different --name."
        )));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(storage_error)?;
    let project = Project {
        project_id: ProjectId::try_from(format!(
            "p-{}-{}-{}",
            now.as_nanos(),
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))?,
        name: name.to_owned(),
        source_path: source.path,
        default_ref: source.commit_sha,
        created_at: i64::try_from(now.as_secs()).map_err(storage_error)?,
    };
    store.create_project(&project).map_err(storage_error)?;
    Ok(Registration {
        project,
        dirty: source.dirty,
    })
}

/// Return the persisted source pin, even if the checkout has since moved or vanished.
pub fn status(root: &Path, name: &str) -> Result<Project, FabricError> {
    if !root.join("fabric.db").exists() {
        return Err(project_not_found(name));
    }
    open_store(root, false)?
        .project_by_name(name)
        .map_err(storage_error)?
        .ok_or_else(|| project_not_found(name))
}

/// Initialize the existing private local storage layout without registering a project.
pub fn initialize(root: &Path) -> Result<(), FabricError> {
    open_store(root, true).map(drop)
}

fn open_store(root: &Path, create: bool) -> Result<Store, FabricError> {
    if root.as_os_str().is_empty() {
        return Err(storage_error("Fabric storage root must not be empty."));
    }
    if create {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(root).map_err(storage_error)?;
    }
    let metadata = fs::symlink_metadata(root).map_err(storage_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(storage_error(
            "Fabric storage root must be a real directory, not a symlink.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(storage_error(format!(
                "Fabric storage root {} must have owner-only permissions (0700); choose a private --root or set its permissions to 0700.",
                root.display()
            )));
        }
    }
    let database = root.join("fabric.db");
    match fs::symlink_metadata(&database) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(storage_error(
                "Fabric database must be a regular file, not a symlink.",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(storage_error(error)),
    }
    Store::open(database).map_err(storage_error)
}

pub fn default_root() -> Result<PathBuf, FabricError> {
    if let Some(base) =
        std::env::var_os("XDG_DATA_HOME").filter(|base| Path::new(base).is_absolute())
    {
        return Ok(PathBuf::from(base).join("fabric"));
    }
    let home = std::env::var_os("HOME")
        .filter(|home| Path::new(home).is_absolute())
        .ok_or_else(|| {
            storage_error(
                "Set --root, an absolute XDG_DATA_HOME, or an absolute HOME for Fabric storage.",
            )
        })?;
    Ok(PathBuf::from(home).join(".local/share/fabric"))
}

fn project_not_found(name: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::NotFound,
        message: format!(
            "Project {name:?} is not registered in this root; use project add --name NAME --path PATH or select the correct --root."
        ),
    }
}

fn source_error(message: impl Into<String>) -> FabricError {
    FabricError {
        kind: ErrorKind::Source,
        message: message.into(),
    }
}

fn storage_error(error: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Storage,
        message: error.to_string(),
    }
}
