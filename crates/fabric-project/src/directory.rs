//! Descriptor-relative local snapshots: never follow source symlinks or share inodes.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{CStr, CString, OsString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use fabric_domain::FabricError;
use sha2::{Digest, Sha256};

use crate::{source_error, storage_error};

pub(crate) const PREFIX: &str = "directory-sha256:";

pub(crate) fn identity(path: &Path, deadline: Instant) -> Result<String, FabricError> {
    scan(path, None, deadline)
}

pub(crate) fn snapshot(
    source: &Path,
    destination: &Path,
    expected: &str,
    deadline: Instant,
) -> Result<(), FabricError> {
    if scan(source, Some(destination), deadline)? != expected
        || identity(source, deadline)? != expected
    {
        return Err(source_error(
            "Directory source changed during snapshotting.",
        ));
    }
    Ok(())
}

/// Resolve a not-yet-created storage path through its existing ancestors first.
pub(crate) fn outside_source(root: &Path, source: &Path) -> Result<(), FabricError> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir().map_err(storage_error)?.join(root)
    };
    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for part in suffix.iter().rev() {
                    resolved.push(part);
                }
                // Canonicalize lexical parents even when the final directory is absent.
                let mut normalized = PathBuf::new();
                for part in resolved.components() {
                    match part {
                        Component::ParentDir => {
                            normalized.pop();
                        }
                        Component::CurDir => {}
                        _ => normalized.push(part.as_os_str()),
                    }
                }
                if normalized.starts_with(source) {
                    return Err(source_error(
                        "Fabric storage must be outside the directory source.",
                    ));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or_else(|| storage_error(error))?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .ok_or_else(|| source_error("Invalid storage path."))?;
            }
            Err(error) => return Err(storage_error(error)),
        }
    }
}

fn unchanged(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
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

pub(crate) fn open_at(parent: &File, name: &CStr, directory: bool) -> Result<File, FabricError> {
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: the parent owns its descriptor and name is NUL-terminated.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(storage_error(std::io::Error::last_os_error()));
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn names(directory: &File) -> Result<Vec<OsString>, FabricError> {
    let dot = CString::new(".").expect("literal");
    let copy = open_at(directory, &dot, true)?;
    use std::os::fd::IntoRawFd;
    let fd = copy.into_raw_fd();
    // SAFETY: ownership transfers to DIR on success.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(storage_error(error));
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = Directory(stream);
    let mut result = Vec::new();
    loop {
        // Clearing errno distinguishes EOF from a failed readdir on Unix.
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(not(target_os = "macos"))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(storage_error(error));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            result.push(OsString::from_vec(name.to_vec()));
        }
    }
    result.sort();
    Ok(result)
}

fn scan(
    source: &Path,
    destination: Option<&Path>,
    deadline: Instant,
) -> Result<String, FabricError> {
    use std::os::unix::fs::OpenOptionsExt;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)
        .map_err(storage_error)?;
    let before = fs::symlink_metadata(source).map_err(storage_error)?;
    if !unchanged(&before, &directory.metadata().map_err(storage_error)?) {
        return Err(source_error("Directory source changed before traversal."));
    }
    let mut hash = Sha256::new();
    hash.update(b"fabric.directory.v1\0");
    let mut links = BTreeMap::new();
    let mut entries = BTreeMap::new();
    visit(
        &directory,
        (Path::new(""), destination),
        deadline,
        &mut hash,
        &mut links,
        &mut entries,
        &mut [0u8; 64 * 1024],
    )?;
    validate_links(&links, &entries)?;
    if !unchanged(
        &before,
        &fs::symlink_metadata(source).map_err(storage_error)?,
    ) {
        return Err(source_error("Directory source changed during traversal."));
    }
    Ok(format!("{PREFIX}{:x}", hash.finalize()))
}

fn visit(
    file: &File,
    paths: (&Path, Option<&Path>),
    deadline: Instant,
    hash: &mut Sha256,
    links: &mut BTreeMap<PathBuf, PathBuf>,
    entries: &mut BTreeMap<PathBuf, bool>,
    buffer: &mut [u8; 64 * 1024],
) -> Result<(), FabricError> {
    let (relative, destination) = paths;
    crate::prepare::check_deadline(deadline)?;
    let before = file.metadata().map_err(storage_error)?;
    field(hash, relative.as_os_str().as_bytes());
    hash.update((before.mode() & 0o7777).to_be_bytes());
    if before.is_dir() {
        entries.insert(relative.to_owned(), true);
        hash.update(b"d");
        if let Some(path) = destination {
            fs::create_dir(path).map_err(storage_error)?;
        }
        let children = names(file)?;
        hash.update((children.len() as u64).to_be_bytes());
        for child in children {
            crate::prepare::check_deadline(deadline)?;
            let name = CString::new(child.as_bytes()).map_err(storage_error)?;
            // O_PATH is not available on macOS. fstatat observes the entry without following links.
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(storage_error(std::io::Error::last_os_error()));
            }
            let stat = unsafe { stat.assume_init() };
            let relative = relative.join(&child);
            let target = destination.map(|path| path.join(&child));
            if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
                if stat.st_nlink != 1 {
                    return Err(source_error("Hardlinked source symlinks are forbidden."));
                }
                let count = unsafe {
                    libc::readlinkat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if count < 0 {
                    return Err(storage_error(std::io::Error::last_os_error()));
                }
                if count as usize == buffer.len() {
                    return Err(source_error("Source symlink target exceeds safety limit."));
                }
                let link = PathBuf::from(OsString::from_vec(buffer[..count as usize].to_vec()));
                field(hash, relative.as_os_str().as_bytes());
                hash.update((stat.st_mode as u32 & 0o7777).to_be_bytes());
                hash.update(b"l");
                field(hash, link.as_os_str().as_bytes());
                if let Some(target) = target {
                    std::os::unix::fs::symlink(&link, target).map_err(storage_error)?;
                }
                links.insert(relative.clone(), link);
                entries.insert(relative.clone(), false);
                let mut after = std::mem::MaybeUninit::<libc::stat>::uninit();
                if unsafe {
                    libc::fstatat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        after.as_mut_ptr(),
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } != 0
                {
                    return Err(storage_error(std::io::Error::last_os_error()));
                }
                let after = unsafe { after.assume_init() };
                // Reading twice also catches replacement without relying on platform stat timestamp fields.
                let count = unsafe {
                    libc::readlinkat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if stat.st_dev != after.st_dev
                    || stat.st_ino != after.st_ino
                    || stat.st_mode != after.st_mode
                    || count < 0
                    || &buffer[..count as usize] != links[&relative].as_os_str().as_bytes()
                {
                    return Err(source_error("Source symlink changed during traversal."));
                }
            } else if stat.st_mode & libc::S_IFMT == libc::S_IFDIR
                || stat.st_mode & libc::S_IFMT == libc::S_IFREG
            {
                let child = open_at(file, &name, stat.st_mode & libc::S_IFMT == libc::S_IFDIR)?;
                let opened = child.metadata().map_err(storage_error)?;
                if opened.dev() != stat.st_dev as u64 || opened.ino() != stat.st_ino {
                    return Err(source_error("Directory entry changed before snapshotting."));
                }
                visit(
                    &child,
                    (&relative, target.as_deref()),
                    deadline,
                    hash,
                    links,
                    entries,
                    buffer,
                )?;
            } else {
                return Err(source_error(
                    "Directory sources cannot contain special files.",
                ));
            }
        }
    } else if before.is_file() && before.nlink() == 1 {
        entries.insert(relative.to_owned(), false);
        hash.update(b"f");
        hash.update(before.len().to_be_bytes());
        let mut output = destination
            .map(|path| File::create_new(path).map_err(storage_error))
            .transpose()?;
        let mut input = file;
        let mut remaining = before.len();
        while remaining > 0 {
            crate::prepare::check_deadline(deadline)?;
            let size = remaining.min(buffer.len() as u64) as usize;
            let count = input.read(&mut buffer[..size]).map_err(storage_error)?;
            if count == 0 {
                return Err(source_error(
                    "Directory source shortened during snapshotting.",
                ));
            }
            hash.update(&buffer[..count]);
            if let Some(output) = &mut output {
                output.write_all(&buffer[..count]).map_err(storage_error)?;
            }
            remaining -= count as u64;
        }
        if let Some(output) = output {
            output.sync_all().map_err(storage_error)?;
        }
    } else {
        return Err(source_error(
            "Directory sources cannot contain special or hardlinked files.",
        ));
    }
    if !unchanged(&before, &file.metadata().map_err(storage_error)?) {
        return Err(source_error(
            "Directory source changed during snapshotting.",
        ));
    }
    if let Some(path) = destination {
        fs::set_permissions(path, fs::Permissions::from_mode(before.mode() & 0o7777))
            .map_err(storage_error)?;
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(storage_error)?;
    }
    Ok(())
}

fn validate_links(
    links: &BTreeMap<PathBuf, PathBuf>,
    entries: &BTreeMap<PathBuf, bool>,
) -> Result<(), FabricError> {
    for path in links.keys() {
        let mut pending: VecDeque<_> = path
            .components()
            .map(|part| part.as_os_str().to_owned())
            .collect();
        let mut resolved = PathBuf::new();
        let mut followed = 0;
        while let Some(part) = pending.pop_front() {
            match Path::new(&part).components().next() {
                Some(Component::CurDir) => {}
                Some(Component::ParentDir) if resolved.pop() => {}
                Some(Component::Normal(_)) => {
                    resolved.push(part);
                    if let Some(target) = links.get(&resolved) {
                        followed += 1;
                        if followed > 40 || target.is_absolute() {
                            return Err(source_error("Unsafe source symlink target."));
                        }
                        resolved.pop();
                        for part in target.components().rev() {
                            pending.push_front(part.as_os_str().to_owned());
                        }
                    } else if !entries.contains_key(&resolved)
                        || (!pending.is_empty() && !entries[&resolved])
                    {
                        return Err(source_error(
                            "Source symlink target is absent or not a directory.",
                        ));
                    }
                }
                _ => {
                    return Err(source_error(
                        "Source symlink escapes the selected directory.",
                    ));
                }
            }
        }
    }
    Ok(())
}
