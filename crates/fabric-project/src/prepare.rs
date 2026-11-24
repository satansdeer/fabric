//! Private, committed-only staging. Only the final durable DB transition publishes READY.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fabric_domain::{FabricError, PreparedStateId, PreparedStateStatus};
use fabric_store::PreparedState;
use sha2::{Digest, Sha256};

use crate::recipe::Recipe;
use crate::{NEXT_ID, git_text, open_store, project_not_found, source_error, storage_error};

const SAFE_PATH: &str = "/usr/bin:/bin";
const METADATA_CAP: usize = 64 * 1024;

/// Parse first, snapshot declared environment once, then resolve current HEAD once.
/// The registration's persisted SHA is historical identity and is never rewritten.
pub fn prepare(root: &Path, name: &str, recipe_path: &Path) -> Result<PreparedState, FabricError> {
    prepare_with_admission(root, name, recipe_path, || Ok(()))
}

/// Reuse follows the same locked identity/provenance validation as ordinary preparation.
/// Admission runs only for a fresh tree, under its key lock and before durable allocation.
/// Callers coordinating root-wide eviction must acquire the root lease before calling.
pub fn prepare_with_admission(
    root: &Path,
    name: &str,
    recipe_path: &Path,
    admit_fresh: impl FnOnce() -> Result<(), FabricError>,
) -> Result<PreparedState, FabricError> {
    let recipe = Recipe::parse(&fs::read_to_string(recipe_path).map_err(storage_error)?)?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(recipe.max_duration_seconds()))
        .ok_or_else(|| source_error("Preparation duration exceeds the platform clock range."))?;
    let mut environment = BTreeMap::new();
    for name in recipe.environment() {
        match std::env::var(name) {
            Ok(value) => {
                environment.insert(name.clone(), value);
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(source_error(
                    "Declared preparation environment must be UTF-8.",
                ));
            }
        }
    }
    private_directory(root)?;
    let mut store = open_store(root, false)?;
    let root = root.canonicalize().map_err(storage_error)?;
    let project = store
        .project_by_name(name)
        .map_err(storage_error)?
        .ok_or_else(|| project_not_found(name))?;
    let source = Path::new(&project.source_path);
    let top = git_output(source, &["rev-parse", "--show-toplevel"], deadline)?;
    if Path::new(git_text(&top)?)
        .canonicalize()
        .map_err(storage_error)?
        != source
    {
        return Err(source_error(
            "Registered source is no longer the same Git working tree.",
        ));
    }
    let head = git_output(
        source,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        deadline,
    )?;
    let fingerprint = recipe.fingerprint(git_text(&head)?, &environment)?;
    let dirty = !git_output(
        source,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
        deadline,
    )?
    .is_empty();
    if dirty {
        // Warn before execution, including when preparation later fails.
        eprintln!(
            "warning: dirty Git checkout; only committed HEAD is prepared; uncommitted changes are not imported."
        );
    }
    let prepared = root.join("prepared");
    ensure_private_directory(&prepared)?;
    let key = format!("{}-{}", project.project_id, fingerprint.preparation_key);
    let container = prepared.join(&key);
    let tree = container.join("tree");
    // Keep the kernel lock through validation/publication. It releases on process death;
    // lock files remain in place so waiters can never lock different inodes for one key.
    let _lock = preparation_lock(&root, &key, deadline)?;
    if let Some(state) = store
        .prepared_state_by_key(&project.project_id, &fingerprint.preparation_key)
        .map_err(storage_error)?
    {
        if state.status == PreparedStateStatus::Preparing {
            // No live preparer owns this key. Its recipe may already have had side effects:
            // never publish or rerun an uncertain stage, even if it looks complete.
            store
                .finish_preparation(&state.state_id, PreparedStateStatus::Failed, None, 0)
                .map_err(storage_error)?;
        }
        if state.status != PreparedStateStatus::Ready {
            return Err(source_error(format!(
                "Preparation {} FAILED; staging retained at {}. This key cannot be retried safely; change the recipe, committed source commit, or declared environment inputs for a new attempt.",
                state.state_id, state.prepared_path
            )));
        }
        if Path::new(&state.prepared_path) != tree
            || state.source_commit_sha != fingerprint.source_commit_sha
            || state.recipe_digest != fingerprint.recipe_digest
            || state.harness_environment_digest.as_deref()
                != Some(fingerprint.environment_digest.as_str())
        {
            return Err(storage_error(
                "Cached preparation path or provenance does not match the requested managed state.",
            ));
        }
        verify_ready(&root, &state, deadline)?;
        return Ok(state);
    }
    admit_fresh()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(storage_error)?;
    let mut state = PreparedState {
        state_id: PreparedStateId::try_from(format!(
            "s-{}-{}-{}",
            now.as_nanos(),
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))?,
        project_id: project.project_id,
        preparation_key: fingerprint.preparation_key,
        source_commit_sha: fingerprint.source_commit_sha,
        recipe_digest: fingerprint.recipe_digest,
        harness_environment_digest: Some(fingerprint.environment_digest),
        prepared_path: tree
            .to_str()
            .ok_or_else(|| storage_error("Prepared paths must be UTF-8."))?
            .to_owned(),
        status: PreparedStateStatus::Preparing,
        created_at: i64::try_from(now.as_secs()).map_err(storage_error)?,
        verified_at: None,
        size_bytes: 0,
    };
    store.create_prepared_state(&state).map_err(storage_error)?;
    let result = (|| {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&container)
            .map_err(storage_error)?;
        private_directory(&container)?;
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(container.join(".fabric-owner"))
            .map_err(storage_error)?;
        marker
            .write_all(state.state_id.as_str().as_bytes())
            .map_err(storage_error)?;
        marker.sync_all().map_err(storage_error)?;
        File::open(&container)
            .and_then(|dir| dir.sync_all())
            .map_err(storage_error)?;
        File::open(&prepared)
            .and_then(|dir| dir.sync_all())
            .map_err(storage_error)?;
        let mut clone = git_command(source);
        clone
            .args([
                "-c",
                "protocol.file.allow=always",
                "clone",
                "--no-local",
                "--no-hardlinks",
                "--no-checkout",
                "--template=",
                "--",
            ])
            .arg(source)
            .arg(&tree);
        run(clone, deadline, false)?;
        git_output(
            &tree,
            &["checkout", "--quiet", "--detach", &state.source_commit_sha],
            deadline,
        )?;
        // Local config is private, and no developer/global/template hooks are imported.
        git_output(&tree, &["config", "core.hooksPath", "/dev/null"], deadline)?;
        // Pin the fresh committed checkout before any declared recipe can modify it.
        // Never manufacture this baseline from a reused READY tree.
        store
            .pin_checkout(&state.state_id, &content_digest(&tree, deadline)?)
            .map_err(storage_error)?;
        for step in recipe.steps() {
            validate_container(&container, &state.state_id)?;
            private_git(&tree)?;
            let argv = step.argv();
            let mut command = Command::new(&argv[0]);
            command
                .args(&argv[1..])
                .current_dir(&tree)
                .env_clear()
                .env("PATH", SAFE_PATH)
                .envs(&environment)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null");
            // Recipe output is deliberately discarded, never accumulated in memory/disk.
            run(command, deadline, false)?;
        }
        validate_container(&container, &state.state_id)?;
        private_git(&tree)?;
        let final_head = git_output(&tree, &["rev-parse", "--verify", "HEAD^{commit}"], deadline)?;
        if git_text(&final_head)? != state.source_commit_sha {
            return Err(source_error(
                "Preparation changed the pinned Git HEAD; refusing READY.",
            ));
        }
        let size = verify_tree(&tree, &tree, deadline, true)?;
        store
            .pin_prepared_content(&state.state_id, &content_digest(&tree, deadline)?)
            .map_err(storage_error)?;
        fs::set_permissions(
            container.join(".fabric-owner"),
            fs::Permissions::from_mode(0o400),
        )
        .map_err(storage_error)?;
        File::open(container.join(".fabric-owner"))
            .and_then(|file| file.sync_all())
            .map_err(storage_error)?;
        fs::set_permissions(&container, fs::Permissions::from_mode(0o500))
            .map_err(storage_error)?;
        File::open(&container)
            .and_then(|dir| dir.sync_all())
            .map_err(storage_error)?;
        File::open(&prepared)
            .and_then(|dir| dir.sync_all())
            .map_err(storage_error)?;
        check_deadline(deadline)?;
        Ok(size)
    })();
    match result {
        Ok(size) => {
            let verified_at = i64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(storage_error)?
                    .as_secs(),
            )
            .map_err(storage_error)?;
            if let Err(error) = store.finish_preparation(
                &state.state_id,
                PreparedStateStatus::Ready,
                Some(verified_at),
                size,
            ) {
                store
                    .finish_preparation(&state.state_id, PreparedStateStatus::Failed, None, 0)
                    .map_err(storage_error)?;
                return Err(storage_error(error));
            }
            state.status = PreparedStateStatus::Ready;
            state.verified_at = Some(verified_at);
            state.size_bytes = size;
            Ok(state)
        }
        Err(error) => {
            store
                .finish_preparation(&state.state_id, PreparedStateStatus::Failed, None, 0)
                .map_err(storage_error)?;
            Err(source_error(format!(
                "Preparation {} FAILED; staging retained at {}: {}",
                state.state_id,
                container.display(),
                error
            )))
        }
    }
}

/// Revalidate a persisted READY base without changing its tree or metadata.
/// This is a fork prerequisite, not workspace ownership or deletion authority.
pub fn validate_ready(root: &Path, state: &PreparedState) -> Result<(), FabricError> {
    private_directory(root)?;
    let root = root.canonicalize().map_err(storage_error)?;
    let database = fs::symlink_metadata(root.join("fabric.db")).map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions.
    if !database.is_file()
        || database.uid() != unsafe { libc::geteuid() }
        || database.nlink() != 1
        || database.mode() & 0o077 != 0
    {
        return Err(storage_error(
            "READY validation requires a private existing database.",
        ));
    }
    let store = open_store(&root, false)?;
    if store
        .prepared_state(&state.state_id)
        .map_err(storage_error)?
        .as_ref()
        != Some(state)
    {
        return Err(storage_error(
            "Prepared state does not match its persisted record.",
        ));
    }
    verify_ready(&root, state, Instant::now() + Duration::from_secs(1200))
}

fn verify_ready(root: &Path, state: &PreparedState, deadline: Instant) -> Result<(), FabricError> {
    if state.status != PreparedStateStatus::Ready || state.verified_at.is_none() {
        return Err(storage_error(
            "Forking requires a verified READY prepared state.",
        ));
    }
    if state.preparation_key.len() != 64
        || !state
            .preparation_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(storage_error(
            "READY preparation key is not a safe fingerprint.",
        ));
    }
    let prepared = root.join("prepared");
    private_directory(&prepared)?;
    let container = prepared.join(format!("{}-{}", state.project_id, state.preparation_key));
    let tree = container.join("tree");
    if Path::new(&state.prepared_path) != tree {
        return Err(storage_error(
            "READY path does not match its managed preparation.",
        ));
    }
    validate_container(&container, &state.state_id)?;
    if fs::symlink_metadata(&container)
        .map_err(storage_error)?
        .mode()
        & 0o200
        != 0
        || fs::symlink_metadata(container.join(".fabric-owner"))
            .map_err(storage_error)?
            .mode()
            & 0o200
            != 0
    {
        return Err(storage_error(
            "Cached READY ownership must remain read-only.",
        ));
    }
    if verify_tree(&tree, &tree, deadline, false)? != state.size_bytes {
        return Err(storage_error(
            "Cached READY tree size does not match its record.",
        ));
    }
    private_git(&tree)?;
    let head = git_output(&tree, &["rev-parse", "--verify", "HEAD^{commit}"], deadline)?;
    if git_text(&head)? != state.source_commit_sha {
        return Err(source_error(
            "Cached READY HEAD does not match its recorded source.",
        ));
    }
    let evidence = crate::open_store(root, false)?
        .source_evidence(&state.state_id)
        .map_err(storage_error)?
        .ok_or_else(|| source_error(
            "READY source content evidence is absent; legacy states cannot be admitted. Prepare a new key.",
        ))?;
    if content_digest(&tree, deadline)? != evidence.prepared_sha256 {
        return Err(source_error(
            "READY source content does not match its pinned preparation.",
        ));
    }
    Ok(())
}

pub struct PreparationLease(File);
impl Drop for PreparationLease {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Coordinates cache deletion with direct library preparation/reuse of the same key.
pub fn lock_prepared(root: &Path, key: &str) -> Result<PreparationLease, FabricError> {
    if key.is_empty() || key.contains('/') || key == "." || key == ".." {
        return Err(storage_error("Invalid preparation lock key"));
    }
    preparation_lock(root, key, Instant::now() + Duration::from_secs(5))
}

fn preparation_lock(
    root: &Path,
    key: &str,
    deadline: Instant,
) -> Result<PreparationLease, FabricError> {
    let directory = root.join("preparation-locks");
    ensure_private_directory(&directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(format!("{key}.lock")))
        .map_err(storage_error)?;
    let meta = file.metadata().map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.nlink() != 1
        || meta.mode() & 0o077 != 0
    {
        return Err(storage_error(
            "Preparation lock must be a private owned regular file.",
        ));
    }
    loop {
        check_deadline(deadline)?;
        // SAFETY: file owns this descriptor for the entire lock lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(PreparationLease(file));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock
            && error.kind() != std::io::ErrorKind::Interrupted
        {
            return Err(storage_error(error));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn git_command(path: &Path) -> Command {
    // macOS system Git, never a recipe-supplied executable or inherited Git overrides.
    let mut command = Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("PATH", SAFE_PATH)
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
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
        ])
        .arg("-C")
        .arg(path);
    command
}

fn git_output(path: &Path, args: &[&str], deadline: Instant) -> Result<Vec<u8>, FabricError> {
    let mut command = git_command(path);
    command.args(args);
    run(command, deadline, true)
}

struct ProcessGroup {
    child: Child,
    killed: bool,
}
impl ProcessGroup {
    fn kill(&mut self) {
        if !self.killed {
            // SAFETY: this is the private group created for our child, never a DB PID.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            self.killed = true;
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
        let _ = self.child.wait();
    }
}

fn run(mut command: Command, deadline: Instant, capture: bool) -> Result<Vec<u8>, FabricError> {
    check_deadline(deadline)?;
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command.stdout(if capture {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let child = command.spawn().map_err(|_| {
        source_error("Cannot start preparation command; check its executable and permissions.")
    })?;
    let mut group = ProcessGroup {
        child,
        killed: false,
    };
    if let Some(stdout) = &group.child.stdout {
        // SAFETY: stdout owns a valid pipe descriptor; nonblocking reads keep timeout checks live.
        let result = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        if result == -1 {
            return Err(storage_error(std::io::Error::last_os_error()));
        }
    }
    let mut output = Vec::new();
    let mut status = None;
    loop {
        check_deadline(deadline)?;
        if let Some(stdout) = &mut group.child.stdout {
            let mut buffer = [0; 4096];
            // Bound work between deadline checks, even for continuously writing children.
            for _ in 0..16 {
                match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if output.len() + count > METADATA_CAP {
                            return Err(source_error(
                                "Git metadata exceeds the 64 KiB safety limit.",
                            ));
                        }
                        output.extend_from_slice(&buffer[..count]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(storage_error(error)),
                }
            }
        }
        if let Some(status) = status {
            return if std::process::ExitStatus::success(&status) {
                Ok(output)
            } else {
                Err(source_error(format!(
                    "Preparation command failed ({status}); output discarded."
                )))
            };
        }
        status = group.child.try_wait().map_err(storage_error)?;
        if status.is_some() {
            // A successful shell must not leave background writers in a published tree.
            group.kill();
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn check_deadline(deadline: Instant) -> Result<(), FabricError> {
    if Instant::now() >= deadline {
        Err(source_error(
            "Preparation total wall-clock timeout exceeded; process group killed.",
        ))
    } else {
        Ok(())
    }
}

fn private_directory(path: &Path) -> Result<(), FabricError> {
    let meta = fs::symlink_metadata(path).map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions and returns the current effective owner.
    let uid = unsafe { libc::geteuid() };
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != uid
        || meta.mode() & 0o077 != 0
    {
        return Err(storage_error(
            "Managed directory must be real, current-user-owned and owner-only; refusing unexpected path.",
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), FabricError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(storage_error(error)),
    }
    private_directory(path)
}

fn validate_container(container: &Path, id: &PreparedStateId) -> Result<(), FabricError> {
    private_directory(container)?;
    let marker = container.join(".fabric-owner");
    let meta = fs::symlink_metadata(&marker).map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.nlink() != 1
        || meta.mode() & 0o077 != 0
        || meta.len() != id.as_str().len() as u64
        || fs::read_to_string(marker).map_err(storage_error)? != id.as_str()
    {
        return Err(storage_error(
            "Missing or unexpected Fabric ownership marker; refusing publication.",
        ));
    }
    Ok(())
}

fn private_git(tree: &Path) -> Result<(), FabricError> {
    for path in [
        tree.to_path_buf(),
        tree.join(".git"),
        tree.join(".git/objects"),
    ] {
        let meta = fs::symlink_metadata(path).map_err(storage_error)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(source_error(
                "Prepared Git metadata must be a private real directory.",
            ));
        }
    }
    for name in [
        ".git/commondir",
        ".git/objects/info/alternates",
        ".git/objects/info/http-alternates",
    ] {
        match fs::symlink_metadata(tree.join(name)) {
            Ok(_) => {
                return Err(source_error(
                    "Shared Git metadata or object alternates are forbidden.",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(storage_error(error)),
        }
    }
    Ok(())
}

fn validate_symlink(root: &Path, path: &Path, deadline: Instant) -> Result<(), FabricError> {
    let mut pending: VecDeque<_> = path
        .strip_prefix(root)
        .map_err(storage_error)?
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    let mut resolved = root.to_path_buf();
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        check_deadline(deadline)?;
        match Path::new(&component).components().next() {
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) if resolved != root => {
                resolved.pop();
            }
            Some(Component::Normal(_)) => {
                resolved.push(&component);
                let meta = fs::symlink_metadata(&resolved).map_err(storage_error)?;
                if meta.file_type().is_symlink() {
                    links += 1;
                    if links > 40 {
                        return Err(storage_error(
                            "Prepared symlink cycle or excessive link depth.",
                        ));
                    }
                    let target = fs::read_link(&resolved).map_err(storage_error)?;
                    if target.is_absolute() {
                        return Err(storage_error(
                            "Absolute prepared symlink target is forbidden.",
                        ));
                    }
                    resolved.pop();
                    for part in target.components().rev() {
                        pending.push_front(part.as_os_str().to_owned());
                    }
                } else if !pending.is_empty() && !meta.is_dir() {
                    return Err(storage_error("Prepared symlink traverses a non-directory."));
                }
            }
            _ => return Err(storage_error("Prepared symlink escapes its private tree.")),
        }
    }
    Ok(())
}

fn verify_tree(
    root: &Path,
    path: &Path,
    deadline: Instant,
    freeze: bool,
) -> Result<u64, FabricError> {
    check_deadline(deadline)?;
    let meta = fs::symlink_metadata(path).map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(storage_error(
            "Unexpected prepared-tree owner; refusing READY.",
        ));
    }
    if meta.file_type().is_symlink() && meta.nlink() == 1 {
        validate_symlink(root, path, deadline)?;
        // Never open/chmod through a link; its containing directory is synced below.
        return Ok(meta.len());
    }
    if !freeze && meta.mode() & 0o277 != 0 {
        return Err(storage_error(
            "Cached READY tree permissions are no longer immutable.",
        ));
    }
    let (size, mode) = if meta.is_dir() {
        let mut size = 0u64;
        for entry in fs::read_dir(path).map_err(storage_error)? {
            size = size
                .checked_add(verify_tree(
                    root,
                    &entry.map_err(storage_error)?.path(),
                    deadline,
                    freeze,
                )?)
                .ok_or_else(|| storage_error("Prepared size overflow."))?;
        }
        (size, 0o500)
    } else if meta.is_file() && meta.nlink() == 1 {
        (meta.len(), 0o400 | (meta.mode() & 0o100))
    } else {
        return Err(storage_error(
            "Special files and hardlinked files are forbidden in prepared trees.",
        ));
    };
    if freeze {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(storage_error)?;
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(storage_error)?;
    }
    Ok(size)
}

/// Versioned full-tree witness, including ignored recipe output and private Git metadata.
/// Paths and symlink targets are raw Unix bytes; write bits are deliberately not hashed
/// because preparation freezes them and the fork restores owner write permission.
pub fn content_digest(tree: &Path, deadline: Instant) -> Result<String, FabricError> {
    fn unchanged(a: &fs::Metadata, b: &fs::Metadata) -> bool {
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.mode() == b.mode()
            && a.uid() == b.uid()
            && a.nlink() == b.nlink()
            && a.len() == b.len()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.ctime() == b.ctime()
            && a.ctime_nsec() == b.ctime_nsec()
    }
    fn field(hash: &mut Sha256, bytes: &[u8]) {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    fn visit(
        tree: &Path,
        path: &Path,
        deadline: Instant,
        hash: &mut Sha256,
        buffer: &mut [u8; 64 * 1024],
    ) -> Result<(), FabricError> {
        check_deadline(deadline)?;
        let before = fs::symlink_metadata(path).map_err(storage_error)?;
        // SAFETY: geteuid has no preconditions.
        if before.uid() != unsafe { libc::geteuid() } {
            return Err(source_error("Source witness found an unexpected owner."));
        }
        field(
            hash,
            path.strip_prefix(tree)
                .map_err(storage_error)?
                .as_os_str()
                .as_bytes(),
        );
        if before.is_dir() {
            hash.update(b"d");
            let mut names = fs::read_dir(path)
                .map_err(storage_error)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage_error)?;
            names.sort();
            hash.update((names.len() as u64).to_be_bytes());
            for name in names {
                visit(tree, &path.join(name), deadline, hash, buffer)?;
            }
        } else if before.file_type().is_symlink() && before.nlink() == 1 {
            hash.update(b"l");
            field(
                hash,
                fs::read_link(path)
                    .map_err(storage_error)?
                    .as_os_str()
                    .as_bytes(),
            );
        } else if before.is_file() && before.nlink() == 1 {
            hash.update(b"f");
            hash.update((before.mode() & 0o100).to_be_bytes());
            hash.update(before.len().to_be_bytes());
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
                .map_err(storage_error)?;
            if !unchanged(&before, &file.metadata().map_err(storage_error)?) {
                return Err(source_error("Source changed before hashing."));
            }
            let mut remaining = before.len();
            while remaining > 0 {
                check_deadline(deadline)?;
                let size = remaining.min(buffer.len() as u64) as usize;
                let count = file.read(&mut buffer[..size]).map_err(storage_error)?;
                if count == 0 {
                    return Err(source_error("Source shortened during hashing."));
                }
                hash.update(&buffer[..count]);
                remaining -= count as u64;
            }
            if !unchanged(&before, &file.metadata().map_err(storage_error)?) {
                return Err(source_error("Source changed during hashing."));
            }
        } else {
            return Err(source_error(
                "Source witness refuses special or hardlinked files.",
            ));
        }
        if !unchanged(&before, &fs::symlink_metadata(path).map_err(storage_error)?) {
            return Err(source_error("Source changed during traversal."));
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    hash.update(b"fabric.full-tree.v1\0");
    visit(tree, tree, deadline, &mut hash, &mut [0u8; 64 * 1024])?;
    Ok(format!("{:x}", hash.finalize()))
}
