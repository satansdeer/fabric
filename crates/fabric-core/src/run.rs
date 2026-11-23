//! Foreground execution and verified capture followed by the shared safe reclaim path.
use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fabric_domain::{
    BatchId, ErrorKind, FabricError, HarnessId, HarnessPlan, HarnessRequest, PreparedStateId,
    ProfileEvidence, RunId, RunState, SourceProvenance, SourceRef, WorkspaceId,
};
use fabric_runner::{AgentSpec, HarnessAdapter, HarnessRegistry, RegistryOptions};
use fabric_store::{BatchTask, PreparedState, WorkerAttempt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workspace::{WorkspaceRequest, create_workspace_bound, open_store, runner_workspace};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
pub struct RunReport {
    pub run_id: String,
    pub workspace_id: String,
    pub prepared_state_id: String,
    pub base_commit_sha: String,
    #[serde(default)]
    pub source: Option<SourceProvenance>,
    pub prompt_digest: String,
    pub harness: String,
    pub harness_version: Option<String>,
    #[serde(default)]
    pub profile: Option<ProfileEvidence>,
    pub state: RunState,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub exit_signal: Option<i32>,
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_meter_failure: Option<crate::workspace::storage::MeterFailure>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub workspace_path: String,
    pub events: fabric_runner::jsonl::Summary,
    pub capture: fabric_domain::CaptureMetadata,
    pub artifact_manifest_id: Option<String>,
    /// Cleanup refusal never rewrites an already terminal process outcome.
    #[serde(default)]
    pub cleanup: Option<crate::workspace::GcItem>,
    #[serde(default)]
    pub worker_attempts: Vec<WorkerAttempt>,
}

#[derive(Clone)]
pub struct RunOptions {
    pub copy_limit_bytes: u64,
    pub max_log_bytes: u64,
    pub max_artifact_bytes: u64,
    pub limits: fabric_runner::ExecutionLimits,
    pub cancellation: fabric_runner::CancellationToken,
    pub storage: crate::workspace::storage::StorageOptions,
    /// Adapter-specific best-effort request admission, not an exact spending cap.
    pub max_cost_usd: Option<f64>,
    pub harness_request: HarnessRequest,
    pub target_pin: Option<fabric_runner::target::TargetPin>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            copy_limit_bytes: 0,
            max_log_bytes: fabric_domain::StoragePolicy::default().max_log_bytes_per_run,
            max_artifact_bytes: fabric_domain::StoragePolicy::default().max_artifact_bytes_per_run,
            limits: fabric_runner::ExecutionLimits::default(),
            cancellation: fabric_runner::CancellationToken::default(),
            storage: crate::workspace::storage::StorageOptions::default(),
            max_cost_usd: None,
            harness_request: HarnessRequest::default(),
            target_pin: None,
        }
    }
}

impl RunOptions {
    pub(crate) fn validate(&self) -> Result<(), FabricError> {
        self.limits.validate()?;
        validate_capture_limit(self.max_log_bytes)?;
        if self.max_artifact_bytes == 0 || self.max_artifact_bytes > i64::MAX as u64 {
            return Err(FabricError {
                kind: ErrorKind::InvalidArguments,
                message: "max_artifact_bytes must be positive and fit SQLite storage.".into(),
            });
        }
        if self.copy_limit_bytes > i64::MAX as u64 {
            return Err(FabricError {
                kind: ErrorKind::InvalidArguments,
                message: "copy_limit_bytes must fit SQLite storage.".into(),
            });
        }
        Ok(())
    }

    fn request(&self) -> Result<HarnessRequest, FabricError> {
        let mut request = self.harness_request.clone();
        if let Some(cost) = self.max_cost_usd {
            if request.max_cost_usd.is_some_and(|other| other != cost) {
                return Err(FabricError {
                    kind: ErrorKind::InvalidArguments,
                    message: "Conflicting max_cost_usd in RunOptions and harness_request.".into(),
                });
            }
            request.max_cost_usd = Some(cost);
        }
        request.validate()?;
        Ok(request)
    }

    fn select_target(&mut self, placement: &str) -> Result<(), FabricError> {
        let pin = match self.target_pin.take() {
            Some(pin) if pin.placement == placement => pin,
            Some(_) => return Err(error("Accepted target placement changed.")),
            None => fabric_runner::target::TargetPin::select(placement)?,
        };
        pin.preflight(&self.cancellation)?;
        self.target_pin = Some(pin);
        Ok(())
    }
}

pub fn foreground(
    root: &Path,
    project_name: &str,
    prompt_file: &Path,
    harness: &dyn HarnessAdapter,
    options: &RunOptions,
) -> Result<RunReport, FabricError> {
    options.validate()?;
    let request = options.request()?;
    let mut options = options.clone();
    let plan = crate::onboarding::plan(root, harness, request, &harness.probe()?)?;
    options.select_target(&plan.0.request.placement)?;
    let (root, state, source) = select_prepared(root, project_name, &SourceRef::CurrentHead {})?;
    let mut report = foreground_prepared(
        &root,
        &state,
        prompt_file,
        harness,
        &options,
        &plan,
        new_run_id()?,
    )?;
    report.source = Some(source);
    Ok(report)
}

fn select_prepared(
    root: &Path,
    project_name: &str,
    selection: &SourceRef,
) -> Result<(PathBuf, PreparedState, SourceProvenance), FabricError> {
    let (root, store) = open_store(root)?;
    let source_error = |message: &str| FabricError {
        kind: ErrorKind::Source,
        message: message.into(),
    };
    let project = store
        .project_by_name(project_name)
        .map_err(error)?
        .ok_or_else(|| source_error("Unknown project; register it with project add first."))?;
    let mut checkout_dirty = None;
    let state = match selection {
        SourceRef::Prepared { state_id } => {
            store.prepared_state(&PreparedStateId::try_from(state_id.clone())?).map_err(error)?
        }
        SourceRef::Commit { sha } => {
            if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(FabricError { kind: ErrorKind::InvalidArguments,
                    message: "Commit selection requires an exact full hexadecimal SHA.".into() });
            }
            store.ready_state_for_commit(&project.project_id, &sha.to_ascii_lowercase()).map_err(error)?
        }
        SourceRef::CurrentHead {} => {
            let source = fabric_project::resolve_source(Path::new(&project.source_path))?;
            checkout_dirty = Some(source.dirty);
            if source.dirty {
                eprintln!("warning: dirty Git checkout; only committed HEAD is used; uncommitted changes are not imported.");
            }
            store.ready_state_for_commit(&project.project_id, &source.commit_sha).map_err(error)?
        }
    }.ok_or_else(|| source_error("No READY prepared state matches the requested source; run project prepare first."))?;
    if state.project_id != project.project_id
        || state.status != fabric_domain::PreparedStateStatus::Ready
    {
        return Err(source_error(
            "Requested source must be a READY prepared state owned by the project.",
        ));
    }
    fabric_project::prepare::validate_ready(&root, &state).map_err(|failure| FabricError {
        kind: ErrorKind::Source,
        message: failure.message,
    })?;
    let provenance = SourceProvenance {
        source_ref: selection.clone(),
        prepared_state_id: state.state_id.clone(),
        base_commit_sha: state.source_commit_sha.clone(),
        checkout_dirty,
        dirty_changes_excluded: true,
    };
    Ok((root, state, provenance))
}

pub(crate) fn preflight_sources<'a>(
    root: &Path,
    project: &str,
    selections: impl Iterator<Item = Option<&'a SourceRef>>,
) -> Result<(), FabricError> {
    let current = SourceRef::CurrentHead {};
    let mut seen = HashSet::new();
    for selection in selections {
        let selection = selection.unwrap_or(&current);
        if seen.insert(selection) {
            select_prepared(root, project, selection)?;
        }
    }
    Ok(())
}

fn foreground_prepared(
    root: &Path,
    state: &PreparedState,
    prompt_file: &Path,
    harness: &dyn HarnessAdapter,
    options: &RunOptions,
    planned: &(HarnessPlan, Option<String>),
    run_id: RunId,
) -> Result<RunReport, FabricError> {
    let prompt = std::fs::read(prompt_file)
        .map_err(|failure| error(format!("Prompt {}: {failure}", prompt_file.display())))?;
    execute_prepared(root, state, &prompt, harness, options, planned, run_id)
}

pub(crate) fn execute_prepared(
    root: &Path,
    state: &PreparedState,
    prompt: &[u8],
    harness: &dyn HarnessAdapter,
    options: &RunOptions,
    planned: &(HarnessPlan, Option<String>),
    run_id: RunId,
) -> Result<RunReport, FabricError> {
    let (plan, consent) = planned;
    crate::onboarding::revalidate(root, plan, consent.as_deref())?;
    let mut progress = crate::logs::Progress::new(options);
    if options.cancellation.is_cancelled() {
        return Err(error("Cancelled before launch; task was not started."));
    }
    let pin = match &options.target_pin {
        Some(pin) if pin.placement == plan.request.placement => pin.clone(),
        Some(_) => return Err(error("Accepted target placement changed.")),
        None => fabric_runner::target::TargetPin::select(&plan.request.placement)?,
    };
    let target = pin.open(&options.cancellation)?;
    let mut storage = options.storage.clone();
    storage.policy.max_log_bytes_per_run = options.max_log_bytes;
    storage.policy.max_artifact_bytes_per_run = options.max_artifact_bytes;
    let mut reservation = crate::workspace::storage::admit(root, &storage, state, run_id.as_str())?;
    let prompt_digest = format!("{:x}", Sha256::digest(prompt));
    let (root, mut store) = open_store(root)?;
    let created_at = now()?;
    let workspace_id = WorkspaceId::try_from(format!("w-{}", run_id.as_str()))?;
    let request = WorkspaceRequest {
        run_id: run_id.clone(),
        workspace_id: workspace_id.clone(),
        prepared_state_id: state.state_id.clone(),
        harness: harness.id().to_string(),
        prompt_digest,
        created_at,
        // Startup reservation only, not a wall-clock run limit or deletion authority.
        lease_expires_at: created_at
            .checked_add(3600)
            .ok_or_else(|| error("Timestamp overflow"))?,
    };
    let backend = crate::target::Environment {
        root: &root,
        run: &run_id,
        target: &target,
    };
    let workspace = create_workspace_bound(
        &root,
        &backend,
        &request,
        options.copy_limit_bytes,
        Some(&pin),
    )?;
    let harness_id = harness.id();
    let mut attempt = WorkerAttempt {
        attempt_id: format!("a-{run_id}"),
        run_id: run_id.clone(),
        harness_requested: harness_id.clone(),
        harness_resolved: harness_id,
        harness_version: plan.manifest.exact_version.clone(),
        model_requested: plan.request.model.clone(),
        model_resolved: plan.model_resolved.clone(),
        profile_digest: Some(plan.profile.digest.clone()),
        backend_id: "trusted-local".into(),
        event_cursor: 0,
        created_at,
        started_at: None,
        finished_at: None,
        exit_code: None,
        exit_signal: None,
        termination_reason: None,
    };
    let activity = fabric_runner::activity::SharedActivity::default();
    let execution = (|| {
        store.record_worker_attempt(&attempt).map_err(error)?;
        progress.save(&store, &run_id, &activity)?;
        store
            .record_capture_context(
                &run_id,
                &fabric_store::CaptureContext {
                    profile: plan.profile.clone(),
                    artifact_limit_bytes: options.max_artifact_bytes,
                },
            )
            .map_err(error)?;
        let container = workspace
            .path()
            .parent()
            .ok_or_else(|| error("Missing workspace container"))?;
        let prompt_path = container.join("prompt.input");
        let mut input = private_output(&prompt_path)?;
        input.write_all(prompt).map_err(error)?;
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
        .map_err(error)?
        .with_harness(harness)
        .with_activity(std::sync::Arc::clone(&activity));
        File::open(container)
            .and_then(|file| file.sync_all())
            .map_err(error)?;
        crate::onboarding::revalidate(&root, plan, consent.as_deref())?;
        let command = harness.command(&AgentSpec {
            prompt_file: &prompt_path,
            request: &plan.request,
            max_cost_usd: plan.request.max_cost_usd,
            profile: &plan.profile,
        })?;
        // This validation must remain immediately before launch, not merely at fork time.
        let fresh = runner_workspace(&root, &run_id)?;
        crate::workspace::storage::check_launch(&root, &storage)?;
        reservation.launching(&store)?;
        let token = options.cancellation.worker_token();
        let done = std::sync::atomic::AtomicBool::new(false);
        crate::onboarding::revalidate(&root, plan, consent.as_deref())?;
        let observation_store = open_store(&root)?.1;
        let (mut result, meter_failure) = std::thread::scope(|scope| {
            let monitor = scope.spawn(|| {
                crate::workspace::storage::monitor(&root, fresh.path(), &storage, &token, &done)
            });
            let observations = scope.spawn(|| {
                crate::logs::monitor(observation_store, &run_id, &progress, &activity, &done)
            });
            let result = target.execute(
                run_id.as_str(),
                fabric_runner::ExecutionRequest {
                    spec: &command,
                    cwd: fresh.path(),
                    capture,
                    limits: &options.limits,
                    cancellation: &token,
                    owner_lease: Some(reservation.file()),
                },
                || {
                    let validated = runner_workspace(&root, &run_id)?;
                    if validated.path() != fresh.path() {
                        return Err(error("Workspace changed before launch."));
                    }
                    crate::workspace::storage::check_launch(&root, &storage)?;
                    crate::onboarding::revalidate(&root, plan, consent.as_deref())
                },
                |pid| {
                    let started_at = now()?;
                    attempt.started_at = Some(started_at);
                    store.record_worker_attempt(&attempt).map_err(error)?;
                    reservation.started(&store, pid)?;
                    store
                        .start_run(&run_id, &plan.manifest.exact_version, pid, started_at)
                        .map_err(error)?;
                    Ok(())
                },
            );
            done.store(true, std::sync::atomic::Ordering::Release);
            monitor.thread().unpark();
            observations.thread().unpark();
            let meter_failure = monitor
                .join()
                .map_err(|_| error("Storage monitor panicked"))?;
            observations
                .join()
                .map_err(|_| error("Activity monitor panicked"))??;
            Ok::<_, FabricError>((result, meter_failure))
        })?;
        progress.storage_meter_failure = meter_failure;
        // A fast zero-exit worker must not hide an oversized or unmeasurable workspace.
        if let Ok(result) = &mut result {
            if result.stop_reason.is_none() {
                result.stop_reason = token.storage_reason().or_else(|| {
                    match crate::workspace::storage::violation(&root, fresh.path(), &storage) {
                        Ok(reason) => reason,
                        Err(failure) => {
                            progress.storage_meter_failure = Some(failure);
                            Some(fabric_runner::StopReason::StorageMeterFailed)
                        }
                    }
                });
            }
        }
        // Save before exit/capture: finalization failure must not erase meter evidence.
        if progress.storage_meter_failure.is_some() {
            progress.save(&store, &run_id, &activity)?;
        }
        let result = result?;
        let finished_at = now()?;
        attempt.finished_at = Some(finished_at);
        attempt.exit_code = result.status.code();
        attempt.exit_signal = result.status.signal();
        let native_failure = result
            .capture
            .as_ref()
            .ok()
            .and_then(|capture| capture.native_failure.as_ref());
        let reason = result
            .stop_reason
            .map(|reason| reason.as_str())
            .or_else(|| native_failure.map(|_| "native_failure"));
        attempt.termination_reason = reason.map(str::to_owned);
        if let Ok(capture) = &result.capture {
            // Observed normalized events, including events dropped after truncation;
            // this cursor is not a promise that every event remains replayable.
            attempt.event_cursor = capture.normalized_event_count;
        }
        store.record_worker_attempt(&attempt).map_err(error)?;
        store
            .observe_run_exit(
                &run_id,
                result.status.code(),
                result.status.signal(),
                reason,
                finished_at,
            )
            .map_err(error)?;
        let capture = result.capture.map_err(error)?;
        store.record_capture(&run_id, &capture).map_err(error)?;
        progress.artifact_capture_started_at_ms = Some(fabric_runner::activity::timestamp_ms());
        progress.save(&store, &run_id, &activity)?;
        let (manifest, artifacts) = target.drive(
            "collect",
            run_id.as_str(),
            &[fabric_runner::target::Action::Capture],
            serde_json::json!({"max_bytes":options.max_artifact_bytes}),
            |_| {
                let captured = crate::artifacts::capture(
                    &root,
                    &store,
                    &run_id,
                    state,
                    &capture,
                    &plan.profile,
                    options.max_artifact_bytes,
                )?;
                let evidence = serde_json::json!({"artifact_count":captured.1.len()});
                Ok((captured, evidence))
            },
        )?;
        target.finish()?;
        progress.artifacts_captured_at_ms = Some(fabric_runner::activity::timestamp_ms());
        progress.save(&store, &run_id, &activity)?;
        store
            .finish_run(&run_id, &capture, &manifest, &artifacts, now()?)
            .map_err(error)?;
        Ok::<_, FabricError>(capture)
    })();
    let capture = match execution {
        Ok(capture) => capture,
        Err(failure) => {
            let attempt_failure = if attempt.finished_at.is_none() {
                attempt.finished_at = Some(now()?);
                attempt.termination_reason = Some("execution_failed".into());
                store.record_worker_attempt(&attempt).err()
            } else {
                None
            };
            store
                .fail_run_execution(
                    &run_id,
                    &plan.manifest.exact_version,
                    &failure.message,
                    now()?,
                    options.cancellation.is_cancelled(),
                )
                .map_err(|persist| {
                    error(format!(
                        "{}; could not persist run failure: {persist}",
                        failure.message
                    ))
                })?;
            if let Some(persist) = attempt_failure {
                return Err(error(format!(
                    "Run {run_id}: {}; could not persist worker attempt failure: {persist} (workspace retained)",
                    failure.message
                )));
            }
            return Err(FabricError {
                kind: failure.kind,
                message: format!("Run {run_id}: {} (workspace retained)", failure.message),
            });
        }
    };
    drop(reservation);
    let cleanup = crate::workspace::reclaim_run(&root, &run_id).unwrap_or_else(|failure| {
        crate::workspace::GcItem {
            workspace_id: workspace_id.clone(),
            run_id: run_id.clone(),
            action: "refused".into(),
            reason: "cleanup_unavailable".into(),
            detail: Some(failure.message),
        }
    });
    progress.cleanup_observed_at_ms = Some(fabric_runner::activity::timestamp_ms());
    progress.save(&store, &run_id, &activity)?;
    let run = store
        .run(&run_id)
        .map_err(error)?
        .ok_or_else(|| error("Run disappeared"))?;
    Ok(RunReport {
        run_id: run.run_id.to_string(),
        workspace_id: run.workspace_id.to_string(),
        prepared_state_id: run.prepared_state_id.to_string(),
        base_commit_sha: run.base_commit_sha,
        source: store.task_source(&run_id).map_err(error)?,
        prompt_digest: run.prompt_digest,
        harness: run.harness,
        harness_version: run.harness_version,
        profile: Some(plan.profile.clone()),
        state: run.state,
        exit_code: run.exit_code,
        exit_signal: run.exit_signal,
        error_kind: run.error_kind,
        storage_meter_failure: progress.storage_meter_failure,
        events: capture.event_counts.clone(),
        capture,
        artifact_manifest_id: run.artifact_manifest_id.map(|id| id.to_string()),
        cleanup: Some(cleanup),
        worker_attempts: store.worker_attempts(&run_id).map_err(error)?,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub id: String,
    pub prompt_file: PathBuf,
    #[serde(default)]
    pub harness: Option<HarnessId>,
    #[serde(default)]
    pub request: Option<HarnessRequest>,
}

pub struct BatchHarnesses<'a> {
    pub registry: &'a HarnessRegistry,
    pub options: &'a RegistryOptions,
    pub default: Option<&'a HarnessId>,
}

#[derive(Serialize, Deserialize)]
pub struct TaskReport {
    pub id: String,
    pub run_id: RunId,
    pub result: Option<RunReport>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
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
    harnesses: &BatchHarnesses<'_>,
    options: &RunOptions,
) -> Result<BatchReport, FabricError> {
    let mut inputs: Vec<TaskInput> =
        serde_json::from_slice(&std::fs::read(manifest).map_err(error)?).map_err(|failure| {
            crate::api::decode_error(
                failure,
                "Invalid task manifest; check the supported task and profile fields.",
            )
        })?;
    let parent = std::path::absolute(manifest)
        .map_err(error)?
        .parent()
        .ok_or_else(|| error("Manifest has no parent directory."))?
        .to_owned();
    for task in &mut inputs {
        if !task.prompt_file.as_os_str().is_empty() {
            task.prompt_file = parent.join(&task.prompt_file);
        }
    }
    spawn_many(root, project_name, inputs, concurrency, harnesses, options)
}
pub(crate) struct ExecutionTask {
    pub id: String,
    /// Inline bytes, or a legacy prompt file read independently when its worker runs.
    pub prompt: Option<Vec<u8>>,
    pub prompt_file: String,
    pub harness: HarnessId,
    pub options: RunOptions,
    pub source_ref: Option<SourceRef>,
}

pub(crate) struct ExecutionBatch {
    pub project: String,
    pub concurrency: usize,
    pub tasks: Vec<ExecutionTask>,
}

struct PromptSpool(PathBuf);

impl PromptSpool {
    fn write(
        root: &Path,
        inputs: &[ExecutionTask],
        tasks: &mut [BatchTask],
    ) -> Result<Option<Self>, FabricError> {
        if !inputs.iter().any(|task| task.prompt.is_some()) {
            return Ok(None);
        }
        let path = root.join(format!("api-prompts-{}", new_run_id()?.as_str()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(error)?;
        let spool = Self(path);
        for (index, (input, task)) in inputs.iter().zip(tasks).enumerate() {
            if let Some(prompt) = &input.prompt {
                let path = spool.0.join(index.to_string());
                private_output(&path)?.write_all(prompt).map_err(error)?;
                task.prompt_file = path
                    .into_os_string()
                    .into_string()
                    .map_err(|_| error("Prompt paths must be UTF-8."))?;
            }
        }
        Ok(Some(spool))
    }
}

impl Drop for PromptSpool {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Shared typed batch service; prompt paths are already resolved by the caller.
pub fn spawn_many(
    root: &Path,
    project_name: &str,
    inputs: Vec<TaskInput>,
    concurrency: usize,
    harnesses: &BatchHarnesses<'_>,
    options: &RunOptions,
) -> Result<BatchReport, FabricError> {
    let tasks = inputs
        .into_iter()
        .map(|task| {
            let harness = task
                .harness
                .or_else(|| harnesses.default.cloned())
                .ok_or_else(|| FabricError {
                    kind: ErrorKind::UnknownHarness,
                    message: format!("Task {} requires a harness or a batch default.", task.id),
                })?;
            if task.prompt_file.as_os_str().is_empty() {
                return Err(error("Task manifest requires nonempty prompt paths."));
            }
            let prompt_file = task
                .prompt_file
                .into_os_string()
                .into_string()
                .map_err(|_| error("Prompt paths must be UTF-8."))?;
            let mut options = options.clone();
            if let Some(request) = task.request {
                options.harness_request = request;
            }
            Ok(ExecutionTask {
                id: task.id,
                prompt: None,
                prompt_file,
                harness,
                options,
                source_ref: None,
            })
        })
        .collect::<Result<Vec<_>, FabricError>>()?;
    spawn_execution(
        root,
        ExecutionBatch {
            project: project_name.into(),
            concurrency,
            tasks,
        },
        harnesses,
    )
}

pub(crate) fn spawn_execution(
    root: &Path,
    batch: ExecutionBatch,
    harnesses: &BatchHarnesses<'_>,
) -> Result<BatchReport, FabricError> {
    let concurrency = batch.concurrency;
    let PreparedBatch {
        root,
        states,
        sources,
        mut tasks,
        plans,
        adapters,
        inputs,
    } = prepare_batch(root, batch, harnesses)?;
    let batch_id = BatchId::try_from(format!("b-{}", new_run_id()?.as_str()))?;
    let (_, mut store) = open_store(&root)?;
    let _prompt_spool = PromptSpool::write(&root, &inputs, &mut tasks)?;
    store
        .create_batch_with_sources(&batch_id, &sources[0].prepared_state_id, &tasks, &sources)
        .map_err(error)?;
    run_pool(tasks.len(), concurrency, |index| {
        let (_, store) = open_store(&root)?;
        let task = &tasks[index];
        let state = &states[&sources[index].prepared_state_id];
        let (harness_id, plan) = &plans[index];
        let outcome = match &inputs[index].prompt {
            Some(prompt) => execute_prepared(
                &root,
                state,
                prompt,
                adapters[harness_id].0.as_ref(),
                &inputs[index].options,
                plan,
                task.run_id.clone(),
            ),
            None => foreground_prepared(
                &root,
                state,
                Path::new(&task.prompt_file),
                adapters[harness_id].0.as_ref(),
                &inputs[index].options,
                plan,
                task.run_id.clone(),
            ),
        }
        .and_then(|report| serde_json::to_string(&report).map_err(error));
        store
            .finish_batch_task(
                &task.run_id,
                outcome
                    .as_deref()
                    .map_err(|failure| failure.message.as_str()),
            )
            .map_err(error)
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

pub(crate) struct PreparedBatch {
    pub root: PathBuf,
    pub states: HashMap<PreparedStateId, PreparedState>,
    pub sources: Vec<SourceProvenance>,
    pub tasks: Vec<BatchTask>,
    pub plans: Vec<(HarnessId, (HarnessPlan, Option<String>))>,
    pub adapters: PlannedAdapters,
    pub inputs: Vec<ExecutionTask>,
}

pub(crate) type PlannedAdapters =
    HashMap<HarnessId, (Box<dyn HarnessAdapter>, fabric_runner::HarnessInfo)>;

pub(crate) fn plan_task(
    root: &Path,
    id: &HarnessId,
    options: &mut RunOptions,
    harnesses: &BatchHarnesses<'_>,
    adapters: &mut PlannedAdapters,
) -> Result<(HarnessPlan, Option<String>), FabricError> {
    options.validate()?;
    let request = options.request()?;
    let (adapter, probe) = match adapters.entry(id.clone()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let adapter = harnesses.registry.resolve(id, harnesses.options)?;
            let probe = adapter.probe()?;
            entry.insert((adapter, probe))
        }
    };
    let plan = crate::onboarding::plan(root, adapter.as_ref(), request, probe)?;
    options.select_target(&plan.0.request.placement)?;
    Ok(plan)
}

pub(crate) fn validate_durable_plan(plan: &HarnessPlan) -> Result<(), FabricError> {
    if plan.request.profile.as_ref().is_some_and(|profile| {
        profile
            .connection_refs
            .iter()
            .any(|reference| reference.kind == "env")
    }) {
        return Err(FabricError {
            kind: ErrorKind::UnsupportedCapability,
            message: "Async native environment connections are not restart-safe; use a durable native connection or the foreground API.".into(),
        });
    }
    Ok(())
}

pub(crate) fn prepare_batch(
    root: &Path,
    batch: ExecutionBatch,
    harnesses: &BatchHarnesses<'_>,
) -> Result<PreparedBatch, FabricError> {
    if batch.concurrency == 0 {
        return Err(error("Concurrency must be positive."));
    }
    let mut inputs = batch.tasks;
    let mut ids = HashSet::with_capacity(inputs.len());
    if inputs.is_empty()
        || inputs
            .iter()
            .any(|task| task.id.trim().is_empty() || !ids.insert(&task.id))
    {
        return Err(error(
            "Task manifest requires nonempty unique IDs and at least one task.",
        ));
    }
    drop(ids);
    let mut adapters = HashMap::new();
    let mut tasks = Vec::with_capacity(inputs.len());
    let mut plans = Vec::with_capacity(inputs.len());
    // Entire batch admission is read-only until every adapter/request has a plan.
    for task in &mut inputs {
        let id = task.harness.clone();
        let plan = plan_task(root, &id, &mut task.options, harnesses, &mut adapters)?;
        tasks.push(BatchTask {
            task_id: task.id.clone(),
            run_id: new_run_id()?,
            prompt_file: task.prompt_file.clone(),
            result: None,
            error: None,
        });
        plans.push((id, plan));
    }
    let mut states = HashMap::new();
    let mut selected = HashMap::new();
    let mut sources = Vec::with_capacity(inputs.len());
    let (root, _) = open_store(root)?;
    let current = SourceRef::CurrentHead {};
    for input in &inputs {
        let selection = input.source_ref.as_ref().unwrap_or(&current);
        let source = match selected.entry(selection.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let (_, state, source) = select_prepared(&root, &batch.project, selection)?;
                states.entry(state.state_id.clone()).or_insert(state);
                entry.insert(source)
            }
        };
        sources.push(source.clone());
    }
    Ok(PreparedBatch {
        root,
        states,
        sources,
        tasks,
        plans,
        adapters,
        inputs,
    })
}

/// Both foreground and durable batches use this bounded, independent-task pool.
pub(crate) fn run_pool(
    count: usize,
    concurrency: usize,
    execute: impl Fn(usize) -> Result<(), FabricError> + Sync,
) -> Result<(), FabricError> {
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(concurrency.min(count));
        for _ in 0..concurrency.min(count) {
            workers.push(scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    execute(index)?;
                }
                Ok::<_, FabricError>(())
            }));
        }
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
    })
}

fn validate_capture_limit(max_log_bytes: u64) -> Result<(), FabricError> {
    if max_log_bytes < fabric_runner::capture::TRUNCATION_MARKER.len() as u64
        || max_log_bytes > i64::MAX as u64
    {
        return Err(FabricError {
            kind: ErrorKind::InvalidArguments,
            message: "Capture limit must fit the truncation marker and SQLite storage.".into(),
        });
    }
    Ok(())
}

pub(crate) fn new_run_id() -> Result<RunId, FabricError> {
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
