//! Crash ownership is a kernel lock plus native process identity, never a PID kill target.
use super::*;
use std::os::fd::AsRawFd;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessIdentity {
    pid: u32,
    seconds: u64,
    micros: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    file: Identity,
    owner: Option<ProcessIdentity>,
    released: bool,
    launching: bool,
    worker: Option<ProcessIdentity>,
}

pub(super) struct Owner {
    file: File,
    authority: Authority,
}

impl Owner {
    pub(super) fn acquire(root: &Path, store: &Store, token: &str) -> Result<Self, FabricError> {
        // IDs, not arbitrary paths, name permanent lock files. Never unlink/reuse an inode.
        token.parse::<RunId>().map_err(storage_error)?;
        let directory = root.join("leases");
        match fs::create_dir(&directory) {
            Ok(()) => {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                    .map_err(storage_error)?;
                File::open(root)
                    .and_then(|f| f.sync_all())
                    .map_err(storage_error)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(storage_error(e)),
        }
        private_directory(&directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(token))
            .map_err(storage_error)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(storage_error(std::io::Error::last_os_error()));
        }
        file.sync_all().map_err(storage_error)?;
        File::open(&directory)
            .and_then(|f| f.sync_all())
            .map_err(storage_error)?;
        let authority = Authority {
            file: Identity::of(&crate::artifacts::stat(&file)?),
            owner: identity(std::process::id()),
            launching: false,
            worker: None,
            released: false,
        };
        store
            .create_worker_lease(
                token,
                &serde_json::to_string(&authority).map_err(storage_error)?,
                timestamp(0)?.saturating_add(3600),
            )
            .map_err(storage_error)?;
        Ok(Self { file, authority })
    }

    pub(super) fn file(&self) -> &File {
        &self.file
    }

    pub(super) fn launching(&mut self, store: &Store, token: &str) -> Result<(), FabricError> {
        self.authority.launching = true;
        self.persist(store, token)
    }
    pub(super) fn started(&self, store: &Store, token: &str, pid: u32) -> Result<(), FabricError> {
        let authority = Authority {
            file: self.authority.file.clone(),
            owner: self.authority.owner.clone(),
            launching: true,
            released: false,
            worker: identity(pid),
        };
        store
            .update_worker_lease(
                token,
                &serde_json::to_string(&authority).map_err(storage_error)?,
            )
            .map_err(storage_error)
    }

    pub(super) fn release(&mut self, store: &Store, token: &str) -> Result<(), FabricError> {
        self.authority.released = true;
        self.persist(store, token)?;
        // Only normal foreground completion attests the owned group is gone.
        // End its lock now, not when unrelated concurrent forks eventually exec.
        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(storage_error(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn persist(&self, store: &Store, token: &str) -> Result<(), FabricError> {
        store
            .update_worker_lease(
                token,
                &serde_json::to_string(&self.authority).map_err(storage_error)?,
            )
            .map_err(storage_error)
    }
}
// No Drop unlock: supervisor death must not unlock an orphaned worker's inherited
// description. Only release after owned supervision completes may do that.

pub(super) enum Status {
    Active,
    Inactive(CleanupLease),
    Unknown,
    Legacy,
}

pub(super) fn status(root: &Path, store: &Store, token: &str) -> Result<Status, FabricError> {
    let Some((saved, _)) = store.worker_lease(token).map_err(storage_error)? else {
        return Ok(Status::Legacy);
    };
    let check = || -> Result<Status, FabricError> {
        token.parse::<RunId>().map_err(storage_error)?;
        let authority: Authority = serde_json::from_str(&saved).map_err(storage_error)?;
        private_directory(&root.join("leases"))?;
        let path = root.join("leases").join(token);
        private_file(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(storage_error)?;
        if Identity::of(&crate::artifacts::stat(&file)?) != authority.file {
            return Ok(Status::Unknown);
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Ok(
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                    Status::Active
                } else {
                    Status::Unknown
                },
            );
        }
        let guard = CleanupLease(file);
        if authority.released {
            return Ok(Status::Inactive(guard));
        }
        // A worker may close inherited descriptors. Native identity/group inspection
        // is an additional positive protection; failure to inspect retains ownership.
        match process_live(authority.owner.as_ref()) {
            Some(true) => return Ok(Status::Active),
            None => return Ok(Status::Unknown),
            Some(false) => {}
        }
        if authority.launching {
            let Some(worker) = authority.worker.as_ref() else {
                return Ok(Status::Unknown);
            };
            match group_live(worker) {
                Some(false) => {}
                Some(true) => return Ok(Status::Active),
                None => return Ok(Status::Unknown),
            }
        }
        Ok(Status::Inactive(guard))
    };
    // Missing, replaced or malformed authority is not proof of death.
    Ok(check().unwrap_or(Status::Unknown))
}

/// Called only while holding the root cleanup/admission lock. Inactive guards stay
/// locked until the atomic metadata transition and reservation release complete.
pub(super) fn reconcile(root: &Path, store: &mut Store) -> Result<(), FabricError> {
    let now = timestamp(0)?;
    for token in store.worker_lease_tokens().map_err(storage_error)? {
        match status(root, store, &token)? {
            Status::Inactive(_guard) => store
                .reconcile_interrupted(&token, true, now)
                .map_err(storage_error)?,
            Status::Unknown => {
                if store
                    .worker_lease(&token)
                    .map_err(storage_error)?
                    .is_some_and(|(_, expiry)| expiry <= now)
                {
                    store
                        .reconcile_interrupted(&token, false, now)
                        .map_err(storage_error)?;
                }
            }
            Status::Active | Status::Legacy => {}
        }
    }
    // Pre-upgrade/unverifiable runs retain all bytes. Expiry permits an honest
    // recovery diagnostic, never releasing their unowned reservation or deletion.
    for id in store.workspace_ids().map_err(storage_error)? {
        let workspace = store.workspace(&id).map_err(storage_error)?.unwrap();
        if store
            .worker_lease(workspace.run_id.as_str())
            .map_err(storage_error)?
            .is_none()
            && workspace
                .lease_expires_at
                .is_none_or(|expiry| expiry <= now)
        {
            store
                .reconcile_interrupted(workspace.run_id.as_str(), false, now)
                .map_err(storage_error)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn info(pid: u32) -> Result<Option<libc::proc_bsdinfo>, ()> {
    let pid = i32::try_from(pid).map_err(|_| ())?;
    let mut value = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let count = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            value.as_mut_ptr().cast(),
            size,
        )
    };
    if count == size {
        let value = unsafe { value.assume_init() };
        Ok((value.pbi_status != libc::SZOMB).then_some(value))
    } else if count == 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Err(())
    }
}

#[cfg(target_os = "macos")]
fn identity(pid: u32) -> Option<ProcessIdentity> {
    info(pid).ok().flatten().map(|p| ProcessIdentity {
        pid,
        seconds: p.pbi_start_tvsec,
        micros: p.pbi_start_tvusec,
    })
}

#[cfg(target_os = "macos")]
fn process_live(saved: Option<&ProcessIdentity>) -> Option<bool> {
    let saved = saved?;
    info(saved.pid).ok().map(|current| {
        current.is_some_and(|p| {
            (p.pbi_start_tvsec, p.pbi_start_tvusec) == (saved.seconds, saved.micros)
        })
    })
}

#[cfg(target_os = "macos")]
fn group_live(worker: &ProcessIdentity) -> Option<bool> {
    if process_live(Some(worker))? {
        return Some(true);
    }
    // PROC_PGRP_ONLY (libproc.h): inspect, never signal, a persisted process group.
    const PGRP: u32 = 2;
    unsafe {
        *libc::__error() = 0;
    }
    let bytes = unsafe { libc::proc_listpids(PGRP, worker.pid, std::ptr::null_mut(), 0) };
    if bytes < 0 || (bytes == 0 && std::io::Error::last_os_error().raw_os_error() != Some(0)) {
        return None;
    }
    let mut pids = vec![0u32; bytes as usize / std::mem::size_of::<u32>() + 32];
    let capacity = (pids.len() * std::mem::size_of::<u32>()) as i32;
    unsafe {
        *libc::__error() = 0;
    }
    let used = unsafe { libc::proc_listpids(PGRP, worker.pid, pids.as_mut_ptr().cast(), capacity) };
    if used < 0
        || used >= capacity
        || (used == 0 && std::io::Error::last_os_error().raw_os_error() != Some(0))
    {
        return None;
    }
    for pid in &pids[..used as usize / std::mem::size_of::<u32>()] {
        if *pid != 0 && info(*pid).ok()?.is_some() {
            return Some(true);
        }
    }
    Some(false)
}

#[cfg(not(target_os = "macos"))]
fn identity(_pid: u32) -> Option<ProcessIdentity> {
    None
}
#[cfg(not(target_os = "macos"))]
fn process_live(_saved: Option<&ProcessIdentity>) -> Option<bool> {
    None
}
#[cfg(not(target_os = "macos"))]
fn group_live(_worker: &ProcessIdentity) -> Option<bool> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::super::cleanup_tests::Fixture;
    use super::*;
    use std::process::{Command, Stdio};

    fn options() -> storage::StorageOptions {
        let mut options = storage::StorageOptions::default();
        options.policy.max_managed_bytes = 64 * 1024 * 1024;
        options.policy.min_free_bytes = 1;
        options.policy.max_workspace_bytes = 1024 * 1024;
        options.policy.max_log_bytes_per_run = 1024 * 1024;
        options.policy.max_artifact_bytes_per_run = 1024 * 1024;
        options
    }

    #[test]
    fn reservation_crash_child() {
        let Some(root) = std::env::var_os("FABRIC_LEASE_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let (_, store) = open_store(&root).unwrap();
        let state = store
            .prepared_state(
                &std::env::var("FABRIC_LEASE_TEST_STATE")
                    .unwrap()
                    .parse()
                    .unwrap(),
            )
            .unwrap()
            .unwrap();
        let _reservation = storage::admit(&root, &options(), &state, "r-prelaunch-crash").unwrap();
        let ready = std::env::var_os("FABRIC_LEASE_TEST_READY").unwrap();
        File::create(ready).unwrap().write_all(b"ready\n").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn pre_run_crash_reservation_reconciles_before_admission_without_headroom_leak() {
        let fixture = Fixture::captured();
        let (_, store) = open_store(&fixture.root).unwrap();
        let workspace = store.workspace(&fixture.id).unwrap().unwrap();
        let state = store.prepared_state(&workspace.state_id).unwrap().unwrap();
        let ready = fixture.temp.path().join("reservation-ready");
        let name = std::ffi::CString::new(ready.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace::leases::tests::reservation_crash_child",
                "--nocapture",
            ])
            .env("FABRIC_LEASE_TEST_ROOT", &fixture.root)
            .env("FABRIC_LEASE_TEST_STATE", state.state_id.as_str())
            .env("FABRIC_LEASE_TEST_READY", &ready)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut message = String::new();
        File::open(ready)
            .unwrap()
            .read_to_string(&mut message)
            .unwrap();
        assert_eq!(message, "ready\n");
        let db = rusqlite::Connection::open(fixture.root.join("fabric.db")).unwrap();
        db.execute(
            "UPDATE worker_leases SET expires_at=0 WHERE token='r-prelaunch-crash'",
            [],
        )
        .unwrap();
        garbage_collect(&fixture.root, true).unwrap();
        assert_eq!(store.reserved_storage_bytes().unwrap(), 3 * 1024 * 1024);
        assert!(
            store
                .run(&"r-prelaunch-crash".parse().unwrap())
                .unwrap()
                .is_none()
        );
        child.kill().unwrap();
        child.wait().unwrap();
        let mut limits = options();
        limits.policy.max_managed_bytes =
            storage::usage(&fixture.root, &limits).unwrap() + 3 * 1024 * 1024 + 128 * 1024;
        let next = storage::admit(&fixture.root, &limits, &state, "r-next").unwrap();
        assert_eq!(store.reserved_storage_bytes().unwrap(), 3 * 1024 * 1024);
        drop(next);
        assert_eq!(store.reserved_storage_bytes().unwrap(), 0);
        // READY reuse is a startup path too, not only a new-fork path.
        let prepared = storage::prepare(
            &fixture.root,
            "fixture",
            &fixture.temp.path().join("recipe.toml"),
            &limits,
        )
        .unwrap();
        assert_eq!(prepared.state_id, state.state_id);
        assert_eq!(store.reserved_storage_bytes().unwrap(), 0);
    }

    #[test]
    fn completed_owner_and_inactive_probe_unlock_despite_unrelated_fork() {
        for probe in [false, true] {
            let fixture = Fixture::captured();
            let (_, store) = open_store(&fixture.root).unwrap();
            let mut owner =
                Some(Owner::acquire(&fixture.root, &store, "r-inherited-lease").unwrap());
            let probe_guard = if probe {
                owner
                    .as_mut()
                    .unwrap()
                    .release(&store, "r-inherited-lease")
                    .unwrap();
                drop(owner.take());
                Some(status(&fixture.root, &store, "r-inherited-lease").unwrap())
            } else {
                None
            };
            let mut ready = [0; 2];
            let mut resume = [0; 2];
            assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
            assert_eq!(unsafe { libc::pipe(resume.as_mut_ptr()) }, 0);
            let child = unsafe { libc::fork() };
            assert!(child >= 0);
            if child == 0 {
                // No allocator or Rust destructor after a multithreaded fork.
                unsafe {
                    libc::close(ready[0]);
                    libc::close(resume[1]);
                    libc::write(ready[1], b"r".as_ptr().cast(), 1);
                    let mut byte = 0u8;
                    libc::read(resume[0], (&mut byte as *mut u8).cast(), 1);
                    libc::_exit(0);
                }
            }
            let mut byte = 0u8;
            assert_eq!(
                unsafe { libc::read(ready[0], (&mut byte as *mut u8).cast(), 1) },
                1
            );
            if let Some(mut owner) = owner.take() {
                owner.release(&store, "r-inherited-lease").unwrap();
            }
            drop(probe_guard);
            let inactive = matches!(
                status(&fixture.root, &store, "r-inherited-lease").unwrap(),
                Status::Inactive(_)
            );
            unsafe {
                libc::write(resume[1], b"x".as_ptr().cast(), 1);
                libc::waitpid(child, std::ptr::null_mut(), 0);
                for fd in [ready[0], ready[1], resume[0], resume[1]] {
                    libc::close(fd);
                }
            }
            assert!(
                inactive,
                "unrelated fork prolonged completed ownership (probe={probe})"
            );
        }
    }

    #[test]
    fn native_identity_protects_live_owner_after_descriptor_close_and_expiry() {
        let fixture = Fixture::captured();
        let (_, mut store) = open_store(&fixture.root).unwrap();
        let owner = Owner::acquire(&fixture.root, &store, "r-closed-descriptor").unwrap();
        store
            .reserve_storage("r-closed-descriptor", None, 4096)
            .unwrap();
        let current = identity(std::process::id()).unwrap();
        assert_eq!(process_live(Some(&current)), Some(true));
        let mut reused = current.clone();
        reused.micros = reused.micros.wrapping_add(1);
        assert_eq!(process_live(Some(&reused)), Some(false));
        drop(owner); // Simulate a trusted worker closing inherited descriptors.
        let db = rusqlite::Connection::open(fixture.root.join("fabric.db")).unwrap();
        db.execute(
            "UPDATE worker_leases SET expires_at=0 WHERE token='r-closed-descriptor'",
            [],
        )
        .unwrap();
        assert!(matches!(
            status(&fixture.root, &store, "r-closed-descriptor").unwrap(),
            Status::Active
        ));
        reconcile(&fixture.root, &mut store).unwrap();
        assert_eq!(store.reserved_storage_bytes().unwrap(), 4096);
    }

    #[test]
    fn interrupted_exit_capture_preserves_observed_status_and_unique_evidence_idempotently() {
        for phase in ["PREPARING", "RUNNING", "FINALIZING"] {
            let fixture = Fixture::captured();
            let (_, store) = open_store(&fixture.root).unwrap();
            let prior = store.workspace(&fixture.id).unwrap().unwrap();
            let run_id: RunId = "r-interrupted".parse().unwrap();
            let workspace_id: WorkspaceId = "w-interrupted".parse().unwrap();
            let now = timestamp(0).unwrap();
            let request = WorkspaceRequest {
                run_id: run_id.clone(),
                workspace_id: workspace_id.clone(),
                prepared_state_id: prior.state_id,
                harness: "fake".into(),
                prompt_digest: "digest".into(),
                created_at: now,
                lease_expires_at: now + 3600,
            };
            let backend = fabric_workspace::LocalWorkspaceBackend::new(
                &fixture.root,
                fabric_workspace::ForkStrategy::CloneOrCopy,
            )
            .unwrap();
            let tree = create_workspace(&fixture.root, &backend, &request, 1024 * 1024).unwrap();
            fs::write(tree.path().join("unique-interrupted-source"), b"only copy").unwrap();
            if phase != "PREPARING" {
                store
                    .start_run(&run_id, "fake", std::process::id(), now)
                    .unwrap();
            }
            if phase == "FINALIZING" {
                store
                    .observe_run_exit(&run_id, Some(7), None, None, now)
                    .unwrap();
                let capture = store.run(&prior.run_id).unwrap().unwrap().capture.unwrap();
                store.record_capture(&run_id, &capture).unwrap();
            }
            let before = store.run(&run_id).unwrap().unwrap();
            let db = rusqlite::Connection::open(fixture.root.join("fabric.db")).unwrap();
            db.execute(
                "UPDATE workspaces SET lease_expires_at=0 WHERE run_id=?1",
                [run_id.as_str()],
            )
            .unwrap();
            garbage_collect(&fixture.root, true).unwrap();
            let after = store.run(&run_id).unwrap().unwrap();
            assert_eq!(after.state, RunState::NeedsAttention);
            assert_eq!(after.exit_code, before.exit_code);
            assert_eq!(after.exit_signal, before.exit_signal);
            assert_eq!(after.finished_at, before.finished_at);
            assert_eq!(after.capture, before.capture);
            assert_eq!(after.artifact_manifest_id, None);
            assert!(store.artifacts(&run_id).unwrap().is_empty());
            assert_eq!(
                store.workspace(&workspace_id).unwrap().unwrap().state,
                WorkspaceState::RecoveryRequired
            );
            assert_eq!(
                fs::read(tree.path().join("unique-interrupted-source")).unwrap(),
                b"only copy"
            );
            garbage_collect(&fixture.root, true).unwrap();
            assert_eq!(store.run(&run_id).unwrap().unwrap(), after);
        }
    }

    #[test]
    fn terminal_capture_survives_owner_crash_before_release_and_cleanup() {
        use std::os::unix::process::CommandExt;
        let fixture = Fixture::captured();
        let (_, store) = open_store(&fixture.root).unwrap();
        let workspace = store.workspace(&fixture.id).unwrap().unwrap();
        let before = store.run(&workspace.run_id).unwrap().unwrap();
        assert_eq!(before.state, RunState::Succeeded);
        let artifacts = store.artifacts(&workspace.run_id).unwrap();
        let (saved, _) = store
            .worker_lease(workspace.run_id.as_str())
            .unwrap()
            .unwrap();
        let mut authority: Authority = serde_json::from_str(&saved).unwrap();
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let dead = identity(child.id()).unwrap();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        authority.owner = Some(dead.clone());
        authority.worker = Some(dead);
        authority.launching = true;
        authority.released = false;
        store
            .update_worker_lease(
                workspace.run_id.as_str(),
                &serde_json::to_string(&authority).unwrap(),
            )
            .unwrap();
        store
            .reserve_storage(workspace.run_id.as_str(), None, 4096)
            .unwrap();
        // Simulate the durable terminal/capture transaction preceding owner-release.
        // Neither that crash nor cleanup may rewrite the already observed outcome.
        let report = garbage_collect(&fixture.root, true).unwrap();
        assert_eq!(report.items[0].action, "reclaimed");
        assert_eq!(store.run(&workspace.run_id).unwrap().unwrap(), before);
        assert_eq!(store.artifacts(&workspace.run_id).unwrap(), artifacts);
        assert_eq!(store.reserved_storage_bytes().unwrap(), 0);
        garbage_collect(&fixture.root, true).unwrap();
        assert_eq!(store.run(&workspace.run_id).unwrap().unwrap(), before);
    }

    #[test]
    fn unverifiable_owner_or_launch_window_never_releases_or_reclaims() {
        let fixture = Fixture::captured();
        let (_, mut store) = open_store(&fixture.root).unwrap();
        let workspace = store.workspace(&fixture.id).unwrap().unwrap();
        let (saved, _) = store
            .worker_lease(workspace.run_id.as_str())
            .unwrap()
            .unwrap();
        let mut authority: Authority = serde_json::from_str(&saved).unwrap();
        authority.file.inode = authority.file.inode.wrapping_add(1);
        store
            .update_worker_lease(
                workspace.run_id.as_str(),
                &serde_json::to_string(&authority).unwrap(),
            )
            .unwrap();
        store
            .reserve_storage(workspace.run_id.as_str(), None, 4096)
            .unwrap();
        let db = rusqlite::Connection::open(fixture.root.join("fabric.db")).unwrap();
        db.execute(
            "UPDATE runs SET pid_metadata=?1",
            [format!("local-process-group:{}", std::process::id())],
        )
        .unwrap();
        let report = garbage_collect(&fixture.root, true).unwrap();
        assert_eq!(report.items[0].reason, "worker_owner_unverifiable");
        assert!(
            Path::new(&workspace.managed_path)
                .join("new-source")
                .exists()
        );
        assert_eq!(store.reserved_storage_bytes().unwrap(), 4096);
        // Spawn-before-callback crash: no saved worker identity, so no negative
        // liveness proof even if the inherited lock is no longer held.
        let mut owner = Owner::acquire(&fixture.root, &store, "r-launch-window").unwrap();
        owner.launching(&store, "r-launch-window").unwrap();
        let (saved, _) = store.worker_lease("r-launch-window").unwrap().unwrap();
        let mut authority: Authority = serde_json::from_str(&saved).unwrap();
        authority.owner.as_mut().unwrap().micros += 1;
        store
            .update_worker_lease(
                "r-launch-window",
                &serde_json::to_string(&authority).unwrap(),
            )
            .unwrap();
        store
            .reserve_storage("r-launch-window", None, 8192)
            .unwrap();
        drop(owner);
        assert!(matches!(
            status(&fixture.root, &store, "r-launch-window").unwrap(),
            Status::Unknown
        ));
        reconcile(&fixture.root, &mut store).unwrap();
        assert_eq!(store.reserved_storage_bytes().unwrap(), 12288);
    }
}
