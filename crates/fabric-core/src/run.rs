//! Foreground execution shared by single runs and bounded batches; no cleanup.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fabric_domain::{BatchId, ErrorKind, FabricError, RunId, RunState, WorkspaceId};
use fabric_runner::{AgentSpec, HarnessAdapter};
use fabric_store::{BatchTask, PreparedState};
use fabric_workspace::{ForkStrategy, LocalWorkspaceBackend};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workspace::{WorkspaceRequest, create_workspace, open_store, runner_workspace};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
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
    pub events: fabric_runner::jsonl::Summary,
    pub capture: fabric_domain::CaptureMetadata,
}

pub fn foreground(
    root: &Path,
    project_name: &str,
    prompt_file: &Path,
    harness: &(impl HarnessAdapter + ?Sized),
    copy_limit_bytes: u64,
    max_log_bytes: u64,
) -> Result<RunReport, FabricError> {
    validate_capture_limit(max_log_bytes)?;
    let (root, state) = select_prepared(root, project_name)?;
    foreground_prepared(
        &root,
        &state,
        prompt_file,
        harness,
        &ExecutionOptions {
            copy_limit_bytes,
            max_log_bytes,
            harness_version: harness.probe()?.version,
        },
        new_run_id()?,
    )
}

fn select_prepared(
    root: &Path,
    project_name: &str,
) -> Result<(PathBuf, PreparedState), FabricError> {
    let (root, store) = open_store(root)?;
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
    Ok((root, state))
}

struct ExecutionOptions {
    copy_limit_bytes: u64,
    max_log_bytes: u64,
    harness_version: String,
}

fn foreground_prepared(
    root: &Path,
    state: &PreparedState,
    prompt_file: &Path,
    harness: &(impl HarnessAdapter + ?Sized),
    options: &ExecutionOptions,
    run_id: RunId,
) -> Result<RunReport, FabricError> {
    // Snapshot once: digest and child input cannot disagree after this point.
    let prompt = std::fs::read(prompt_file)
        .map_err(|failure| error(format!("Prompt {}: {failure}", prompt_file.display())))?;
    let prompt_digest = format!("{:x}", Sha256::digest(&prompt));
    let (root, mut store) = open_store(root)?;
    let created_at = now()?;
    let workspace_id = WorkspaceId::try_from(format!("w-{}", run_id.as_str()))?;
    let request = WorkspaceRequest {
        run_id: run_id.clone(),
        workspace_id,
        prepared_state_id: state.state_id.clone(),
        harness: harness.name().to_owned(),
        prompt_digest,
        created_at,
        // Startup reservation only, not a wall-clock run limit or deletion authority.
        lease_expires_at: created_at
            .checked_add(3600)
            .ok_or_else(|| error("Timestamp overflow"))?,
    };
    let backend = LocalWorkspaceBackend::new(&root, ForkStrategy::CloneOrCopy)?;
    let workspace = create_workspace(&root, &backend, &request, options.copy_limit_bytes)?;
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
        let events = private_output(&container.join("events.jsonl"))?;
        let capture = fabric_runner::capture::Capture::new(
            stdout,
            stderr,
            Some(events),
            options.max_log_bytes,
        )
        .map_err(error)?;
        File::open(container)
            .and_then(|file| file.sync_all())
            .map_err(error)?;
        let command = harness.command(&AgentSpec {
            prompt_file: &prompt_path,
        })?;
        // This validation must remain immediately before launch, not merely at fork time.
        let fresh = runner_workspace(&root, &run_id)?;
        let result = fabric_runner::execute(&command, fresh.path(), capture, |pid| {
            store
                .start_run(&run_id, &options.harness_version, pid, now()?)
                .map_err(error)
        })?;
        store
            .observe_run_exit(&run_id, result.status.code(), now()?)
            .map_err(error)?;
        let capture = result.capture.map_err(error)?;
        store.finish_run(&run_id, &capture, now()?).map_err(error)?;
        Ok::<_, FabricError>(capture)
    })();
    let capture = match execution {
        Ok(capture) => capture,
        Err(failure) => {
            store
                .fail_run_execution(&run_id, &options.harness_version, &failure.message, now()?)
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
    };
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
        events: capture.event_counts.clone(),
        capture,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskInput {
    id: String,
    prompt_file: PathBuf,
}

#[derive(Serialize)]
pub struct TaskReport {
    pub id: String,
    pub run_id: RunId,
    pub result: Option<RunReport>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct BatchReport {
    pub batch_id: BatchId,
    pub succeeded: bool,
    pub tasks: Vec<TaskReport>,
}

pub fn run_many(
    root: &Path,
    project_name: &str,
    manifest: &Path,
    concurrency: usize,
    harness: &(impl HarnessAdapter + Sync + ?Sized),
    copy_limit_bytes: u64,
    max_log_bytes: u64,
) -> Result<BatchReport, FabricError> {
    if concurrency == 0 {
        return Err(error("Concurrency must be positive."));
    }
    validate_capture_limit(max_log_bytes)?;
    let inputs: Vec<TaskInput> =
        serde_json::from_slice(&std::fs::read(manifest).map_err(error)?)
            .map_err(|failure| error(format!("Invalid task manifest: {failure}")))?;
    let mut ids = std::collections::HashSet::with_capacity(inputs.len());
    if inputs.is_empty()
        || inputs.iter().any(|task| {
            task.id.trim().is_empty()
                || task.prompt_file.as_os_str().is_empty()
                || !ids.insert(&task.id)
        })
    {
        return Err(error(
            "Task manifest requires nonempty unique IDs and prompt paths, and at least one task.",
        ));
    }
    let parent = std::path::absolute(manifest)
        .map_err(error)?
        .parent()
        .ok_or_else(|| error("Manifest has no parent directory."))?
        .to_owned();
    let tasks = inputs
        .into_iter()
        .map(|task| {
            Ok(BatchTask {
                task_id: task.id,
                run_id: new_run_id()?,
                prompt_file: parent
                    .join(task.prompt_file)
                    .into_os_string()
                    .into_string()
                    .map_err(|_| error("Prompt paths must be UTF-8."))?,
                result: None,
                error: None,
            })
        })
        .collect::<Result<Vec<_>, FabricError>>()?;
    let (root, state) = select_prepared(root, project_name)?;
    let options = ExecutionOptions {
        copy_limit_bytes,
        max_log_bytes,
        harness_version: harness.probe()?.version,
    };
    let batch_id = BatchId::try_from(format!("b-{}", new_run_id()?.as_str()))?;
    let (_, mut store) = open_store(&root)?;
    store
        .create_batch(&batch_id, &state.state_id, &tasks)
        .map_err(error)?;
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(concurrency.min(tasks.len()));
        for _ in 0..concurrency.min(tasks.len()) {
            workers.push(scope.spawn(|| {
                let (_, store) = open_store(&root)?;
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(task) = tasks.get(index) else { break };
                    let outcome = foreground_prepared(
                        &root,
                        &state,
                        Path::new(&task.prompt_file),
                        harness,
                        &options,
                        task.run_id.clone(),
                    )
                    .and_then(|report| serde_json::to_string(&report).map_err(error));
                    store
                        .finish_batch_task(
                            &task.run_id,
                            outcome
                                .as_deref()
                                .map_err(|failure| failure.message.as_str()),
                        )
                        .map_err(error)?;
                }
                Ok::<_, FabricError>(())
            }));
        }
        // Join every worker even if one reports an error; independent work must finish.
        let mut failure = None;
        for worker in workers {
            if let Err(error) = worker.join().unwrap_or_else(|_| {
                Err(error(
                    "Batch worker panicked; durable pending tasks require attention.",
                ))
            }) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    })?;
    let tasks = store
        .batch_tasks(&batch_id)
        .map_err(error)?
        .into_iter()
        .map(|task| {
            Ok(TaskReport {
                id: task.task_id,
                run_id: task.run_id,
                result: task
                    .result
                    .map(|value| serde_json::from_str(&value).map_err(error))
                    .transpose()?,
                error: task.error,
            })
        })
        .collect::<Result<Vec<_>, FabricError>>()?;
    let succeeded = tasks.iter().all(|task| {
        task.error.is_none()
            && task
                .result
                .as_ref()
                .is_some_and(|run| run.state == RunState::Succeeded)
    });
    store.finish_batch(&batch_id, succeeded).map_err(error)?;
    Ok(BatchReport {
        batch_id,
        succeeded,
        tasks,
    })
}

fn validate_capture_limit(max_log_bytes: u64) -> Result<(), FabricError> {
    if max_log_bytes < fabric_runner::capture::TRUNCATION_MARKER.len() as u64 {
        return Err(error("Capture limit must fit the truncation marker."));
    }
    Ok(())
}

fn new_run_id() -> Result<RunId, FabricError> {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(error)?;
    RunId::try_from(format!(
        "r-{}-{}-{}",
        clock.as_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
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
