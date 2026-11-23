//! Generic worker requests and the version-1 compatibility translation.
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use fabric_domain::{
    AgentProfile, ErrorKind, ExecutionContext, FabricError, HarnessId, HarnessRequest, ModelPolicy,
    WorkerLimits, WorkerTask,
};
use fabric_runner::{CancellationToken, HarnessRegistry, RegistryOptions};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::run::{BatchHarnesses, BatchReport, ExecutionBatch, ExecutionTask, RunOptions};

pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_TASKS: usize = 128;
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    pub version: u32,
    pub op: String,
    pub project: String,
    pub concurrency: usize,
    #[serde(default)]
    pub default_harness: Option<HarnessId>,
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub options: Options,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub harness: Option<HarnessId>,
    #[serde(default)]
    pub request: Option<HarnessRequest>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub timeout_seconds: Option<u64>,
    pub copy_limit_bytes: Option<u64>,
    pub max_log_bytes: Option<u64>,
    pub max_artifact_bytes: Option<u64>,
}

impl Options {
    pub(crate) fn from_run(options: &RunOptions) -> Self {
        Self {
            timeout_seconds: Some(options.limits.timeout.as_secs()),
            copy_limit_bytes: Some(options.copy_limit_bytes),
            max_log_bytes: Some(options.max_log_bytes),
            max_artifact_bytes: Some(options.max_artifact_bytes),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct BatchResponse {
    pub version: u32,
    pub op: String,
    pub batch: BatchReport,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerBatchRequest {
    pub project: String,
    pub tasks: Vec<WorkerTask>,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "local_placement")]
    pub placement: String,
}

fn default_concurrency() -> usize {
    1
}
fn local_placement() -> String {
    "local".into()
}

/// Bounded, non-echoing JSON decoding for file/transport consumers.
pub fn read_request(reader: impl Read) -> Result<BatchRequest, FabricError> {
    read_json(reader)
}

pub fn read_worker_request(reader: impl Read) -> Result<WorkerBatchRequest, FabricError> {
    let request = read_json(reader)?;
    validate_worker_request(&request)?;
    Ok(request)
}

fn read_json<T: DeserializeOwned>(reader: impl Read) -> Result<T, FabricError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("Cannot read API request JSON."))?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(invalid("API request exceeds 1 MiB."));
    }
    serde_json::from_slice(&bytes).map_err(|failure| {
        decode_error(
            failure,
            "Invalid API request JSON; check the supported request and profile fields.",
        )
    })
}

pub(crate) fn decode_error(failure: serde_json::Error, fallback: &str) -> FabricError {
    const MIGRATION: &str = "Portable profile.skills/plugins/mcp are no longer supported; configure skills, plugins, and MCP in the native harness instead.";
    if failure.to_string().starts_with(MIGRATION) {
        FabricError {
            kind: ErrorKind::UnsupportedCapability,
            message: MIGRATION.into(),
        }
    } else {
        invalid(fallback)
    }
}

pub fn spawn_many(
    root: &Path,
    request: BatchRequest,
    registry: &HarnessRegistry,
    registry_options: &RegistryOptions,
    cancellation: CancellationToken,
) -> Result<BatchResponse, FabricError> {
    let batch = crate::run::spawn_execution(
        root,
        translate_v1(request, cancellation)?,
        &BatchHarnesses {
            registry,
            options: registry_options,
            default: None,
        },
    )?;
    Ok(BatchResponse {
        version: 1,
        op: "spawn_many".into(),
        batch,
    })
}

pub(crate) fn translate_v1(
    request: BatchRequest,
    cancellation: CancellationToken,
) -> Result<ExecutionBatch, FabricError> {
    validate_request(&request)?;
    let options = run_options(&request.options, cancellation)?;
    let tasks = request
        .tasks
        .into_iter()
        .map(|task| {
            let harness = task
                .harness
                .or_else(|| request.default_harness.clone())
                .ok_or_else(|| FabricError {
                    kind: ErrorKind::UnknownHarness,
                    message: format!("Task {} requires a harness or a batch default.", task.id),
                })?;
            let mut options = options.clone();
            if let Some(request) = task.request {
                options.harness_request = request;
            }
            Ok(ExecutionTask {
                id: task.id,
                prompt: Some(task.prompt.into_bytes()),
                prompt_file: "inline-input".into(),
                harness,
                options,
                source_ref: None,
            })
        })
        .collect::<Result<Vec<_>, FabricError>>()?;
    Ok(ExecutionBatch {
        project: request.project,
        concurrency: request.concurrency,
        tasks,
    })
}

pub(crate) fn validate_worker_request(request: &WorkerBatchRequest) -> Result<(), FabricError> {
    HarnessId::try_from(request.placement.clone()).map_err(|_| FabricError {
        kind: ErrorKind::UnsupportedPlacement,
        message: "Placement must name an installed target.".into(),
    })?;
    validate_size(
        &request.project,
        request.max_concurrency,
        request.tasks.len(),
        request.tasks.iter().map(|task| task.prompt.len()),
    )?;
    serde_json::to_writer(RequestBudget(MAX_REQUEST_BYTES), request)
        .map_err(|_| invalid("API request exceeds 1 MiB."))?;
    let mut ids = HashSet::with_capacity(request.tasks.len());
    if request
        .tasks
        .iter()
        .any(|task| task.task_id.trim().is_empty() || !ids.insert(&task.task_id))
    {
        return Err(invalid("Tasks require nonempty unique task_id values."));
    }
    for task in &request.tasks {
        if task
            .limits
            .as_ref()
            .is_some_and(|limits| limits.max_cost_usd.is_some())
        {
            return Err(FabricError { kind: ErrorKind::UnsupportedCapability,
                message: "No registered adapter enforces a hard max_cost_usd limit; native best-effort budgets are not spending caps.".into() });
        }
    }
    Ok(())
}

pub(crate) fn translate_workers(
    request: WorkerBatchRequest,
) -> Result<ExecutionBatch, FabricError> {
    validate_worker_request(&request)?;
    let tasks = request
        .tasks
        .into_iter()
        .map(|task| {
            let mut options = worker_options(
                &task.harness_id,
                task.model,
                task.execution_context,
                task.limits.unwrap_or_default(),
            )?;
            options.harness_request.placement = request.placement.clone();
            Ok(ExecutionTask {
                id: task.task_id,
                prompt: Some(task.prompt.into_bytes()),
                prompt_file: "inline-input".into(),
                harness: task.harness_id,
                options,
                source_ref: task.source_ref,
            })
        })
        .collect::<Result<Vec<_>, FabricError>>()?;
    Ok(ExecutionBatch {
        project: request.project,
        concurrency: request.max_concurrency,
        tasks,
    })
}

fn worker_options(
    harness: &HarnessId,
    model: Option<String>,
    context: Option<ExecutionContext>,
    limits: WorkerLimits,
) -> Result<RunOptions, FabricError> {
    let mut options = run_options(
        &Options {
            timeout_seconds: limits.timeout_seconds,
            copy_limit_bytes: limits.copy_limit_bytes,
            max_log_bytes: limits.max_log_bytes,
            max_artifact_bytes: limits.max_artifact_bytes,
        },
        CancellationToken::default(),
    )?;
    options.harness_request.model = model;
    if let Some(ExecutionContext::Explicit {
        harness_version,
        native_config_refs,
        connection_refs,
    }) = context
    {
        options.harness_request.profile = Some(AgentProfile {
            harness_id: harness.clone(),
            harness_version,
            native_config_refs,
            connection_refs,
            model_policy: ModelPolicy::NativeDefault {},
            skills: Vec::new(),
            plugins: Vec::new(),
            mcp: Vec::new(),
        });
    }
    Ok(options)
}

/// The client uses the same read-only adapter negotiation before starting any manager.
/// The manager repeats it at its trust boundary; prompts are never copied for preflight.
pub(crate) fn preflight_workers(
    root: &Path,
    request: &WorkerBatchRequest,
    registry_options: &RegistryOptions,
) -> Result<(), FabricError> {
    validate_worker_request(request)?;
    crate::run::preflight_sources(
        root,
        &request.project,
        request.tasks.iter().map(|task| task.source_ref.as_ref()),
    )?;
    let registry = HarnessRegistry::default();
    let harnesses = BatchHarnesses {
        registry: &registry,
        options: registry_options,
        default: None,
    };
    let mut adapters = crate::run::PlannedAdapters::new();
    for task in &request.tasks {
        let mut options = worker_options(
            &task.harness_id,
            task.model.clone(),
            task.execution_context.clone(),
            task.limits.clone().unwrap_or_default(),
        )?;
        options.harness_request.placement = request.placement.clone();
        let (plan, _) = crate::run::plan_task(
            root,
            &task.harness_id,
            &mut options,
            &harnesses,
            &mut adapters,
        )?;
        crate::run::validate_durable_plan(&plan)?;
    }
    Ok(())
}

fn validate_size(
    project: &str,
    concurrency: usize,
    count: usize,
    prompts: impl Iterator<Item = usize>,
) -> Result<(), FabricError> {
    if project.trim().is_empty() || concurrency == 0 {
        return Err(invalid(
            "API requires a nonempty project and positive concurrency.",
        ));
    }
    if count == 0
        || count > MAX_TASKS
        || prompts.into_iter().any(|length| length > MAX_PROMPT_BYTES)
    {
        return Err(invalid(
            "API requires 1–128 tasks with at most 64 KiB per prompt.",
        ));
    }
    Ok(())
}

pub(crate) fn validate_request(request: &BatchRequest) -> Result<(), FabricError> {
    if request.version != 1 || request.op != "spawn_many" {
        return Err(invalid("API requires version 1 and op spawn_many."));
    }
    validate_size(
        &request.project,
        request.concurrency,
        request.tasks.len(),
        request.tasks.iter().map(|task| task.prompt.len()),
    )?;
    // Count without allocating another copy; typed callers obey the transport bound too.
    serde_json::to_writer(RequestBudget(MAX_REQUEST_BYTES), &request)
        .map_err(|_| invalid("API request exceeds 1 MiB."))?;
    Ok(())
}

pub(crate) fn run_options(
    request: &Options,
    cancellation: CancellationToken,
) -> Result<RunOptions, FabricError> {
    let mut options = RunOptions {
        cancellation,
        ..RunOptions::default()
    };
    if let Some(seconds) = request.timeout_seconds {
        options.limits.timeout = Duration::from_secs(seconds);
    }
    if let Some(limit) = request.copy_limit_bytes {
        options.copy_limit_bytes = limit;
    }
    if let Some(limit) = request.max_log_bytes {
        options.max_log_bytes = limit;
    }
    if let Some(limit) = request.max_artifact_bytes {
        options.max_artifact_bytes = limit;
    }
    options.validate()?;
    Ok(options)
}

struct RequestBudget(usize);
impl Write for RequestBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("Request too large"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn invalid(message: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidArguments,
        message: message.into(),
    }
}
