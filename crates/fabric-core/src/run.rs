//! One foreground worker; no batch scheduling, OMP adapter, artifacts or cleanup.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fabric_domain::{ErrorKind, FabricError, RunId, RunState, WorkspaceId};
use fabric_runner::{AgentSpec, HarnessAdapter};
use fabric_workspace::{ForkStrategy, LocalWorkspaceBackend};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::workspace::{WorkspaceRequest, create_workspace, open_store, runner_workspace};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub workspace_id: String,
    pub prepared_state_id: String,
    pub base_commit_sha: String,
    pub prompt_digest: String,
    pub harness: String,
    pub harness_version: Option<String>,
    pub state: RunState,
    pub exit_code: Option<i32>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub workspace_path: String,
}

pub fn foreground(
    root: &Path,
    project_name: &str,
    prompt_file: &Path,
    harness: &impl HarnessAdapter,
    copy_limit_bytes: u64,
) -> Result<RunReport, FabricError> {
    // Snapshot the exact prompt once: digest and child input cannot disagree if
    // the caller changes its file after this point.
    let prompt = std::fs::read(prompt_file).map_err(error)?;
    let prompt_digest = format!("{:x}", Sha256::digest(&prompt));
    let (root, mut store) = open_store(root)?;
    let project = store
        .project_by_name(project_name)
        .map_err(error)?
        .ok_or_else(|| error("Unknown project; register it with project add first."))?;
    let source = fabric_project::resolve_source(Path::new(&project.source_path))?;
    let state = store
        .ready_state_for_commit(&project.project_id, &source.commit_sha)
        .map_err(error)?
        .ok_or_else(|| {
            error("No READY state for current committed HEAD; run project prepare first.")
        })?;
    if source.dirty {
        eprintln!(
            "warning: dirty Git checkout; only committed HEAD is used; uncommitted changes are not imported."
        );
    }
    let info = harness.probe()?;
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(error)?;
    let created_at = i64::try_from(clock.as_secs()).map_err(error)?;
    let unique = format!(
        "{}-{}-{}",
        clock.as_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let run_id = RunId::try_from(format!("r-{unique}"))?;
    let workspace_id = WorkspaceId::try_from(format!("w-{unique}"))?;
    let request = WorkspaceRequest {
        run_id: run_id.clone(),
        workspace_id,
        prepared_state_id: state.state_id,
        harness: harness.name().to_owned(),
        prompt_digest,
        created_at,
        // Startup reservation only, not a wall-clock run limit or deletion authority.
        lease_expires_at: created_at
            .checked_add(3600)
            .ok_or_else(|| error("Timestamp overflow"))?,
    };
    let backend = LocalWorkspaceBackend::new(&root, ForkStrategy::CloneOrCopy)?;
    let workspace = create_workspace(&root, &backend, &request, copy_limit_bytes)?;
    let execution = (|| {
        let container = workspace
            .path()
            .parent()
            .ok_or_else(|| error("Missing workspace container"))?;
        let prompt_path = container.join("prompt.input");
        let mut input = private_output(&prompt_path)?;
        input.write_all(&prompt).map_err(error)?;
        input.sync_all().map_err(error)?;
        drop(input);
        let stdout = private_output(&container.join("stdout.log"))?;
        let stderr = private_output(&container.join("stderr.log"))?;
        File::open(container)
            .and_then(|file| file.sync_all())
            .map_err(error)?;
        let command = harness.command(&AgentSpec {
            prompt_file: &prompt_path,
        })?;
        // This validation must remain immediately before launch, not merely at fork time.
        let fresh = runner_workspace(&root, &run_id)?;
        let status = fabric_runner::execute(&command, fresh.path(), stdout, stderr, |pid| {
            store
                .start_run(&run_id, &info.version, pid, now()?)
                .map_err(error)
        })?;
        store
            .observe_run_exit(&run_id, status.code(), now()?)
            .map_err(error)?;
        store.finish_run(&run_id, now()?).map_err(error)
    })();
    if let Err(failure) = execution {
        store
            .fail_run_execution(&run_id, &info.version, &failure.message, now()?)
            .map_err(|persist| {
                error(format!(
                    "{}; could not persist run failure: {persist}",
                    failure.message
                ))
            })?;
        return Err(error(format!(
            "Run {run_id}: {} (workspace retained)",
            failure.message
        )));
    }
    let run = store
        .run(&run_id)
        .map_err(error)?
        .ok_or_else(|| error("Run disappeared"))?;
    Ok(RunReport {
        run_id: run.run_id.to_string(),
        workspace_id: run.workspace_id.to_string(),
        prepared_state_id: run.prepared_state_id.to_string(),
        base_commit_sha: run.base_commit_sha,
        prompt_digest: run.prompt_digest,
        harness: run.harness,
        harness_version: run.harness_version,
        state: run.state,
        exit_code: run.exit_code,
        created_at: run.created_at,
        started_at: run.started_at,
        finished_at: run.finished_at,
        workspace_path: workspace
            .path()
            .to_str()
            .ok_or_else(|| error("Workspace path must be UTF-8"))?
            .to_owned(),
    })
}

fn private_output(path: &Path) -> Result<File, FabricError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(error)
}
fn now() -> Result<i64, FabricError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(error)?
            .as_secs(),
    )
    .map_err(error)
}
fn error(value: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Output,
        message: value.to_string(),
    }
}
