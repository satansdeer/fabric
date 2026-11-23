//! Best-effort logical accounting and conservative, cross-process admission.
use super::*;
use fabric_domain::{PreparedStateId, StoragePolicy};
use fabric_store::PreparedState;
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub trait DiskMeter: Send + Sync {
    fn free_bytes(&self, root: &Path) -> Result<u64, MeterCause>;
}

pub struct NativeDiskMeter;
impl DiskMeter for NativeDiskMeter {
    fn free_bytes(&self, root: &Path) -> Result<u64, MeterCause> {
        use std::os::unix::ffi::OsStrExt;
        let path =
            CString::new(root.as_os_str().as_bytes()).map_err(|_| MeterCause::InvalidPath)?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(MeterCause::io(
                MeterOperation::Statvfs,
                std::io::Error::last_os_error(),
            ));
        }
        let stat = unsafe { stat.assume_init() };
        (stat.f_bavail as u64)
            .checked_mul(stat.f_frsize)
            .ok_or(MeterCause::FreeSpaceOverflow)
    }
}

#[derive(Clone)]
pub struct StorageOptions {
    pub policy: StoragePolicy,
    pub meter: Arc<dyn DiskMeter>,
}
impl Default for StorageOptions {
    fn default() -> Self {
        Self {
            policy: StoragePolicy::default(),
            meter: Arc::new(NativeDiskMeter),
        }
    }
}

/// Content-free measurement evidence; never constructed from an error's text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MeterFailure {
    pub stage: MeterStage,
    #[serde(flatten)]
    pub cause: MeterCause,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeterStage {
    WorkspaceUsage,
    ManagedUsage,
    FreeSpace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum MeterCause {
    DepthLimit,
    EntryLimit,
    TimeLimit,
    MountedSubtree,
    IdentityChanged,
    InvalidSize,
    UsageOverflow,
    InvalidPath,
    FreeSpaceOverflow,
    Io {
        operation: MeterOperation,
        os_code: Option<i32>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeterOperation {
    OpenDirectory,
    ReadDirectory,
    Metadata,
    Statvfs,
}

impl MeterCause {
    fn io(operation: MeterOperation, error: std::io::Error) -> Self {
        Self::Io {
            operation,
            os_code: error.raw_os_error(),
        }
    }
}

impl From<MeterCause> for FabricError {
    fn from(cause: MeterCause) -> Self {
        storage_error(format!("Storage meter: {cause:?}"))
    }
}

/// Logical file lengths, including symlink text, never APFS exclusive physical bytes.
/// ponytail: bounded full scan; use incremental accounting if large trees need it.
pub fn usage(path: &Path) -> Result<u64, FabricError> {
    measured_usage(path).map_err(Into::into)
}

fn measured_usage(path: &Path) -> Result<u64, MeterCause> {
    use MeterOperation::{Metadata, OpenDirectory, ReadDirectory};
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|error| MeterCause::io(OpenDirectory, error))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut remaining = 200_000usize;
    fn scan(
        dir: &File,
        device: u64,
        depth: usize,
        remaining: &mut usize,
        deadline: Instant,
    ) -> Result<u64, MeterCause> {
        if depth > 64 {
            return Err(MeterCause::DepthLimit);
        }
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(MeterCause::io(
                OpenDirectory,
                std::io::Error::last_os_error(),
            ));
        }
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(MeterCause::io(ReadDirectory, error));
        }
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe { libc::closedir(self.0) };
            }
        }
        let stream = Stream(stream);
        let mut bytes = 0u64;
        loop {
            if *remaining == 0 {
                return Err(MeterCause::EntryLimit);
            }
            if Instant::now() >= deadline {
                return Err(MeterCause::TimeLimit);
            }
            #[cfg(target_os = "macos")]
            let errno = unsafe { libc::__error() };
            #[cfg(not(target_os = "macos"))]
            let errno = unsafe { libc::__errno_location() };
            unsafe {
                *errno = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if unsafe { *errno } != 0 {
                    return Err(MeterCause::io(
                        ReadDirectory,
                        std::io::Error::last_os_error(),
                    ));
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name == c"." || name == c".." {
                continue;
            }
            *remaining -= 1;
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    continue;
                }
                return Err(MeterCause::io(Metadata, error));
            }
            let stat = unsafe { metadata.assume_init() };
            if stat.st_dev as u64 != device {
                return Err(MeterCause::MountedSubtree);
            }
            let value = if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                let fd = unsafe {
                    libc::openat(
                        dir.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    let error = std::io::Error::last_os_error();
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                    ) {
                        continue;
                    }
                    return Err(MeterCause::io(OpenDirectory, error));
                }
                let child = unsafe { File::from_raw_fd(fd) };
                let metadata = crate::artifacts::stat(&child).map_err(|_| MeterCause::Io {
                    operation: Metadata,
                    os_code: None,
                })?;
                check_identity(&metadata, &Identity::of(&stat))
                    .map_err(|_| MeterCause::IdentityChanged)?;
                scan(&child, device, depth + 1, remaining, deadline)?
            } else {
                u64::try_from(stat.st_size).map_err(|_| MeterCause::InvalidSize)?
            };
            bytes = bytes.checked_add(value).ok_or(MeterCause::UsageOverflow)?;
        }
        Ok(bytes)
    }
    scan(
        &directory,
        directory
            .metadata()
            .map_err(|error| MeterCause::io(Metadata, error))?
            .dev(),
        0,
        &mut remaining,
        deadline,
    )
}

pub(crate) struct Reservation {
    root: PathBuf,
    token: String,
    owner: super::leases::Owner,
}
impl Reservation {
    pub(crate) fn file(&self) -> &File {
        self.owner.file()
    }
    pub(crate) fn launching(&mut self, store: &Store) -> Result<(), FabricError> {
        self.owner.launching(store, &self.token)
    }
    pub(crate) fn started(&self, store: &Store, pid: u32) -> Result<(), FabricError> {
        self.owner.started(store, &self.token, pid)
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        // Failure deliberately retains capacity; expiry alone does not release authority.
        if let Ok((_, store)) = open_store(&self.root) {
            if self.owner.release(&store, &self.token).is_ok() {
                let _ = store.release_storage(&self.token);
            }
        }
    }
}

fn headroom(
    root: &Path,
    store: &Store,
    options: &StorageOptions,
    requested: u64,
) -> Result<bool, FabricError> {
    let reserved = store.reserved_storage_bytes().map_err(storage_error)?;
    let additional = reserved
        .checked_add(requested)
        .ok_or_else(|| storage_error("Storage reservation overflow"))?;
    Ok(usage(root)?
        .checked_add(additional)
        .is_some_and(|total| total <= options.policy.max_managed_bytes)
        && options
            .meter
            .free_bytes(root)?
            .checked_sub(additional)
            .is_some_and(|free| free >= options.policy.min_free_bytes))
}

fn resume_evictions(root: &Path, store: &Store) -> Result<(), FabricError> {
    for (id, name, authority) in store.cache_evictions().map_err(storage_error)? {
        let _key_lease = fabric_project::prepare::lock_prepared(root, &name)?;
        let saved: CleanupAuthority = serde_json::from_str(&authority).map_err(storage_error)?;
        let name = CString::new(name).map_err(storage_error)?;
        let opened = open_owned_cleanup(root, c"prepared", &name, &id, Some(&saved))?;
        opened.delete_named(&name, &mut |_| Ok(()))?;
        store
            .finish_cache_eviction(&id, &authority)
            .map_err(storage_error)?;
    }
    Ok(())
}

fn make_room(
    root: &Path,
    store: &mut Store,
    options: &StorageOptions,
    requested: u64,
    protected: Option<&PreparedStateId>,
) -> Result<(), FabricError> {
    resume_evictions(root, store)?;
    if headroom(root, store, options, requested)? {
        return Ok(());
    }
    for id in store.workspace_ids().map_err(storage_error)? {
        clean_one(root, store, &id, true, &mut |_| Ok(()))?;
    }
    for state in store.prepared_states().map_err(storage_error)? {
        if headroom(root, store, options, requested)? {
            return Ok(());
        }
        if protected == Some(&state.state_id)
            || store
                .cache_retention(&state.state_id)
                .map_err(storage_error)?
                .is_some()
        {
            continue;
        }
        // Validation and exact descriptor identities precede the transactional cutover.
        let name = format!("{}-{}", state.project_id, state.preparation_key);
        let _key_lease = fabric_project::prepare::lock_prepared(root, &name)?;
        fabric_project::prepare::validate_ready(root, &state)?;
        let component = CString::new(name.as_str()).map_err(storage_error)?;
        let opened =
            open_owned_cleanup(root, c"prepared", &component, state.state_id.as_str(), None)?;
        let authority = serde_json::to_string(&opened.authority).map_err(storage_error)?;
        store
            .begin_cache_eviction(&state, &name, &authority)
            .map_err(storage_error)?;
        opened.delete_named(&component, &mut |_| Ok(()))?;
        store
            .finish_cache_eviction(state.state_id.as_str(), &authority)
            .map_err(storage_error)?;
    }
    if headroom(root, store, options, requested)? {
        return Ok(());
    }
    Err(storage_error(format!(
        "storage_admission: insufficient headroom for {requested} reserved bytes; managed limit {}, free-space reserve {}. Inspect storage status and storage gc --dry-run; preserve recovery-required workspaces.",
        options.policy.max_managed_bytes, options.policy.min_free_bytes
    )))
}

pub(crate) fn admit(
    root: &Path,
    options: &StorageOptions,
    state: &PreparedState,
    token: &str,
) -> Result<Reservation, FabricError> {
    options.policy.validate()?;
    if state.size_bytes > options.policy.max_workspace_bytes {
        return Err(storage_error(
            "storage_admission: prepared tree exceeds max_workspace_bytes",
        ));
    }
    let requested = options
        .policy
        .max_workspace_bytes
        .checked_add(options.policy.max_log_bytes_per_run)
        .and_then(|bytes| bytes.checked_add(options.policy.max_artifact_bytes_per_run))
        .ok_or_else(|| storage_error("Storage estimate overflow"))?;
    let (root, mut store, _lease) = cleanup_store(root, true, true)?;
    super::leases::reconcile(&root, &mut store)?;
    make_room(&root, &mut store, options, requested, Some(&state.state_id))?;
    let owner = super::leases::Owner::acquire(&root, &store, token)?;
    store
        .reserve_storage(token, Some(&state.state_id), requested)
        .map_err(storage_error)?;
    Ok(Reservation {
        root,
        token: token.to_owned(),
        owner,
    })
}

/// Root lease serializes preparation with admission/eviction; existing workers continue.
pub fn prepare(
    root: &Path,
    name: &str,
    recipe: &Path,
    options: &StorageOptions,
) -> Result<PreparedState, FabricError> {
    options.policy.validate()?;
    let (root, mut store, _lease) = cleanup_store(root, true, true)?;
    super::leases::reconcile(&root, &mut store)?;
    // Resume deletion before taking the requested key lock: its own interrupted
    // eviction must finish before the shared preparation path resolves READY reuse.
    resume_evictions(&root, &store)?;
    fabric_project::prepare::prepare_with_admission(&root, name, recipe, || {
        make_room(
            &root,
            &mut store,
            options,
            options.policy.max_workspace_bytes,
            None,
        )
    })
}

#[derive(serde::Serialize)]
pub struct UsageItem {
    pub id: String,
    pub run_id: Option<RunId>,
    pub path: String,
    pub usage_bytes: Option<u64>,
    pub retention_reason: String,
    pub diagnostic: Option<String>,
    pub action: String,
}
#[derive(serde::Serialize)]
pub struct StorageReport {
    pub accounting: &'static str,
    pub managed_bytes: u64,
    pub free_bytes: u64,
    pub reserved_bytes: u64,
    pub policy: StoragePolicy,
    pub caches: Vec<UsageItem>,
    pub workspaces: Vec<UsageItem>,
}
pub fn status(root: &Path, options: &StorageOptions) -> Result<StorageReport, FabricError> {
    let (root, store, _lease) = cleanup_store(root, false, true)?;
    let item = |id: String,
                path: String,
                expected: PathBuf,
                retention_reason: String,
                absent: bool,
                container: bool| {
        let measured = if Path::new(&path) != expected {
            Err(storage_error(
                "Registered storage path is not its owned location",
            ))
        } else if absent {
            match std::fs::symlink_metadata(expected.parent().unwrap()) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
                Err(error) => Err(storage_error(error)),
                Ok(_) => Err(storage_error(
                    "Reclaimed storage container unexpectedly exists",
                )),
            }
        } else {
            expected
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| storage_error("Missing storage parent"))
                .and_then(private_directory)
                .and_then(|()| private_directory(expected.parent().unwrap()))
                .and_then(|()| {
                    if container {
                        private_directory(&expected)?;
                        usage(expected.parent().unwrap())
                    } else {
                        usage(&expected)
                    }
                })
        };
        UsageItem {
            id,
            run_id: None,
            path,
            retention_reason,
            usage_bytes: measured.as_ref().ok().copied(),
            diagnostic: measured.err().map(|e| e.message),
            action: "No recovery action required.".into(),
        }
    };
    let caches = store
        .prepared_states()
        .map_err(storage_error)?
        .into_iter()
        .map(|state| {
            let reason = store
                .cache_retention(&state.state_id)
                .map_err(storage_error)?
                .unwrap_or_else(|| "rebuildable_unreferenced".into());
            let expected = root
                .join("prepared")
                .join(format!("{}-{}", state.project_id, state.preparation_key))
                .join("tree");
            Ok(item(
                state.state_id.to_string(),
                state.prepared_path,
                expected,
                reason,
                false,
                false,
            ))
        })
        .collect::<Result<Vec<_>, FabricError>>()?;
    let mut workspaces = store
        .workspace_ids()
        .map_err(storage_error)?
        .into_iter()
        .map(|id| {
            let workspace = store
                .workspace(&id)
                .map_err(storage_error)?
                .ok_or_else(|| storage_error("Missing workspace"))?;
            let reason = workspace.retention_reason.clone().unwrap_or_else(|| {
                match workspace.state {
                    WorkspaceState::Active => "active_worker",
                    WorkspaceState::CleanupPending => "verified_capture_cleanup_pending",
                    WorkspaceState::Reclaimed => "reclaimed_artifacts_preserved",
                    _ => "recoverability_not_verified",
                }
                .into()
            });
            let expected = root.join("fork-staging").join(id.as_str()).join("tree");
            let mut entry = item(
                id.to_string(),
                workspace.managed_path,
                expected,
                reason,
                workspace.state == WorkspaceState::Reclaimed,
                true,
            );
            entry.action = match workspace.state {
                WorkspaceState::Reclaimed => "Already reclaimed; artifacts preserved.".into(),
                WorkspaceState::Active => format!(
                    "Worker for Run {} may be active; do not remove its workspace.",
                    workspace.run_id
                ),
                _ => format!(
                    "Inspect recovery: fabric recover {} --json",
                    workspace.run_id
                ),
            };
            entry.run_id = Some(workspace.run_id);
            Ok(entry)
        })
        .collect::<Result<Vec<_>, FabricError>>()?;
    workspaces.sort_by_key(|workspace| std::cmp::Reverse(workspace.usage_bytes));
    Ok(StorageReport {
        accounting: "logical_bytes_best_effort_not_exclusive",
        managed_bytes: usage(&root)?,
        free_bytes: options.meter.free_bytes(&root)?,
        reserved_bytes: store.reserved_storage_bytes().map_err(storage_error)?,
        policy: options.policy.clone(),
        caches,
        workspaces,
    })
}

pub(crate) fn check_launch(root: &Path, options: &StorageOptions) -> Result<(), FabricError> {
    if options.meter.free_bytes(root)? < options.policy.min_free_bytes
        || usage(root)? > options.policy.max_managed_bytes
    {
        return Err(storage_error(
            "storage_admission: pressure before process launch",
        ));
    }
    Ok(())
}

pub(crate) fn violation(
    root: &Path,
    workspace: &Path,
    options: &StorageOptions,
) -> Result<Option<fabric_runner::StopReason>, MeterFailure> {
    use fabric_runner::StopReason;
    let bytes = measured_usage(workspace).map_err(|cause| MeterFailure {
        stage: MeterStage::WorkspaceUsage,
        cause,
    })?;
    if bytes > options.policy.max_workspace_bytes {
        return Ok(Some(StopReason::WorkspaceLimit));
    }
    if bytes >= options.policy.max_workspace_bytes.saturating_mul(9) / 10 {
        eprintln!("warning: workspace near storage limit: {bytes} bytes");
    }
    let free = options
        .meter
        .free_bytes(root)
        .map_err(|cause| MeterFailure {
            stage: MeterStage::FreeSpace,
            cause,
        })?;
    if free < options.policy.min_free_bytes
        || measured_usage(root).map_err(|cause| MeterFailure {
            stage: MeterStage::ManagedUsage,
            cause,
        })? > options.policy.max_managed_bytes
    {
        return Ok(Some(StopReason::StoragePressure));
    }
    Ok(None)
}

pub(crate) fn monitor(
    root: &Path,
    workspace: &Path,
    options: &StorageOptions,
    token: &fabric_runner::CancellationToken,
    done: &std::sync::atomic::AtomicBool,
) -> Option<MeterFailure> {
    use std::sync::atomic::Ordering;
    while !done.load(Ordering::Acquire) {
        let (reason, failure) = match violation(root, workspace, options) {
            Ok(Some(reason)) => (reason, None),
            Err(failure) => (fabric_runner::StopReason::StorageMeterFailed, Some(failure)),
            Ok(None) => {
                std::thread::park_timeout(Duration::from_millis(100));
                continue;
            }
        };
        token.stop_for_storage(reason);
        return failure;
    }
    None
}
