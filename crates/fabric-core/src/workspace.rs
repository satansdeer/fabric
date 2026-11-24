//! Durable workspace ownership and lease-aware, descriptor-rooted reclamation.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fabric_domain::{
    ErrorKind, FabricError, PreparedStateId, RunId, RunState, WorkspaceId, WorkspaceState,
};
use fabric_store::{Run, RunWorkspaceTransition, Store, Workspace};
use fabric_workspace::{WorkspaceBackend, fork_path};

#[path = "leases.rs"]
mod leases;
#[path = "storage.rs"]
pub mod storage;

/// IDs belong to the caller and remain stable even when a fork fails.
#[derive(Debug)]
pub struct WorkspaceRequest {
    pub run_id: RunId,
    pub workspace_id: WorkspaceId,
    pub prepared_state_id: PreparedStateId,
    pub harness: String,
    pub prompt_digest: String,
    /// Unix seconds, including the startup reservation's expiry below.
    pub created_at: i64,
    /// Expiry is diagnostic only; kernel/process ownership remains authoritative.
    pub lease_expires_at: i64,
}

/// Only fresh durable ownership and marker validation can construct this value.
/// Revalidate with `runner_workspace` immediately before future process launch;
/// this is not a sandbox, an execution lease or permission to delete.
#[derive(Debug)]
pub struct RunnerWorkspace {
    run_id: RunId,
    path: PathBuf,
}
impl RunnerWorkspace {
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Commit the one-to-one pair before invoking the existing fork backend.
/// Partial failures retain ownership, diagnostics and every file; IDs cannot be
/// reused. Interrupted/pending ownership is never adopted as runner-ready.
pub fn create_workspace(
    root: &Path,
    backend: &impl WorkspaceBackend,
    request: &WorkspaceRequest,
    copy_limit_bytes: u64,
) -> Result<RunnerWorkspace, FabricError> {
    create_workspace_bound(root, backend, request, copy_limit_bytes, None, None)
}

pub(crate) fn create_workspace_bound(
    root: &Path,
    backend: &impl WorkspaceBackend,
    request: &WorkspaceRequest,
    copy_limit_bytes: u64,
    target: Option<(
        &fabric_runner::target::TargetPin,
        &fabric_runner::target::Target,
    )>,
    environment: Option<&fabric_domain::EnvironmentId>,
) -> Result<RunnerWorkspace, FabricError> {
    if request.created_at < 0 || request.lease_expires_at <= timestamp(request.created_at)? {
        return Err(storage_error(
            "Startup lease must expire after creation and the current time.",
        ));
    }
    let (root, mut store) = open_store(root)?;
    let state = store
        .prepared_state(&request.prepared_state_id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Unknown prepared state in this Fabric root."))?;
    let path = fork_path(&root, &request.workspace_id);
    let run = Run {
        run_id: request.run_id.clone(),
        batch_id: store
            .batch_for_run(&request.run_id)
            .map_err(storage_error)?,
        project_id: state.project_id.clone(),
        prepared_state_id: state.state_id.clone(),
        base_commit_sha: state.source_commit_sha.clone(),
        harness: request.harness.clone(),
        harness_version: None,
        prompt_digest: request.prompt_digest.clone(),
        state: RunState::Queued,
        workspace_id: request.workspace_id.clone(),
        pid_metadata: None,
        exit_code: None,
        exit_signal: None,
        created_at: request.created_at,
        updated_at: request.created_at,
        started_at: None,
        finished_at: None,
        error_kind: None,
        artifact_manifest_id: None,
        capture: None,
    };
    let workspace = Workspace {
        workspace_id: request.workspace_id.clone(),
        run_id: request.run_id.clone(),
        managed_path: path
            .to_str()
            .ok_or_else(|| storage_error("Workspace paths must be UTF-8."))?
            .to_owned(),
        state_id: state.state_id.clone(),
        state: WorkspaceState::Active,
        lease_expires_at: Some(request.lease_expires_at),
        usage_bytes: 0,
        retention_reason: Some("Workspace fork pending; not runner-ready.".into()),
    };
    store
        .create_run_workspace(&run, &workspace)
        .map_err(storage_error)?;
    store
        .transition_run_workspace(
            &run.run_id,
            RunWorkspaceTransition {
                expected_run: RunState::Queued,
                next_run: RunState::Preparing,
                expected_workspace: WorkspaceState::Active,
                next_workspace: WorkspaceState::Active,
                updated_at: request.created_at,
            },
        )
        .map_err(storage_error)?;
    let result =
        (|| {
            if let Some((pin, _)) = target {
                store
                    .record_run_target(
                        &run.run_id,
                        &serde_json::to_string(pin).map_err(storage_error)?,
                    )
                    .map_err(storage_error)?;
            }
            if let Some(environment) = environment {
                store
                    .bind_run_environment(&run.run_id, environment)
                    .map_err(storage_error)?;
            }
            let mut prepare = |backend: &dyn WorkspaceBackend| {
                let fork = backend.fork(&state, &request.workspace_id, copy_limit_bytes)?;
                if fork.path != path || fork.size_bytes != state.size_bytes {
                    return Err(storage_error(
                        "Backend returned an unexpected workspace path or size.",
                    ));
                }
                validate_directories(&root, &workspace)?;
                let container = path
                    .parent()
                    .ok_or_else(|| storage_error("Missing workspace container."))?;
                let mut marker = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(container.join(".fabric-owner"))
                    .map_err(storage_error)?;
                marker
                    .write_all(marker_contents(&root, &workspace).as_bytes())
                    .map_err(storage_error)?;
                marker.sync_all().map_err(storage_error)?;
                File::open(container)
                    .and_then(|dir| dir.sync_all())
                    .map_err(storage_error)?;
                store
                    .finish_workspace_fork(
                        &run.run_id,
                        Ok(fork.size_bytes),
                        timestamp(request.created_at)?,
                    )
                    .map_err(storage_error)?;
                let evidence = serde_json::json!({"size_bytes":fork.size_bytes,
                "cloned_files":fork.cloned_files,"copied_files":fork.copied_files});
                Ok((runner_workspace(&root, &run.run_id)?, evidence))
            };
            if let Some((_, target)) = target {
                if target.is_remote() {
                    // Controller-owned transfer staging, not a remote workspace or HOME.
                    return prepare(backend).map(|(workspace, _)| workspace);
                }
                use fabric_runner::target::{Action, ForkMode};
                use fabric_workspace::{ForkStrategy, LocalWorkspaceBackend};
                let committed = target.drive(
                if environment.is_some() { "environment.workspace" } else { "prepare" },
                run.run_id.as_str(),
                &[
                    Action::Fork {
                        strategy: ForkMode::CloneOrCopy,
                    },
                    Action::Fork {
                        strategy: ForkMode::Copy,
                    },
                ],
                if let Some(environment) = environment {
                    serde_json::json!({"size_bytes":state.size_bytes,"copy_limit":copy_limit_bytes,
                        "environment_id":environment})
                } else {
                    serde_json::json!({"size_bytes":state.size_bytes,"copy_limit":copy_limit_bytes})
                },
                |action| {
                    let strategy = match action {
                        Action::Fork {
                            strategy: ForkMode::CloneOrCopy,
                        } => ForkStrategy::CloneOrCopy,
                        Action::Fork {
                            strategy: ForkMode::Copy,
                        } => ForkStrategy::Copy,
                        _ => unreachable!("validated offered action"),
                    };
                    prepare(&LocalWorkspaceBackend::new(&root, strategy)?)
                },
            )?;
                if committed.failure.is_some() {
                    store
                        .record_target_failure(&run.run_id)
                        .map_err(storage_error)?;
                }
                committed.into_result()
            } else {
                prepare(backend).map(|(workspace, _)| workspace)
            }
        })();
    if let Err(error) = &result {
        store
            .finish_workspace_fork(
                &run.run_id,
                Err(&error.message),
                timestamp(request.created_at)?,
            )
            .map_err(|failure| {
                storage_error(format!(
                    "{error}; could not persist fork failure: {failure}"
                ))
            })?;
    }
    result
}

/// Reopen the configured root and re-read both owners and the private marker.
/// Unknown, incomplete, failed, relocated or tampered workspaces are refused.
pub fn runner_workspace(root: &Path, id: &RunId) -> Result<RunnerWorkspace, FabricError> {
    owned_workspace(root, id, true)
}

pub(super) fn capture_workspace(root: &Path, id: &RunId) -> Result<RunnerWorkspace, FabricError> {
    owned_workspace(root, id, false)
}

fn owned_workspace(
    root: &Path,
    id: &RunId,
    launching: bool,
) -> Result<RunnerWorkspace, FabricError> {
    let (root, store) = open_store(root)?;
    let run = store
        .run(id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Unknown workspace owner Run."))?;
    let workspace = store
        .workspace(&run.workspace_id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Missing workspace ownership record."))?;
    let now = timestamp(run.created_at)?;
    if run.state
        != if launching {
            RunState::Preparing
        } else {
            RunState::Finalizing
        }
        || workspace.state != WorkspaceState::Active
        || workspace.run_id != run.run_id
        || workspace.state_id != run.prepared_state_id
        || workspace.retention_reason.is_some()
        || (launching
            && workspace
                .lease_expires_at
                .is_none_or(|expiry| expiry <= now))
    {
        return Err(storage_error("Workspace ownership is not runner-ready."));
    }
    validate_owned_workspace(&root, &run, &workspace)
}

pub(super) fn validate_owned_workspace(
    root: &Path,
    run: &Run,
    workspace: &Workspace,
) -> Result<RunnerWorkspace, FabricError> {
    validate_owned_workspace_tree(root,run,workspace,"tree")
}

fn validate_owned_workspace_tree(root: &Path, run: &Run, workspace: &Workspace, tree_name: &str) -> Result<RunnerWorkspace, FabricError> {
    if workspace.run_id != run.run_id
        || workspace.workspace_id != run.workspace_id
        || workspace.state_id != run.prepared_state_id
    {
        return Err(storage_error("Workspace ownership records do not match."));
    }
    validate_directories_tree(root, workspace, tree_name)?;
    let path = Path::new(&workspace.managed_path);
    let marker = path
        .parent()
        .ok_or_else(|| storage_error("Missing workspace container."))?
        .join(".fabric-owner");
    let recorded = private_file(&marker)?;
    let expected = marker_contents(root, workspace);
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&marker)
        .map_err(storage_error)?;
    let meta = input.metadata().map_err(storage_error)?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.len() != expected.len() as u64
        || (meta.dev(), meta.ino(), meta.uid(), meta.mode())
            != (
                recorded.dev(),
                recorded.ino(),
                recorded.uid(),
                recorded.mode(),
            )
    {
        return Err(storage_error("Invalid workspace ownership marker."));
    }
    let mut buffer = [0; 1024];
    for chunk in expected.as_bytes().chunks(buffer.len()) {
        input
            .read_exact(&mut buffer[..chunk.len()])
            .map_err(storage_error)?;
        if chunk != &buffer[..chunk.len()] {
            return Err(storage_error(
                "Workspace ownership marker does not match its durable owner.",
            ));
        }
    }
    Ok(RunnerWorkspace {
        run_id: run.run_id.clone(),
        path: path.to_owned(),
    })
}

fn validate_directories(root: &Path, workspace: &Workspace) -> Result<(), FabricError> {
    validate_directories_tree(root,workspace,"tree")
}

fn validate_directories_tree(root: &Path, workspace: &Workspace, tree_name: &str) -> Result<(), FabricError> {
    private_directory(root)?;
    let expected = fork_path(root, &workspace.workspace_id);
    if Path::new(&workspace.managed_path) != expected {
        return Err(storage_error(
            "Workspace path is not its exact managed descendant.",
        ));
    }
    private_directory(&root.join("fork-staging"))?;
    private_directory(
        expected
            .parent()
            .ok_or_else(|| storage_error("Missing workspace container."))?,
    )?;
    let expected = expected.parent().ok_or_else(||storage_error("Missing container"))?.join(tree_name);
    private_directory(&expected)?;
    if expected.canonicalize().map_err(storage_error)? != expected {
        return Err(storage_error("Workspace path contains a symlink surprise."));
    }
    Ok(())
}

fn marker_contents(root: &Path, workspace: &Workspace) -> String {
    format!(
        "fabric.workspace.v1\n{}\n{}\n{}\n{}\n",
        root.display(),
        workspace.run_id,
        workspace.workspace_id,
        workspace.state_id
    )
}

pub(super) fn open_store(root: &Path) -> Result<(PathBuf, Store), FabricError> {
    // Never create a database in an unknown root or follow a root/database link.
    private_directory(root)?;
    let root = root.canonicalize().map_err(storage_error)?;
    private_directory(&root)?;
    private_file(&root.join("fabric.db"))?;
    let store = Store::open(root.join("fabric.db")).map_err(storage_error)?;
    Ok((root, store))
}

pub(super) fn private_directory(path: &Path) -> Result<(), FabricError> {
    let meta = fs::symlink_metadata(path).map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(storage_error(
            "Managed directory must be real, current-user-owned and owner-only.",
        ));
    }
    Ok(())
}

pub(super) fn private_file(path: &Path) -> Result<fs::Metadata, FabricError> {
    let meta = fs::symlink_metadata(path).map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
    {
        return Err(storage_error(
            "Managed file must be regular, unlinked, current-user-owned and owner-only.",
        ));
    }
    Ok(meta)
}

fn timestamp(created_at: i64) -> Result<i64, FabricError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(storage_error)?
        .as_secs();
    Ok(i64::try_from(seconds)
        .map_err(storage_error)?
        .max(created_at))
}

fn storage_error(error: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Storage,
        message: error.to_string(),
    }
}

/// Stable reasons are machine-readable; details retain the actionable diagnostic.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct GcItem {
    pub workspace_id: WorkspaceId,
    pub run_id: RunId,
    pub action: String,
    pub reason: String,
    pub detail: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct GcReport {
    pub mode: &'static str,
    pub items: Vec<GcItem>,
    pub unknown_paths: Vec<UnknownCleanupPath>,
}

#[derive(Debug, serde::Serialize)]
pub struct UnknownCleanupPath {
    pub path: String,
    pub reason: &'static str,
}

/// Apply also reconciles interrupted ownership; only verified captured forks qualify for deletion.
pub fn garbage_collect(root: &Path, apply: bool) -> Result<GcReport, FabricError> {
    let (root, mut store, _lease) = cleanup_store(root, apply, false)?;
    if apply {
        leases::reconcile(&root, &mut store)?;
    }
    use std::os::unix::ffi::OsStrExt;
    let ids = store.workspace_ids().map_err(storage_error)?;
    let known: std::collections::HashSet<_> = ids.iter().map(WorkspaceId::as_str).collect();
    let mut items = Vec::new();
    for id in &ids {
        items.push(recover_one(&root, &mut store, id, apply)?);
    }
    let root_directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&root)
        .map_err(storage_error)?;
    let mut unknown_paths = Vec::new();
    if !missing_at(&root_directory, c"fork-staging")? {
        let staging =
            crate::artifacts::open_at(&root_directory, c"fork-staging", libc::O_DIRECTORY)?;
        cleanup_directory(&crate::artifacts::stat(&staging)?)?;
        for name in directory_names(&staging)? {
            if !name.to_str().is_ok_and(|name| known.contains(name)) {
                unknown_paths.push(UnknownCleanupPath {
                    path: root
                        .join("fork-staging")
                        .join(std::ffi::OsStr::from_bytes(name.as_bytes()))
                        .display()
                        .to_string(),
                    reason: "unknown_path",
                });
            }
        }
    }
    Ok(GcReport {
        mode: if apply { "apply" } else { "dry_run" },
        items,
        unknown_paths,
    })
}

#[derive(Debug, serde::Serialize)]
pub struct RecoveryReport {
    pub mode: &'static str,
    pub run_id: RunId,
    pub workspace_id: WorkspaceId,
    pub state: RunState,
    pub workspace_state: WorkspaceState,
    pub action: String,
    pub reason: String,
    pub detail: Option<String>,
    pub artifact_manifest_id: Option<fabric_domain::ArtifactManifestId>,
}

/// Inspect by default. Applying never launches or retries accepted native work.
pub fn recover(root: &Path, id: &RunId, apply: bool) -> Result<RecoveryReport, FabricError> {
    let (root, mut store, _lease) = cleanup_store(root, apply, false)?;
    if apply {
        leases::reconcile(&root, &mut store)?;
    }
    let run = store
        .run(id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Unknown recovery Run"))?;
    let item = recover_one(&root, &mut store, &run.workspace_id, apply)?;
    let run = store.run(id).map_err(storage_error)?.unwrap();
    let workspace = store
        .workspace(&run.workspace_id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Missing recovery workspace"))?;
    Ok(RecoveryReport {
        mode: if apply { "apply" } else { "inspect" },
        run_id: run.run_id,
        workspace_id: workspace.workspace_id,
        state: run.state,
        workspace_state: workspace.state,
        action: match item.action.as_str() {
            "reclaimed" => "reclaimed",
            "would_recover" | "would_reclaim" => "would_recover",
            "skipped" if item.reason == "already_reclaimed" => "already_reclaimed",
            _ => "retained",
        }
        .into(),
        reason: item.reason,
        detail: item.detail,
        artifact_manifest_id: run.artifact_manifest_id,
    })
}

fn recover_one(
    root: &Path,
    store: &mut Store,
    id: &WorkspaceId,
    apply: bool,
) -> Result<GcItem, FabricError> {
    let workspace = store
        .workspace(id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Unknown recovery workspace"))?;
    let run = store
        .run(&workspace.run_id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Missing recovery Run"))?;
    if !matches!(
        workspace.state,
        WorkspaceState::Active | WorkspaceState::RecoveryRequired
    ) || run.artifact_manifest_id.is_some()
        || matches!(
            run.state,
            RunState::Succeeded | RunState::Failed | RunState::Cancelled
        )
    {
        return clean_one(root, store, id, apply, &mut |_| Ok(()));
    }
    let mut item = GcItem {
        workspace_id: id.clone(),
        run_id: run.run_id.clone(),
        action: "refused".into(),
        reason: "recovery_evidence_unavailable".into(),
        detail: None,
    };
    let result: Result<(), FabricError> = (|| {
        let owner = match leases::status(root, store, run.run_id.as_str())? {
            leases::Status::Inactive(guard) => guard,
            leases::Status::Active => {
                item.reason = "worker_owner_active".into();
                return Ok(());
            }
            leases::Status::Unknown | leases::Status::Legacy => {
                item.reason = "worker_owner_unverifiable".into();
                return Ok(());
            }
        };
        if apply
            && (run.state != RunState::NeedsAttention
                || workspace.state != WorkspaceState::RecoveryRequired)
        {
            item.reason = "run_not_recovery_required".into();
            return Ok(());
        }
        item.reason = "unsafe_path_or_owner".into();
        recovery_workspace(root, store, &run, &workspace)?;
        if run.capture.is_none() && store.launch_receipt(&run.run_id).map_err(storage_error)?
            .is_some_and(|receipt|receipt.remote.is_some()) {
            item.reason = "remote_capture_unavailable".into();
            recover_remote_output(root,store,&run,&workspace,apply)?;
            if !apply {
                item.action = "would_recover".into();
                item.reason = "captureable_inactive_workspace".into();
                return Ok(());
            }
            drop(owner);
            item = clean_one(root,store,id,true,&mut |_|Ok(()))?;
            return Ok(());
        }
        item.reason = "target_unavailable".into();
        crate::target::drive(
            store,
            &run.run_id,
            "inspect",
            fabric_runner::target::Action::Inspect,
            serde_json::json!({}),
            |_| {
                validate_owned_workspace(root, &run, &workspace)?;
                Ok((
                    (),
                    serde_json::json!({"owner_inactive":true,"ownership_valid":true}),
                ))
            },
        )?
        .into_result()?;
        let owned = validate_owned_workspace(root, &run, &workspace)?;
        if let Some(installation) = crate::remote::Installation::load(store,&run)? {
            let receipt = crate::remote::recovery_binding(store,&run)?;
            let capture = run.capture.as_ref().ok_or_else(||storage_error("Missing installed capture metadata"))?;
            installation.validate(owned.path().parent().ok_or_else(||storage_error("Missing container"))?,
                &receipt.actual_sha256,capture)?;
        }
        item.reason = if run.capture.is_some() {
            "incomplete_or_invalid_capture"
        } else {
            "capture_metadata_unavailable"
        }
        .into();
        let captured = crate::target::drive(
            store,
            &run.run_id,
            "collect",
            fabric_runner::target::Action::Recover,
            serde_json::json!({"apply":apply}),
            |store| {
                let captured = crate::artifacts::recover_capture(root, store, &run, &owned, apply)?;
                let evidence = serde_json::json!({"captured":captured.is_some(),"applied":apply});
                if apply {
                    let (manifest, artifacts) = captured.as_ref().ok_or_else(|| {
                        storage_error("Recovery did not establish a complete capture")
                    })?;
                    validate_owned_workspace(root, &run, &workspace)?;
                    store
                        .recover_artifacts(&run, &workspace, manifest, artifacts)
                        .map_err(storage_error)?;
                }
                Ok((captured, evidence))
            },
        )?;
        captured.into_result()?;
        if !apply {
            item.action = "would_recover".into();
            item.reason = "captureable_inactive_workspace".into();
            return Ok(());
        }
        // The root guard remains held. Release our run flock before the existing
        // descriptor cleanup engine acquires that same authority independently.
        drop(owner);
        item = clean_one(root, store, id, true, &mut |_| Ok(()))?;
        Ok(())
    })();
    if let Err(failure) = result {
        item.detail = Some(failure.message);
    }
    Ok(item)
}

fn recovery_workspace(root: &Path, store: &Store, run: &Run, workspace: &Workspace) -> Result<RunnerWorkspace, FabricError> {
    match fs::symlink_metadata(&workspace.managed_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        _ => return validate_owned_workspace(root,run,workspace),
    }
    // Only the exact committed install may authorize the tree-rename gap.
    let owned = validate_owned_workspace_tree(root,run,workspace,"transfer-previous")?;
    let receipt = crate::remote::recovery_binding(store,run)?;
    let remote = receipt.remote.as_ref().ok_or_else(||storage_error("Missing remote receipt"))?;
    let result = store.remote_completion(&run.run_id).map_err(storage_error)?
        .ok_or_else(||storage_error("Missing installation completion"))?;
    let completion = crate::remote::completion(result,&remote.resource)?;
    crate::remote::validate_completion(store,run,&completion)?;
    let installation = crate::remote::Installation::load(store,run)?
        .ok_or_else(||storage_error("Unrecorded partial installation"))?;
    installation.validate(owned.path().parent().ok_or_else(||storage_error("Missing container"))?,
        &receipt.actual_sha256,&completion.capture)?;
    Ok(owned)
}

fn recover_remote_output(root: &Path, store: &mut Store, run: &Run, workspace: &Workspace, apply: bool) -> Result<(), FabricError> {
    use crate::remote;
    let owned = recovery_workspace(root,store,run,workspace)?;
    let receipt = remote::recovery_binding(store,run)?;
    let remote = receipt.remote.as_ref().ok_or_else(||remote::error("missing remote receipt"))?;
    let pin_json = store.run_target(&run.run_id).map_err(storage_error)?.ok_or_else(||remote::error("missing target"))?;
    let installation = remote::Installation::load(store,run)?;
    let container = owned.path().parent().ok_or_else(||remote::error("missing container"))?;
    for path in [root.join("artifacts").join(format!("a-{}",run.run_id)),
        root.join("artifact-staging").join(format!("a-{}",run.run_id)),
        container.join("result.transfer.tar"),container.join("remote-return")] {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            _ => return Err(remote::error("partial local capture; retain evidence")),
        }
    }
    if installation.is_none() {
        for name in ["stdout.log","stderr.log","events.jsonl","transfer-previous"] {
            match fs::symlink_metadata(container.join(name)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                _ => return Err(remote::error("unrecorded partial local capture; retain evidence")),
            }
        }
        if fabric_project::prepare::content_digest(owned.path(),std::time::Instant::now()+std::time::Duration::from_secs(1200))? != receipt.actual_sha256 {
            return Err(remote::protocol_error("local source staging changed"));
        }
    }
    let pin: fabric_runner::target::TargetPin = serde_json::from_str(&pin_json).map_err(remote::error)?;
    let target = pin.open(&fabric_runner::CancellationToken::default())?;
    if !target.is_remote() { return Err(remote::protocol_error("remote backend changed")); }
    let reply = target.remote_call("worker.poll",serde_json::json!({"resource":remote.resource}))?;
    if reply["state"] != "complete" { return Err(remote::error("remote inactivity unproven")); }
    let result = remote::completion(reply["result"].clone(),&remote.resource)?;
    remote::validate_completion(store,run,&result)?;
    if let Some(saved) = &installation { saved.validate(container,&receipt.actual_sha256,&result.capture)?; }
    let temporary = std::env::temp_dir();
    let mut staging = remote::download_staged(&target,&remote.resource,if apply {container} else {&temporary},remote::TRANSFER_LIMIT)?;
    let witness = &staging.witness;
    if witness["resource"] != remote.resource || witness["actual_sha256"] != receipt.actual_sha256
        || witness["instance"] != remote.instance || witness["os"] != remote.os
        || witness["arch"] != remote.arch || witness["hostname"] != remote.hostname {
        return Err(remote::protocol_error("retained source witness changed"));
    }
    remote::verify_output(&staging.path,&result.capture)?;
    let mut prompt = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(container.join("prompt.input")).map_err(storage_error)?;
    if !prompt.metadata().map_err(storage_error)?.is_file() { return Err(remote::error("prompt type")); }
    let mut copied = remote::private_file(&staging.path.join("prompt.input"))?;
    if std::io::copy(&mut (&mut prompt).take(remote::TRANSFER_LIMIT+1),&mut copied).map_err(storage_error)? > remote::TRANSFER_LIMIT {
        return Err(remote::error("prompt bound"));
    }
    copied.sync_all().map_err(storage_error)?;
    let staged_workspace = RunnerWorkspace {run_id:run.run_id.clone(),path:staging.path.join("tree")};
    let mut observed = run.clone();
    observed.capture = Some(result.capture.clone());
    crate::artifacts::recover_capture(root,store,&observed,&staged_workspace,false)?;
    // Shutdown must succeed before any durable recovery effect.
    target.finish()?;
    if !apply { return Ok(()); }
    recovery_workspace(root,store,run,workspace)?;
    store.record_remote_completion(&run.run_id,&serde_json::to_value(&result).map_err(storage_error)?).map_err(storage_error)?;
    remote::install_download(&mut staging,container,store,run,&receipt.actual_sha256,&result.capture)?;
    store.record_recovered_capture(run,&result.capture,result.exit_code,result.exit_signal,result.reason()).map_err(storage_error)?;
    let current = store.run(&run.run_id).map_err(storage_error)?.ok_or_else(||remote::error("run disappeared"))?;
    let captured = crate::artifacts::recover_capture(root,store,&current,&owned,true)?
        .ok_or_else(||remote::error("capture not established"))?;
    store.recover_artifacts(&current,workspace,&captured.0,&captured.1).map_err(storage_error)
}

pub(super) fn reclaim_run(root: &Path, id: &RunId) -> Result<GcItem, FabricError> {
    let (root, mut store, _lease) = cleanup_store(root, true, true)?;
    let run = store
        .run(id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Unknown cleanup Run"))?;
    clean_one(&root, &mut store, &run.workspace_id, true, &mut |_| Ok(()))
}

pub(crate) struct CleanupLease(File);

impl Drop for CleanupLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        // A concurrent fork can inherit the open description until exec despite
        // CLOEXEC. Explicit unlock ends our lease without waiting for that child.
        // SAFETY: this guard still owns the descriptor for the entire call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn cleanup_store(
    root: &Path,
    apply: bool,
    wait: bool,
) -> Result<(PathBuf, Store, CleanupLease), FabricError> {
    use std::os::fd::AsRawFd;
    private_directory(root)?;
    let root = root.canonicalize().map_err(storage_error)?;
    private_directory(&root)?;
    let path = root.join("fabric.db");
    private_file(&path)?;
    let recorded = fs::symlink_metadata(&root).map_err(storage_error)?;
    let lease = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&root)
        .map_err(storage_error)?;
    let actual = lease.metadata().map_err(storage_error)?;
    if (recorded.dev(), recorded.ino()) != (actual.dev(), actual.ino()) {
        return Err(storage_error(
            "Cleanup root changed during lease acquisition",
        ));
    }
    // The kernel releases this exclusive cleanup lease even after SIGKILL.
    // Lock the root directory, not fabric.db: Darwin's SQLite locks can conflict.
    if unsafe {
        libc::flock(
            lease.as_raw_fd(),
            libc::LOCK_EX | if wait { 0 } else { libc::LOCK_NB },
        )
    } != 0
    {
        return Err(storage_error(
            "cleanup_lease_busy: another cleaner holds the cleanup lease",
        ));
    }
    let lease = CleanupLease(lease);
    let store = if apply {
        Store::open(path)
    } else {
        Store::open_existing_read_only(path)
    }
    .map_err(storage_error)?;
    Ok((root, store, lease))
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
    birth_seconds: i64,
    birth_nanos: i64,
}

impl Identity {
    fn of(meta: &libc::stat) -> Self {
        Self {
            device: meta.st_dev as u64,
            inode: meta.st_ino,
            #[cfg(target_os = "macos")]
            birth_seconds: meta.st_birthtime,
            #[cfg(target_os = "macos")]
            birth_nanos: meta.st_birthtime_nsec,
            #[cfg(not(target_os = "macos"))]
            birth_seconds: 0,
            #[cfg(not(target_os = "macos"))]
            birth_nanos: 0,
        }
    }
}

#[derive(Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupAuthority {
    version: u32,
    root: Identity,
    staging: Identity,
    container: Identity,
    tree: Identity,
    marker: Identity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupStep {
    Intent,
    Unlinked,
    Deleted,
}

fn clean_one(
    root: &Path,
    store: &mut Store,
    id: &WorkspaceId,
    apply: bool,
    hook: &mut impl FnMut(CleanupStep) -> Result<(), FabricError>,
) -> Result<GcItem, FabricError> {
    // This fresh read is deliberately inside the exclusive cleanup lease.
    let workspace = store
        .workspace(id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Unknown workspace"))?;
    let mut report = GcItem {
        workspace_id: id.clone(),
        run_id: workspace.run_id.clone(),
        action: "refused".into(),
        reason: "unsafe_workspace".into(),
        detail: None,
    };
    let result: Result<(&str, &str), FabricError> = (|| {
        let now = timestamp(0)?;
        if workspace.state == WorkspaceState::Reclaimed {
            let container = fork_path(root, id).parent().unwrap().to_path_buf();
            if fs::symlink_metadata(container).is_ok() {
                return Ok(("refused", "reclaimed_path_reappeared"));
            }
            return Ok(("skipped", "already_reclaimed"));
        }
        if workspace
            .lease_expires_at
            .is_some_and(|expiry| expiry > now)
        {
            return Ok(("refused", "worker_lease_valid"));
        }
        let _owner = match leases::status(root, store, workspace.run_id.as_str())? {
            leases::Status::Active => return Ok(("refused", "worker_owner_active")),
            leases::Status::Unknown | leases::Status::Legacy => {
                return Ok(("refused", "worker_owner_unverifiable"));
            }
            leases::Status::Inactive(guard) => guard,
        };
        if workspace.state != WorkspaceState::CleanupPending || workspace.retention_reason.is_some()
        {
            return Ok(("refused", "workspace_not_cleanup_pending"));
        }
        let run = store
            .run(&workspace.run_id)
            .map_err(storage_error)?
            .ok_or_else(|| storage_error("Missing cleanup Run"))?;
        if !matches!(
            run.state,
            RunState::Succeeded | RunState::Failed | RunState::Cancelled | RunState::NeedsAttention
        ) {
            return Ok(("refused", "run_not_terminal"));
        }
        if run.workspace_id != workspace.workspace_id
            || run.prepared_state_id != workspace.state_id
            || Path::new(&workspace.managed_path) != fork_path(root, id)
        {
            return Ok(("refused", "ownership_or_path_mismatch"));
        }
        report.reason = "invalid_or_missing_manifest".into();
        crate::artifacts::verify_registered(root, store, &run)?;
        let saved = store.cleanup_intent(id).map_err(storage_error)?;
        let prior: Option<CleanupAuthority> = saved
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(storage_error)?;
        report.reason = "unsafe_path_or_owner".into();
        let opened = open_cleanup(root, &workspace, prior.as_ref())?;
        let authority = serde_json::to_string(&opened.authority).map_err(storage_error)?;
        if !apply {
            return Ok((
                "would_reclaim",
                if saved.is_some() {
                    "resume_cleanup"
                } else {
                    "verified_capture"
                },
            ));
        }
        report.reason = "cleanup_intent_conflict".into();
        store
            .record_cleanup_intent(&run, &workspace, &authority, timestamp(0)?)
            .map_err(storage_error)?;
        report.reason = "cleanup_interrupted".into();
        hook(CleanupStep::Intent)?;
        report.reason = "target_unavailable".into();
        let committed = crate::target::drive(
            store,
            &run.run_id,
            "reclaim",
            fabric_runner::target::Action::Reclaim,
            serde_json::json!({"cleanup_intent":true}),
            |store| {
                report.reason = "cleanup_interrupted".into();
                opened.delete(id, hook)?;
                hook(CleanupStep::Deleted)?;
                store
                    .finish_cleanup(id, &authority)
                    .map_err(storage_error)?;
                Ok(((), serde_json::json!({"removed":true})))
            },
        )?;
        if let Some(failure) = committed.failure {
            report.detail = Some(failure.message);
            return Ok(("reclaimed", "target_protocol_failed"));
        }
        Ok(("reclaimed", "verified_capture"))
    })();
    match result {
        Ok((action, reason)) => {
            report.action = action.into();
            report.reason = reason.into();
        }
        Err(failure) => {
            if apply && report.reason == "target_unavailable" {
                store
                    .record_target_failure(&workspace.run_id)
                    .map_err(storage_error)?;
                report.reason = "target_protocol_failed".into();
            }
            report.detail = Some(failure.message);
        }
    }
    Ok(report)
}

struct OpenCleanup {
    authority: CleanupAuthority,
    staging: File,
    container: Option<File>,
    tree: Option<File>,
}

fn open_cleanup(
    root: &Path,
    workspace: &Workspace,
    prior: Option<&CleanupAuthority>,
) -> Result<OpenCleanup, FabricError> {
    open_owned_cleanup(
        root,
        c"fork-staging",
        &std::ffi::CString::new(workspace.workspace_id.as_str()).map_err(storage_error)?,
        &marker_contents(root, workspace),
        prior,
    )
}

fn open_owned_cleanup(
    root: &Path,
    staging_name: &std::ffi::CStr,
    name: &std::ffi::CStr,
    expected_marker: &str,
    prior: Option<&CleanupAuthority>,
) -> Result<OpenCleanup, FabricError> {
    use crate::artifacts::{open_at, stat, stat_at};
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(root)
        .map_err(storage_error)?;
    let root_meta = stat(&directory)?;
    cleanup_directory(&root_meta)?;
    let staging = open_at(&directory, staging_name, libc::O_DIRECTORY)?;
    let staging_meta = stat(&staging)?;
    cleanup_directory(&staging_meta)?;
    if let Some(saved) = prior {
        if saved.version != 1
            || saved.root != Identity::of(&root_meta)
            || saved.staging != Identity::of(&staging_meta)
        {
            return Err(storage_error("Cleanup root identity changed"));
        }
    }
    if missing_at(&staging, name)? {
        return prior
            .map(|authority| OpenCleanup {
                authority: CleanupAuthority {
                    version: authority.version,
                    root: authority.root.clone(),
                    staging: authority.staging.clone(),
                    container: authority.container.clone(),
                    tree: authority.tree.clone(),
                    marker: authority.marker.clone(),
                },
                staging,
                container: None,
                tree: None,
            })
            .ok_or_else(|| storage_error("Missing container without durable cleanup intent"));
    }
    let container = open_at(&staging, name, libc::O_DIRECTORY)?;
    let container_meta = stat(&container)?;
    cleanup_directory(&container_meta)?;
    if root_meta.st_dev != staging_meta.st_dev || root_meta.st_dev != container_meta.st_dev {
        return Err(storage_error(
            "Cleanup refuses mounted staging or container",
        ));
    }
    let tree = if missing_at(&container, c"tree")? {
        if prior.is_none() {
            return Err(storage_error(
                "Missing workspace tree without durable cleanup intent",
            ));
        }
        None
    } else {
        let tree = open_at(&container, c"tree", libc::O_DIRECTORY)?;
        let metadata = stat(&tree)?;
        cleanup_directory(&metadata)?;
        if metadata.st_dev != root_meta.st_dev {
            return Err(storage_error("Cleanup refuses mounted workspace tree"));
        }
        Some(tree)
    };
    let marker = if missing_at(&container, c".fabric-owner")? {
        if prior.is_none() {
            return Err(storage_error("Missing workspace ownership marker"));
        }
        None
    } else {
        let recorded = stat_at(&container, c".fabric-owner")?;
        let mut marker = open_at(&container, c".fabric-owner", libc::O_NONBLOCK)?;
        let meta = stat(&marker)?;
        let expected = expected_marker;
        if Identity::of(&recorded) != Identity::of(&meta)
            || meta.st_mode & libc::S_IFMT != libc::S_IFREG
            || meta.st_uid != unsafe { libc::geteuid() }
            || meta.st_nlink != 1
            || meta.st_mode & 0o077 != 0
            || meta.st_size != expected.len() as i64
        {
            return Err(storage_error("Invalid workspace ownership marker"));
        }
        let mut bytes = vec![0; expected.len()];
        marker.read_exact(&mut bytes).map_err(storage_error)?;
        if bytes != expected.as_bytes() {
            return Err(storage_error(
                "Workspace ownership marker does not match durable owner",
            ));
        }
        Some(Identity::of(&meta))
    };
    let authority = CleanupAuthority {
        version: 1,
        root: Identity::of(&root_meta),
        staging: Identity::of(&staging_meta),
        container: Identity::of(&container_meta),
        tree: tree
            .as_ref()
            .map(|file| stat(file).map(|meta| Identity::of(&meta)))
            .transpose()?
            .or_else(|| prior.map(|saved| saved.tree.clone()))
            .ok_or_else(|| storage_error("Missing tree authority"))?,
        marker: marker
            .or_else(|| prior.map(|saved| saved.marker.clone()))
            .ok_or_else(|| storage_error("Missing marker authority"))?,
    };
    if prior.is_some_and(|saved| saved != &authority) {
        return Err(storage_error("Workspace substitution after cleanup intent"));
    }
    Ok(OpenCleanup {
        authority,
        staging,
        container: Some(container),
        tree,
    })
}

fn cleanup_directory(meta: &libc::stat) -> Result<(), FabricError> {
    if meta.st_mode & libc::S_IFMT != libc::S_IFDIR
        || meta.st_uid != unsafe { libc::geteuid() }
        || meta.st_mode & 0o077 != 0
    {
        return Err(storage_error(
            "Cleanup container/tree must be real, private and owned",
        ));
    }
    Ok(())
}

fn missing_at(parent: &File, name: &std::ffi::CStr) -> Result<bool, FabricError> {
    use std::os::fd::AsRawFd;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: live descriptor, one NUL-terminated component and valid output storage.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Ok(false);
    }
    let failure = std::io::Error::last_os_error();
    if failure.kind() == std::io::ErrorKind::NotFound {
        Ok(true)
    } else {
        Err(storage_error(failure))
    }
}

impl OpenCleanup {
    fn delete(
        self,
        id: &WorkspaceId,
        hook: &mut impl FnMut(CleanupStep) -> Result<(), FabricError>,
    ) -> Result<(), FabricError> {
        self.delete_named(
            &std::ffi::CString::new(id.as_str()).map_err(storage_error)?,
            hook,
        )
    }

    fn delete_named(
        self,
        name: &std::ffi::CStr,
        hook: &mut impl FnMut(CleanupStep) -> Result<(), FabricError>,
    ) -> Result<(), FabricError> {
        use crate::artifacts::{stat, stat_at};
        if let Some(container) = &self.container {
            check_identity(&stat_at(&self.staging, name)?, &self.authority.container)?;
            // Prepared containers are sealed read-only. Authority is already durable.
            use std::os::fd::AsRawFd;
            if unsafe { libc::fchmod(container.as_raw_fd(), 0o700) } != 0 {
                return Err(storage_error(std::io::Error::last_os_error()));
            }
            if let Some(tree) = &self.tree {
                check_identity(&stat_at(container, c"tree")?, &self.authority.tree)?;
                delete_contents(tree, self.authority.root.device, hook)?;
                check_identity(&stat_at(container, c"tree")?, &Identity::of(&stat(tree)?))?;
                unlink_at(container, c"tree", true)?;
                hook(CleanupStep::Unlinked)?;
            }
            // The marker is removed last, but durable intent also authorizes a restart
            // after its unlink. Any replacement container/tree is still refused.
            delete_contents(container, self.authority.root.device, hook)?;
            check_identity(&stat_at(&self.staging, name)?, &self.authority.container)?;
            unlink_at(&self.staging, name, true)?;
            hook(CleanupStep::Unlinked)?;
        }
        if !missing_at(&self.staging, name)? {
            return Err(storage_error(
                "Cleanup container reappeared during deletion",
            ));
        }
        self.staging.sync_all().map_err(storage_error)
    }
}

fn check_identity(meta: &libc::stat, expected: &Identity) -> Result<(), FabricError> {
    if &Identity::of(meta) != expected {
        return Err(storage_error("Cleanup entry identity changed"));
    }
    Ok(())
}

fn delete_contents(
    directory: &File,
    device: u64,
    hook: &mut impl FnMut(CleanupStep) -> Result<(), FabricError>,
) -> Result<(), FabricError> {
    use crate::artifacts::{open_at, stat, stat_at};
    use std::os::fd::AsRawFd;
    // Only after durable intent: allow deletion of read-only prepared dependencies.
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(storage_error(std::io::Error::last_os_error()));
    }
    for name in directory_names(directory)? {
        let before = stat_at(directory, &name)?;
        if Identity::of(&before).device != device || before.st_uid != unsafe { libc::geteuid() } {
            return Err(storage_error(
                "Cleanup refuses foreign ownership or mounted filesystem",
            ));
        }
        let is_directory = before.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if is_directory {
            let child = open_at(directory, &name, libc::O_DIRECTORY)?;
            check_identity(&stat(&child)?, &Identity::of(&before))?;
            delete_contents(&child, device, hook)?;
        }
        check_identity(&stat_at(directory, &name)?, &Identity::of(&before))?;
        // unlinkat never dereferences symlinks, including absolute internal links.
        unlink_at(directory, &name, is_directory)?;
        hook(CleanupStep::Unlinked)?;
    }
    directory.sync_all().map_err(storage_error)
}

fn unlink_at(parent: &File, name: &std::ffi::CStr, directory: bool) -> Result<(), FabricError> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor pins the parent; name is one validated directory entry.
    if unsafe {
        libc::unlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            if directory { libc::AT_REMOVEDIR } else { 0 },
        )
    } != 0
    {
        return Err(storage_error(std::io::Error::last_os_error()));
    }
    parent.sync_all().map_err(storage_error)
}

fn directory_names(directory: &File) -> Result<Vec<std::ffi::CString>, FabricError> {
    use std::ffi::{CStr, CString};
    use std::os::fd::{AsRawFd, IntoRawFd};
    let duplicate = crate::artifacts::open_at(directory, c".", libc::O_DIRECTORY)?;
    // SAFETY: fdopendir takes ownership only on success.
    let stream = unsafe { libc::fdopendir(duplicate.as_raw_fd()) };
    if stream.is_null() {
        return Err(storage_error(std::io::Error::last_os_error()));
    }
    let _ = duplicate.into_raw_fd();
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: this stream exclusively owns the directory descriptor.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Stream(stream);
    let mut names = Vec::new();
    loop {
        #[cfg(target_os = "macos")]
        let errno = unsafe { libc::__error() };
        #[cfg(not(target_os = "macos"))]
        let errno = unsafe { libc::__errno_location() };
        // SAFETY: errno is thread-local; readdir's name lives until the next call.
        unsafe {
            *errno = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if unsafe { *errno } != 0 {
                return Err(storage_error(std::io::Error::last_os_error()));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if ![b".".as_slice(), b".."].contains(&name.to_bytes()) {
            names.push(CString::from(name));
        }
    }
    // Keep the marker until other container entries have been removed.
    names.sort_by(|a, b| {
        (a.as_bytes() == b".fabric-owner", a.as_bytes())
            .cmp(&(b.as_bytes() == b".fabric-owner", b.as_bytes()))
    });
    Ok(names)
}

/// Environment storage uses the same descriptor-anchored ownership and deletion
/// interpreter as disposable workspaces, but never enters normal run reclamation.
pub(crate) struct EnvironmentResource {
    cleanup: OpenCleanup,
    name: std::ffi::CString,
    home_roots: Option<[Identity; 5]>,
}

impl EnvironmentResource {
    pub(crate) fn authority(&self) -> Result<String, FabricError> {
        serde_json::to_string(&(&self.cleanup.authority, self.home_identities()?))
            .map_err(storage_error)
    }

    pub(crate) fn absent(&self) -> bool {
        self.cleanup.container.is_none()
    }

    pub(crate) fn validate_home(&self) -> Result<(), FabricError> {
        let observed = self.home_identities()?;
        if self
            .home_roots
            .as_ref()
            .is_some_and(|saved| saved != &observed)
        {
            return Err(storage_error(
                "Environment config root was replaced; ownership denied",
            ));
        }
        Ok(())
    }

    fn home_identities(&self) -> Result<[Identity; 5], FabricError> {
        use crate::artifacts::{open_at, stat};
        let tree = self
            .cleanup
            .tree
            .as_ref()
            .ok_or_else(|| storage_error("Environment home is absent"))?;
        let device = stat(tree)?.st_dev;
        let validate = |directory: &File| {
            let metadata = stat(directory)?;
            cleanup_directory(&metadata)?;
            if metadata.st_dev != device {
                return Err(storage_error(
                    "Environment config root crosses a device boundary",
                ));
            }
            Ok(Identity::of(&metadata))
        };
        let config = validate(&open_at(tree, c".config", libc::O_DIRECTORY)?)?;
        let cache = validate(&open_at(tree, c".cache", libc::O_DIRECTORY)?)?;
        let local = open_at(tree, c".local", libc::O_DIRECTORY)?;
        let local_identity = validate(&local)?;
        let share = validate(&open_at(&local, c"share", libc::O_DIRECTORY)?)?;
        let state = validate(&open_at(&local, c"state", libc::O_DIRECTORY)?)?;
        Ok([config, cache, local_identity, share, state])
    }

    pub(crate) fn destroy(self) -> Result<(), FabricError> {
        self.cleanup.delete_named(&self.name, &mut |_| Ok(()))
    }
}

pub(crate) fn environment_resource(
    root: &Path,
    id: &fabric_domain::EnvironmentId,
    marker: &str,
    saved: Option<&str>,
    destroying: bool,
) -> Result<EnvironmentResource, FabricError> {
    let saved: Option<(CleanupAuthority, [Identity; 5])> = saved
        .map(serde_json::from_str)
        .transpose()
        .map_err(storage_error)?;
    let (authority, home_roots) = saved.unzip();
    let name = std::ffi::CString::new(id.as_str()).map_err(storage_error)?;
    let cleanup = open_owned_cleanup(
        root,
        c"environments",
        &name,
        marker,
        if destroying { authority.as_ref() } else { None },
    )?;
    if authority
        .as_ref()
        .is_some_and(|saved| saved != &cleanup.authority)
    {
        return Err(storage_error(
            "Environment resource was replaced; ownership denied",
        ));
    }
    Ok(EnvironmentResource {
        cleanup,
        name,
        home_roots,
    })
}

pub(crate) fn environment_lock(root: &Path, exclusive: bool) -> Result<CleanupLease, FabricError> {
    use std::os::fd::AsRawFd;
    let path = root.join("environments");
    private_directory(&path)?;
    let before = fs::symlink_metadata(&path).map_err(storage_error)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(&path)
        .map_err(storage_error)?;
    let after = file.metadata().map_err(storage_error)?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(storage_error(
            "Environment namespace changed while acquiring lease",
        ));
    }
    // ponytail: one namespace lock, shared by runs/inspection; lifecycle mutations
    // serialize and fail closed while any environment run holds a shared lease.
    if unsafe {
        libc::flock(
            file.as_raw_fd(),
            (if exclusive {
                libc::LOCK_EX
            } else {
                libc::LOCK_SH
            }) | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(storage_error(
            "environment_lease_busy: environment is in active use",
        ));
    }
    Ok(CleanupLease(file))
}

pub(crate) fn environment_inactive(
    root: &Path,
    store: &Store,
    id: &fabric_domain::EnvironmentId,
) -> Result<Vec<CleanupLease>, FabricError> {
    let mut guards = Vec::new();
    for id in store.environment_runs(id).map_err(storage_error)? {
        let run = store
            .run(&id)
            .map_err(storage_error)?
            .ok_or_else(|| storage_error("Missing environment Run ownership"))?;
        if !run.state.is_terminal() {
            return Err(storage_error(
                "environment_active_or_unknown: reconcile/cancel its runs first",
            ));
        }
        match leases::status(root, store, id.as_str())? {
            leases::Status::Inactive(guard) => guards.push(guard),
            _ => {
                return Err(storage_error(
                    "environment_lease_unknown_or_active: expiry is not ownership proof",
                ));
            }
        }
    }
    Ok(guards)
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use fabric_runner::FakeHarness;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    pub(super) struct Fixture {
        pub(super) temp: tempfile::TempDir,
        pub(super) root: PathBuf,
        pub(super) id: WorkspaceId,
    }
    impl Fixture {
        pub(super) fn captured() -> Self {
            Self::capture(false)
        }

        fn capture(interrupt_association: bool) -> Self {
            Self::capture_with_target(interrupt_association, crate::target::reference_pin())
        }

        fn capture_with_target(
            interrupt_association: bool,
            pin: fabric_runner::target::TargetPin,
        ) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            let git = |args: &[&str]| {
                let output = Command::new("/usr/bin/git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_AUTHOR_NAME", "Test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                    .env("GIT_COMMITTER_NAME", "Test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
            };
            git(&["init", "--quiet"]);
            fs::write(repo.join("tracked"), b"base").unwrap();
            git(&["add", "."]);
            git(&["commit", "--quiet", "-m", "initial"]);
            let root = temp.path().join("storage");
            fabric_project::register(&root, "fixture", &repo).unwrap();
            let recipe = temp.path().join("recipe.toml");
            fs::write(&recipe, "version=1\n[[steps]]\nargv=['/usr/bin/true']\n").unwrap();
            fabric_project::prepare::prepare(&root, "fixture", &recipe).unwrap();
            let fake = temp.path().join("fake");
            fs::write(&fake, "#!/bin/sh\numask 077\nif [ \"$1\" = '--version' ]; then printf 'fabric-fake-harness 1\\n'; exit 0; fi\nprintf changed > tracked\nprintf unique > new-source\nprintf 'worker log\\n'\n").unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
            let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
            db.execute_batch("CREATE TRIGGER test_lease AFTER UPDATE OF state ON workspaces
                WHEN NEW.state='CLEANUP_PENDING' BEGIN
                UPDATE workspaces SET lease_expires_at=9223372036854775807 WHERE workspace_id=NEW.workspace_id; END;").unwrap();
            if interrupt_association {
                db.execute_batch(
                    "CREATE TRIGGER test_association BEFORE UPDATE OF artifact_manifest_id ON runs
                     WHEN NEW.artifact_manifest_id IS NOT NULL
                     BEGIN SELECT RAISE(ABORT, 'injected artifact association interruption'); END;",
                )
                .unwrap();
            }
            drop(db);
            let prompt = temp.path().join("prompt");
            fs::write(&prompt, b"prompt").unwrap();
            let result = crate::run::foreground(
                &root,
                "fixture",
                &prompt,
                &FakeHarness::new(fake).unwrap(),
                &crate::run::RunOptions {
                    copy_limit_bytes: 1024 * 1024,
                    harness_request: fabric_domain::HarnessRequest {
                        placement: pin.placement.clone(),
                        ..fabric_domain::HarnessRequest::default()
                    },
                    target_pin: Some(pin),
                    ..crate::run::RunOptions::default()
                },
            );
            let id = if interrupt_association {
                let error = result.err().expect("association must fail");
                assert!(
                    error
                        .to_string()
                        .contains("injected artifact association interruption"),
                    "{error}"
                );
                let (_, store) = open_store(&root).unwrap();
                let ids = store.workspace_ids().unwrap();
                assert_eq!(ids.len(), 1);
                ids.into_iter().next().unwrap()
            } else {
                let report = result.unwrap();
                assert_eq!(
                    report.cleanup.as_ref().unwrap().reason,
                    "worker_lease_valid"
                );
                report.workspace_id.parse().unwrap()
            };
            let db = rusqlite::Connection::open(root.join("fabric.db")).unwrap();
            db.execute_batch(
                "DROP TRIGGER test_lease; DROP TRIGGER IF EXISTS test_association;
                 UPDATE workspaces SET lease_expires_at=NULL;",
            )
            .unwrap();
            Self {
                temp,
                root: root.canonicalize().unwrap(),
                id,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fn writable(path: &Path) {
                if fs::symlink_metadata(path).unwrap().is_dir() {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
                    for entry in fs::read_dir(path).unwrap() {
                        writable(&entry.unwrap().path());
                    }
                }
            }
            writable(self.temp.path());
        }
    }

    #[test]
    fn dry_run_and_owner_mismatch_never_dispatch_target_reclaim() {
        let target_home = tempfile::tempdir().unwrap();
        let pid = target_home.path().join("pid");
        let pin = fabric_runner::target::TargetPin::from_manifest(serde_json::from_value(serde_json::json!({
            "id":"external-target","version":"1.0.0","kind":"target","protocol":{"min":1,"max":1},
            "executable":"/usr/bin/python3",
            "args":[concat!(env!("CARGO_MANIFEST_DIR"),"/../fabric-runner/tests/fixtures/plugin.py"),
                "target","concrete",pid],
            "capabilities":["target.execute"]
        })).unwrap(), "external-target").unwrap();
        let fixture = Fixture::capture_with_target(false, pin);
        let calls = pid.with_extension("calls");
        fs::write(&calls, "").unwrap();
        let tree = fork_path(&fixture.root, &fixture.id);
        let before = fs::read(tree.join("tracked")).unwrap();
        let dry = garbage_collect(&fixture.root, false).unwrap();
        assert_eq!(dry.items[0].action, "would_reclaim");
        assert_eq!(fs::read(tree.join("tracked")).unwrap(), before);
        assert!(fs::read_to_string(&calls).unwrap().is_empty());
        let (_, store) = open_store(&fixture.root).unwrap();
        assert!(store.cleanup_intent(&fixture.id).unwrap().is_none());
        drop(store);
        fs::remove_file(tree.parent().unwrap().join(".fabric-owner")).unwrap();
        let refused = garbage_collect(&fixture.root, true).unwrap();
        assert_eq!(refused.items[0].reason, "unsafe_path_or_owner");
        assert!(fs::read_to_string(&calls).unwrap().is_empty());
        assert_eq!(fs::read(tree.join("tracked")).unwrap(), before);
    }

    #[test]
    fn gc_recovers_promoted_sealed_capture_before_database_association() {
        for boundary in 0..3 {
            recover_promoted_capture(boundary);
        }
    }

    fn recover_promoted_capture(boundary: u8) {
        use sha2::{Digest, Sha256};

        // Abort only the association transaction, after the real capture has been
        // promoted and sealed. This is a durable-boundary fault, not a SIGKILL.
        let fixture = Fixture::capture(true);
        let (_, store) = open_store(&fixture.root).unwrap();
        let workspace = store.workspace(&fixture.id).unwrap().unwrap();
        let before = store.run(&workspace.run_id).unwrap().unwrap();
        let attempts = store.worker_attempts(&before.run_id).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(before.state, RunState::NeedsAttention);
        assert_eq!(workspace.state, WorkspaceState::RecoveryRequired);
        assert_eq!(before.exit_code, Some(0));
        assert!(before.capture.is_some());
        assert!(before.artifact_manifest_id.is_none());
        assert!(store.artifacts(&before.run_id).unwrap().is_empty());
        let manifest_id = format!("a-{}", before.run_id);
        let final_path = fixture.root.join("artifacts").join(&manifest_id);
        assert_eq!(
            fs::metadata(&final_path).unwrap().permissions().mode() & 0o777,
            0o500
        );
        assert!(
            !fixture
                .root
                .join("artifact-staging")
                .join(&manifest_id)
                .exists()
        );
        let manifest_bytes = fs::read(final_path.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
        let mut expected: Vec<fabric_store::Artifact> =
            serde_json::from_value(manifest["files"].clone()).unwrap();
        expected.push(fabric_store::Artifact {
            kind: "manifest".into(),
            relative_path: format!("artifacts/{manifest_id}/manifest.json"),
            sha256: format!("{:x}", Sha256::digest(&manifest_bytes)),
            size_bytes: manifest_bytes.len() as u64,
        });
        expected.sort_by(|left, right| left.kind.cmp(&right.kind));
        assert_eq!(expected.len(), 14);
        let evidence = expected
            .iter()
            .map(|artifact| {
                let bytes = fs::read(fixture.root.join(&artifact.relative_path)).unwrap();
                assert_eq!(bytes.len() as u64, artifact.size_bytes);
                assert_eq!(format!("{:x}", Sha256::digest(&bytes)), artifact.sha256);
                (artifact.relative_path.clone(), bytes)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            fs::read(final_path.join("stdout.log")).unwrap(),
            b"worker log\n"
        );
        assert_eq!(
            fs::read(final_path.join("prompt.input")).unwrap(),
            b"prompt"
        );
        assert_eq!(
            manifest["profile"]["digest"].as_str().unwrap(),
            attempts[0].profile_digest.as_deref().unwrap()
        );
        if boundary != 0 {
            fs::set_permissions(&final_path, fs::Permissions::from_mode(0o700)).unwrap();
            if boundary == 2 {
                for artifact in &expected {
                    fs::set_permissions(
                        fixture.root.join(&artifact.relative_path),
                        fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                }
                let stage = fixture.root.join("artifact-staging").join(&manifest_id);
                fs::rename(&final_path, &stage).unwrap();
                File::open(stage).unwrap().sync_all().unwrap();
                File::open(fixture.root.join("artifact-staging"))
                    .unwrap()
                    .sync_all()
                    .unwrap();
            }
            File::open(fixture.root.join("artifacts"))
                .unwrap()
                .sync_all()
                .unwrap();
        }
        let inspection = recover(&fixture.root, &before.run_id, false).unwrap();
        assert_eq!(inspection.action, "would_recover", "{inspection:?}");
        assert_eq!(store.run(&before.run_id).unwrap().unwrap(), before);
        drop(store);

        let report = garbage_collect(&fixture.root, true).unwrap();
        let item = report
            .items
            .iter()
            .find(|item| item.workspace_id == fixture.id)
            .unwrap();
        assert_eq!(item.action, "reclaimed", "{item:?}");
        let (_, store) = open_store(&fixture.root).unwrap();
        let recovered = store.run(&before.run_id).unwrap().unwrap();
        let mut unchanged = before.clone();
        unchanged.artifact_manifest_id = Some(manifest_id.parse().unwrap());
        unchanged.updated_at = recovered.updated_at;
        assert_eq!(recovered, unchanged);
        assert_eq!(store.worker_attempts(&before.run_id).unwrap(), attempts);
        let mut registered = store.artifacts(&before.run_id).unwrap();
        registered.sort_by(|left, right| left.kind.cmp(&right.kind));
        assert_eq!(registered, expected);
        assert_eq!(
            store.workspace(&fixture.id).unwrap().unwrap().state,
            WorkspaceState::Reclaimed
        );
        assert!(
            !Path::new(&workspace.managed_path)
                .parent()
                .unwrap()
                .exists()
        );
        crate::artifacts::verify_registered(&fixture.root, &store, &recovered).unwrap();
        drop(store);

        for _ in 0..2 {
            let report = garbage_collect(&fixture.root, true).unwrap();
            let item = report
                .items
                .iter()
                .find(|item| item.workspace_id == fixture.id)
                .unwrap();
            assert_eq!(item.reason, "already_reclaimed");
            let (_, store) = open_store(&fixture.root).unwrap();
            assert_eq!(store.run(&before.run_id).unwrap().unwrap(), recovered);
            let mut registered = store.artifacts(&before.run_id).unwrap();
            registered.sort_by(|left, right| left.kind.cmp(&right.kind));
            assert_eq!(registered, expected);
            assert_eq!(store.worker_attempts(&before.run_id).unwrap(), attempts);
            for (relative_path, bytes) in &evidence {
                assert_eq!(fs::read(fixture.root.join(relative_path)).unwrap(), *bytes);
            }
            crate::artifacts::verify_registered(&fixture.root, &store, &recovered).unwrap();
        }
    }

    #[test]
    fn interrupted_cleanup_resumes_after_intent_partial_unlink_marker_and_container_removal() {
        for stage in 0..4 {
            let fixture = Fixture::captured();
            let container = fork_path(&fixture.root, &fixture.id)
                .parent()
                .unwrap()
                .to_path_buf();
            let (_, mut store, lease) = cleanup_store(&fixture.root, true, false).unwrap();
            let interrupted =
                clean_one(&fixture.root, &mut store, &fixture.id, true, &mut |step| {
                    let stop = match stage {
                        0 => step == CleanupStep::Intent,
                        1 => step == CleanupStep::Unlinked,
                        2 => {
                            step == CleanupStep::Unlinked
                                && container.exists()
                                && !container.join(".fabric-owner").exists()
                        }
                        _ => step == CleanupStep::Deleted,
                    };
                    if stop {
                        Err(storage_error("injected cleanup interruption"))
                    } else {
                        Ok(())
                    }
                })
                .unwrap();
            assert_eq!(
                interrupted.reason, "cleanup_interrupted",
                "{stage}: {interrupted:?}"
            );
            assert!(store.cleanup_intent(&fixture.id).unwrap().is_some());
            assert_eq!(
                store.workspace(&fixture.id).unwrap().unwrap().state,
                WorkspaceState::CleanupPending
            );
            drop(store);
            drop(lease);
            let dry = garbage_collect(&fixture.root, false).unwrap();
            assert_eq!(dry.items[0].reason, "resume_cleanup", "{stage}: {dry:?}");
            let completed = garbage_collect(&fixture.root, true).unwrap();
            assert_eq!(
                completed.items[0].action, "reclaimed",
                "{stage}: {completed:?}"
            );
            assert!(!container.exists());
            assert_eq!(
                garbage_collect(&fixture.root, true).unwrap().items[0].reason,
                "already_reclaimed"
            );
        }
    }

    #[test]
    fn durable_intent_refuses_replaced_container_tree_or_marker() {
        for component in ["container", "tree", ".fabric-owner"] {
            let fixture = Fixture::captured();
            let container = fork_path(&fixture.root, &fixture.id)
                .parent()
                .unwrap()
                .to_path_buf();
            let (_, mut store, lease) = cleanup_store(&fixture.root, true, false).unwrap();
            let report = clean_one(&fixture.root, &mut store, &fixture.id, true, &mut |_| {
                Err(storage_error("interrupted after intent"))
            })
            .unwrap();
            assert_eq!(report.reason, "cleanup_interrupted");
            drop(store);
            drop(lease);
            let original = match component {
                "container" => container.clone(),
                name => container.join(name),
            };
            fs::rename(&original, fixture.temp.path().join("saved-component")).unwrap();
            if component == ".fabric-owner" {
                fs::copy(fixture.temp.path().join("saved-component"), &original).unwrap();
            } else {
                fs::create_dir(&original).unwrap();
            }
            fs::set_permissions(
                &original,
                fs::Permissions::from_mode(if component == ".fabric-owner" {
                    0o600
                } else {
                    0o700
                }),
            )
            .unwrap();
            let report = garbage_collect(&fixture.root, true).unwrap();
            assert_eq!(report.items[0].action, "refused", "{component}: {report:?}");
            assert!(original.exists());
            assert!(fixture.temp.path().join("saved-component").exists());
        }
    }

    #[test]
    fn releasing_cleanup_lease_does_not_wait_for_inherited_child_descriptors() {
        use std::io::Write;
        use std::os::fd::AsRawFd;

        let fixture = Fixture::captured();
        let (_, store, lease) = cleanup_store(&fixture.root, true, false).unwrap();
        drop(store);
        let (read, mut write) = std::io::pipe().unwrap();
        let pipe = [read.as_raw_fd(), write.as_raw_fd()];
        // SAFETY: the child uses only async-signal-safe syscalls before _exit.
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            // SAFETY: these are inherited pipe descriptors; no Rust state or
            // allocator is touched in the forked child.
            unsafe {
                libc::close(pipe[1]);
                let mut byte = 0_u8;
                while libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1) < 0 {}
                libc::_exit(0);
            }
        }
        drop(read);
        drop(lease);
        // The child remains blocked with its inherited lease FD until this
        // collection completes. Closing our FD alone cannot release flock.
        let report = garbage_collect(&fixture.root, true);
        let mut status = 0;
        // SAFETY: child is the positive PID returned by our own fork.
        let waiting = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
        let wake = write.write_all(&[1]);
        drop(write);
        // SAFETY: reap only the child we created; the gate byte wakes it.
        let reaped = unsafe { libc::waitpid(child, &mut status, 0) };
        assert_eq!(
            waiting, 0,
            "child must still hold its inherited descriptors"
        );
        assert_eq!(reaped, child);
        wake.unwrap();
        assert_eq!(status, 0);
        assert_eq!(report.unwrap().items[0].action, "reclaimed");
    }

    #[test]
    fn concurrent_cleaner_cannot_take_exclusive_cleanup_lease() {
        let fixture = Fixture::captured();
        let (_, store, lease) = cleanup_store(&fixture.root, true, false).unwrap();
        let root = fixture.root.clone();
        let error = std::thread::spawn(move || garbage_collect(&root, true).unwrap_err())
            .join()
            .unwrap();
        assert!(error.message.contains("cleanup_lease_busy"), "{error:?}");
        assert!(fork_path(&fixture.root, &fixture.id).exists());
        drop(store);
        drop(lease);
        let barrier = std::sync::Barrier::new(2);
        let reports = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = &barrier;
                    let root = &fixture.root;
                    scope.spawn(move || {
                        barrier.wait();
                        garbage_collect(root, true)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut reclaimed = 0;
        for report in reports {
            match report {
                Ok(report) => reclaimed += usize::from(report.items[0].action == "reclaimed"),
                Err(error) => assert!(error.message.contains("cleanup_lease_busy"), "{error:?}"),
            }
        }
        assert_eq!(reclaimed, 1);
        assert_eq!(
            garbage_collect(&fixture.root, true).unwrap().items[0].reason,
            "already_reclaimed"
        );
    }
    #[test]
    fn storage_cache_intent_resumes_and_same_key_rebuilds_after_interruption() {
        for stage in 0..3 {
            let fixture = Fixture::captured();
            let recipe = fixture.temp.path().join("evicted.toml");
            fs::write(
                &recipe,
                "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf cache > disposable']\n",
            )
            .unwrap();
            let old =
                storage::prepare(&fixture.root, "fixture", &recipe, &Default::default()).unwrap();
            let newer = fixture.temp.path().join("selected.toml");
            fs::write(
                &newer,
                "version=1\n[[steps]]\nargv=['/bin/sh','-c','printf newest > selected']\n",
            )
            .unwrap();
            storage::prepare(&fixture.root, "fixture", &newer, &Default::default()).unwrap();
            let (_, mut store, lease) = cleanup_store(&fixture.root, true, false).unwrap();
            assert_eq!(store.cache_retention(&old.state_id).unwrap(), None);
            let name = format!("{}-{}", old.project_id, old.preparation_key);
            let component = std::ffi::CString::new(name.as_str()).unwrap();
            let opened = open_owned_cleanup(
                &fixture.root,
                c"prepared",
                &component,
                old.state_id.as_str(),
                None,
            )
            .unwrap();
            let authority = serde_json::to_string(&opened.authority).unwrap();
            store.begin_cache_eviction(&old, &name, &authority).unwrap();
            assert!(store.prepared_state(&old.state_id).unwrap().is_none());
            if stage > 0 {
                let result = opened.delete_named(&component, &mut |_| {
                    if stage == 1 {
                        Err(storage_error("injected eviction interruption"))
                    } else {
                        Ok(())
                    }
                });
                if stage == 1 {
                    assert!(result.is_err());
                } else {
                    result.unwrap();
                }
            }
            drop(store);
            drop(lease);
            let rebuilt =
                storage::prepare(&fixture.root, "fixture", &recipe, &Default::default()).unwrap();
            assert_eq!(rebuilt.preparation_key, old.preparation_key);
            assert_ne!(rebuilt.state_id, old.state_id);
            fabric_project::prepare::validate_ready(&fixture.root, &rebuilt).unwrap();
            let (_, store) = open_store(&fixture.root).unwrap();
            assert!(store.cache_evictions().unwrap().is_empty());
            assert!(store.prepared_state(&old.state_id).unwrap().is_none());
        }
    }
}
