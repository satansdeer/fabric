//! Versioned foreground control surface; execution and admission remain in `run`.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fabric_domain::{ErrorKind, FabricError, HarnessId, HarnessRequest};
use fabric_runner::{CancellationToken, HarnessRegistry, RegistryOptions};
use serde::{Deserialize, Serialize};

use crate::run::{BatchHarnesses, BatchReport, RunOptions, TaskInput};

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

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub timeout_seconds: Option<u64>,
    pub copy_limit_bytes: Option<u64>,
    pub max_log_bytes: Option<u64>,
    pub max_artifact_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize)]
pub struct BatchResponse {
    pub version: u32,
    pub op: String,
    pub batch: BatchReport,
}

/// Bounded, non-echoing JSON decoding for file/transport consumers.
pub fn read_request(reader: impl Read) -> Result<BatchRequest, FabricError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("Cannot read API request JSON."))?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(invalid("API request exceeds 1 MiB."));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        invalid("Invalid API request JSON; check the supported request and profile fields.")
    })
}

/// Inline prompts are private temporary inputs, never a second execution protocol.
pub fn spawn_many(
    root: &Path,
    request: BatchRequest,
    registry: &HarnessRegistry,
    registry_options: &RegistryOptions,
    cancellation: CancellationToken,
) -> Result<BatchResponse, FabricError> {
    if request.version != 1 || request.op != "spawn_many" {
        return Err(invalid("API requires version 1 and op spawn_many."));
    }
    if request.project.trim().is_empty() || request.concurrency == 0 {
        return Err(invalid(
            "API requires a nonempty project and positive concurrency.",
        ));
    }
    if request.tasks.is_empty()
        || request.tasks.len() > MAX_TASKS
        || request
            .tasks
            .iter()
            .any(|task| task.prompt.len() > MAX_PROMPT_BYTES)
    {
        return Err(invalid(
            "API requires 1–128 tasks with at most 64 KiB per prompt.",
        ));
    }
    // Count without allocating another copy; typed callers obey the transport bound too.
    serde_json::to_writer(RequestBudget(MAX_REQUEST_BYTES), &request)
        .map_err(|_| invalid("API request exceeds 1 MiB."))?;
    let mut options = RunOptions {
        cancellation,
        ..RunOptions::default()
    };
    if let Some(seconds) = request.options.timeout_seconds {
        options.limits.timeout = Duration::from_secs(seconds);
    }
    if let Some(limit) = request.options.copy_limit_bytes {
        options.copy_limit_bytes = limit;
    }
    if let Some(limit) = request.options.max_log_bytes {
        options.max_log_bytes = limit;
    }
    if let Some(limit) = request.options.max_artifact_bytes {
        options.max_artifact_bytes = limit;
    }
    let prompts = PromptDirectory::new()?;
    let mut inputs = Vec::with_capacity(request.tasks.len());
    for (index, task) in request.tasks.into_iter().enumerate() {
        let path = prompts.0.join(index.to_string());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| invalid("Cannot create private API prompt input."))?;
        file.write_all(task.prompt.as_bytes())
            .map_err(|_| invalid("Cannot write private API prompt input."))?;
        inputs.push(TaskInput {
            id: task.id,
            prompt_file: path,
            harness: task.harness,
            request: task.request,
        });
    }
    let batch = crate::run::spawn_many(
        root,
        &request.project,
        inputs,
        request.concurrency,
        &BatchHarnesses {
            registry,
            options: registry_options,
            default: request.default_harness.as_ref(),
        },
        &options,
    )?;
    Ok(BatchResponse {
        version: 1,
        op: "spawn_many".into(),
        batch,
    })
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

struct PromptDirectory(PathBuf);
impl PromptDirectory {
    fn new() -> Result<Self, FabricError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let clock = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("Cannot create private API prompt directory."))?;
        let path = std::env::temp_dir().join(format!(
            "fabric-api-{}-{}-{}",
            std::process::id(),
            clock.as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| invalid("Cannot create private API prompt directory."))?;
        Ok(Self(path))
    }
}
impl Drop for PromptDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn invalid(message: &str) -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidArguments,
        message: message.into(),
    }
}
