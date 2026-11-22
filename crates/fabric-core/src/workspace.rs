//! Durable ownership before fork creation; no process execution or reclamation.

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
    /// A startup reservation, not a heartbeat or proof of a live process.
    /// FAB-19 owns lease-owner identity, renewal and crash reconciliation.
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
        batch_id: None,
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
        created_at: request.created_at,
        updated_at: request.created_at,
        started_at: None,
        finished_at: None,
        error_kind: None,
        artifact_manifest_id: None,
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
    let result = (|| {
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
        runner_workspace(&root, &run.run_id)
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
    if run.state != RunState::Preparing
        || workspace.state != WorkspaceState::Active
        || workspace.run_id != run.run_id
        || workspace.state_id != run.prepared_state_id
        || workspace.retention_reason.is_some()
        || workspace
            .lease_expires_at
            .is_none_or(|expiry| expiry <= now)
    {
        return Err(storage_error("Workspace ownership is not runner-ready."));
    }
    validate_directories(&root, &workspace)?;
    let path = Path::new(&workspace.managed_path);
    let marker = path
        .parent()
        .ok_or_else(|| storage_error("Missing workspace container."))?
        .join(".fabric-owner");
    let recorded = private_file(&marker)?;
    let expected = marker_contents(&root, &workspace);
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
        run_id: run.run_id,
        path: path.to_owned(),
    })
}

fn validate_directories(root: &Path, workspace: &Workspace) -> Result<(), FabricError> {
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

fn open_store(root: &Path) -> Result<(PathBuf, Store), FabricError> {
    // Never create a database in an unknown root or follow a root/database link.
    private_directory(root)?;
    let root = root.canonicalize().map_err(storage_error)?;
    private_directory(&root)?;
    private_file(&root.join("fabric.db"))?;
    let store = Store::open(root.join("fabric.db")).map_err(storage_error)?;
    Ok((root, store))
}

fn private_directory(path: &Path) -> Result<(), FabricError> {
    let meta = fs::symlink_metadata(path).map_err(storage_error)?;
    // SAFETY: geteuid has no preconditions.
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(storage_error(
            "Managed directory must be real, current-user-owned and owner-only.",
        ));
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<fs::Metadata, FabricError> {
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
