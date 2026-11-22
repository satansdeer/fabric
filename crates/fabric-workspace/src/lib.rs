//! Full prepared-tree forks. These staging trees are not registered workspaces:
//! no execution, lease, cleanup marker or deletion authority is provided here.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use fabric_domain::{ErrorKind, FabricError, WorkspaceId};
use fabric_store::PreparedState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkStrategy {
    /// macOS clonefile CoW; independently copy only if the entire tree is admitted.
    CloneOrCopy,
    /// Deterministic ordinary-copy path, also subject to whole-tree admission.
    Copy,
}

#[derive(Debug)]
pub struct PreparedFork {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub cloned_files: u64,
    pub copied_files: u64,
}

/// The fork prerequisite only. Run ownership and safe reclamation are separate.
pub trait WorkspaceBackend: Send + Sync {
    /// `copy_limit_bytes` explicitly admits the full logical tree size, not a
    /// per-file allowance or a physical disk quota. Zero disables copy fallback.
    /// Failed/partial forks are retained and their IDs cannot be reused.
    fn fork(
        &self,
        state: &PreparedState,
        fork_id: &WorkspaceId,
        copy_limit_bytes: u64,
    ) -> Result<PreparedFork, FabricError>;
}

/// The sole local fork layout, shared with durable ownership orchestration.
pub fn fork_path(root: &Path, fork_id: &WorkspaceId) -> PathBuf {
    root.join("fork-staging")
        .join(fork_id.as_str())
        .join("tree")
}

pub struct LocalWorkspaceBackend {
    root: PathBuf,
    strategy: ForkStrategy,
}

impl LocalWorkspaceBackend {
    pub fn new(root: &Path, strategy: ForkStrategy) -> Result<Self, FabricError> {
        private_directory(root)?;
        Ok(Self {
            root: root.canonicalize().map_err(storage_error)?,
            strategy,
        })
    }

    fn fork_tree(
        &self,
        source: &Path,
        destination: &Path,
        copy_admitted: bool,
        fork: &mut PreparedFork,
    ) -> Result<(), FabricError> {
        let meta = fs::symlink_metadata(source).map_err(storage_error)?;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(storage_error("Unexpected prepared-tree owner."));
        }
        if meta.file_type().is_symlink() && meta.nlink() == 1 {
            // validate_ready checked the complete internal chain; never follow it.
            let target = fs::read_link(source).map_err(storage_error)?;
            if target.is_absolute() {
                return Err(storage_error("Absolute prepared symlink is forbidden."));
            }
            symlink(target, destination).map_err(storage_error)?;
            add_size(fork, meta.len())?;
        } else if meta.mode() & 0o277 != 0 {
            return Err(storage_error(
                "Prepared tree must remain read-only and private.",
            ));
        } else if meta.is_dir() {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(destination)
                .map_err(storage_error)?;
            private_directory(destination)?;
            for entry in fs::read_dir(source).map_err(storage_error)? {
                let entry = entry.map_err(storage_error)?;
                self.fork_tree(
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    copy_admitted,
                    fork,
                )?;
            }
            File::open(destination)
                .and_then(|dir| dir.sync_all())
                .map_err(storage_error)?;
        } else if meta.is_file() && meta.nlink() == 1 {
            let mut input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(source)
                .map_err(storage_error)?;
            let opened = input.metadata().map_err(storage_error)?;
            if !same_file(&meta, &opened) {
                return Err(storage_error("Prepared file changed before fork."));
            }
            let cloned = self.strategy == ForkStrategy::CloneOrCopy
                && clone_regular(&input, destination).map_err(storage_error)?;
            let output = if cloned {
                fork.cloned_files += 1;
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(destination)
                    .map_err(storage_error)?
            } else {
                if !copy_admitted {
                    return Err(storage_error(
                        "Copy fallback refused: full prepared-tree size exceeds explicit copy admission.",
                    ));
                }
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(destination)
                    .map_err(storage_error)?;
                // Bound a changed/growing source rather than writing beyond admission.
                let copied = io::copy(
                    &mut (&mut input).take(meta.len().saturating_add(1)),
                    &mut output,
                )
                .map_err(storage_error)?;
                if copied != meta.len() {
                    return Err(storage_error("Prepared file size changed during fork."));
                }
                fork.copied_files += 1;
                output
            };
            let copied = output.metadata().map_err(storage_error)?;
            if !copied.is_file()
                || copied.nlink() != 1
                || copied.uid() != meta.uid()
                || copied.len() != meta.len()
                || (copied.dev(), copied.ino()) == (meta.dev(), meta.ino())
                || !same_file(&meta, &input.metadata().map_err(storage_error)?)
            {
                return Err(storage_error(
                    "Fork file is not an independent private copy.",
                ));
            }
            output
                .set_permissions(fs::Permissions::from_mode(0o600 | (meta.mode() & 0o100)))
                .map_err(storage_error)?;
            output.sync_all().map_err(storage_error)?;
            add_size(fork, copied.len())?;
        } else {
            return Err(storage_error(
                "Special files and hardlinked files cannot be forked.",
            ));
        }
        Ok(())
    }
}

impl WorkspaceBackend for LocalWorkspaceBackend {
    fn fork(
        &self,
        state: &PreparedState,
        fork_id: &WorkspaceId,
        copy_limit_bytes: u64,
    ) -> Result<PreparedFork, FabricError> {
        private_directory(&self.root)?;
        fabric_project::prepare::validate_ready(&self.root, state)?;
        let copy_admitted = state.size_bytes <= copy_limit_bytes;
        if self.strategy == ForkStrategy::Copy && !copy_admitted {
            return Err(storage_error(
                "Full prepared-tree size exceeds explicit copy admission.",
            ));
        }
        let staging = self.root.join("fork-staging");
        match fs::DirBuilder::new().mode(0o700).create(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(storage_error(error)),
        }
        private_directory(&staging)?;
        let container = staging.join(fork_id.as_str());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&container)
            .map_err(storage_error)?;
        private_directory(&container)?;
        // No .fabric-owner: an unregistered staging fork is never GC authority.
        let mut fork = PreparedFork {
            path: fork_path(&self.root, fork_id),
            size_bytes: 0,
            cloned_files: 0,
            copied_files: 0,
        };
        self.fork_tree(
            Path::new(&state.prepared_path),
            &fork_path(&self.root, fork_id),
            copy_admitted,
            &mut fork,
        )?;
        if fork.size_bytes != state.size_bytes {
            return Err(storage_error("Fork size differs from its READY record."));
        }
        private_directory(&self.root)?;
        private_directory(&staging)?;
        private_directory(&container)?;
        fabric_project::prepare::validate_ready(&self.root, state)?;
        File::open(&container)
            .and_then(|dir| dir.sync_all())
            .map_err(storage_error)?;
        File::open(&staging)
            .and_then(|dir| dir.sync_all())
            .map_err(storage_error)?;
        Ok(fork)
    }
}

fn add_size(fork: &mut PreparedFork, bytes: u64) -> Result<(), FabricError> {
    fork.size_bytes = fork
        .size_bytes
        .checked_add(bytes)
        .ok_or_else(|| storage_error("Fork size overflow."))?;
    Ok(())
}

fn same_file(expected: &fs::Metadata, actual: &fs::Metadata) -> bool {
    actual.is_file()
        && actual.nlink() == 1
        && expected.dev() == actual.dev()
        && expected.ino() == actual.ino()
        && expected.uid() == actual.uid()
        && expected.mode() == actual.mode()
        && expected.len() == actual.len()
        && expected.mtime() == actual.mtime()
        && expected.mtime_nsec() == actual.mtime_nsec()
}

fn private_directory(path: &Path) -> Result<(), FabricError> {
    let meta = fs::symlink_metadata(path).map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(storage_error(
            "Fork directory must be real, current-user-owned and owner-only.",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_regular(source: &File, destination: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let directory = File::open(destination.parent().expect("fork destination has a parent"))?;
    let name = CString::new(
        destination
            .file_name()
            .expect("fork destination has a name")
            .as_bytes(),
    )?;
    // SAFETY: both FDs are live; name is NUL-terminated. Source FD was opened
    // without following symlinks and verified regular. fclonefileat creates a
    // fresh inode, never a hardlink, and refuses an existing destination.
    if unsafe { libc::fclonefileat(source.as_raw_fd(), directory.as_raw_fd(), name.as_ptr(), 0) }
        == 0
    {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV | libc::ENOSYS | libc::EINVAL) => Ok(false),
        _ => Err(error),
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_regular(_source: &File, _destination: &Path) -> io::Result<bool> {
    Ok(false)
}

fn storage_error(error: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Storage,
        message: format!("workspace fork: {error}"),
    }
}
