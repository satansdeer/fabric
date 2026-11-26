//! Bounded local observations. Raw content is a separate, explicit opt-in.
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fabric_domain::{ErrorKind, FabricError, RunId, RunState, WorkspaceState};
use fabric_runner::activity::{Activity, SharedActivity, timestamp_ms};
use fabric_store::Store;
use serde::{Deserialize, Serialize};

pub const MAX_READ_BYTES: usize = 65536;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub timeout_seconds: u64,
    pub max_log_bytes: u64,
    pub max_artifact_bytes: u64,
    pub copy_limit_bytes: u64,
    pub probe_context_validated_at_ms: u64,
    pub artifact_capture_started_at_ms: Option<u64>,
    pub artifacts_captured_at_ms: Option<u64>,
    pub cleanup_observed_at_ms: Option<u64>,
    pub sampled_at_ms: u64,
    pub activity: Activity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_meter_failure: Option<crate::workspace::storage::MeterFailure>,
}

impl Progress {
    pub(crate) fn new(options: &crate::run::RunOptions) -> Self {
        Self {
            timeout_seconds: options.limits.timeout.as_secs(),
            max_log_bytes: options.max_log_bytes,
            max_artifact_bytes: options.max_artifact_bytes,
            copy_limit_bytes: options.copy_limit_bytes,
            probe_context_validated_at_ms: timestamp_ms(),
            artifact_capture_started_at_ms: None,
            artifacts_captured_at_ms: None,
            cleanup_observed_at_ms: None,
            sampled_at_ms: timestamp_ms(),
            activity: Activity::default(),
            storage_meter_failure: None,
        }
    }

    pub(crate) fn save(
        &self,
        store: &Store,
        run: &RunId,
        activity: &SharedActivity,
    ) -> Result<(), FabricError> {
        let mut snapshot = self.clone();
        snapshot.activity = *activity.lock().unwrap();
        snapshot.sampled_at_ms = timestamp_ms();
        store
            .record_observation(run, &serde_json::to_string(&snapshot).map_err(error)?)
            .map_err(error)
    }
}

pub(crate) fn monitor(
    store: Store,
    run: &RunId,
    progress: &Progress,
    activity: &SharedActivity,
    done: &AtomicBool,
) -> Result<(), FabricError> {
    loop {
        progress.save(&store, run, activity)?;
        if done.load(Ordering::Acquire) {
            return Ok(());
        }
        std::thread::park_timeout(Duration::from_millis(250));
    }
}

#[derive(Clone, Copy, Debug)]
pub enum RawStream {
    Stdout,
    Stderr,
    Events,
}

impl RawStream {
    fn file(self) -> (&'static str, &'static str) {
        match self {
            Self::Stdout => ("stdout", "stdout.log"),
            Self::Stderr => ("stderr", "stderr.log"),
            Self::Events => ("events", "events.jsonl"),
        }
    }
}

#[derive(Serialize)]
pub struct LogRead {
    pub run_id: RunId,
    pub project_id: String,
    pub batch_id: Option<String>,
    pub attempt_id: Option<String>,
    pub harness: String,
    pub state: RunState,
    pub workspace_state: Option<WorkspaceState>,
    pub terminal: bool,
    pub next_cursor: u64,
    pub changed: bool,
    pub queued_at: Option<i64>,
    pub lifecycle_observed_at: Option<i64>,
    pub created_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub termination: Option<&'static str>,
    pub configured_deadline_at_ms: Option<u64>,
    pub verified_progress: &'static str,
    pub process_observation: &'static str,
    pub last_activity_at_ms: Option<u64>,
    pub progress: Option<Progress>,
    pub raw: Option<String>,
    pub raw_eof: bool,
}

/// Cursors are per run and per stream. No caller-controlled filesystem path.
pub fn read(
    root: &Path,
    id: &RunId,
    after: u64,
    limit: usize,
    raw: Option<RawStream>,
) -> Result<LogRead, FabricError> {
    if !(1..=MAX_READ_BYTES).contains(&limit) || after > i64::MAX as u64 {
        return Err(error("Invalid log cursor or byte limit (1..65536)."));
    }
    super::workspace::private_directory(root)?;
    if std::fs::canonicalize(root).map_err(|_| error("Log root unavailable"))? != root {
        return Err(error("Log root must be canonical."));
    }
    super::workspace::private_file(&root.join("fabric.db"))?;
    let store = Store::open_existing_read_only(root.join("fabric.db"))
        .map_err(|_| error("Log metadata unavailable"))?;
    let run = store
        .run(id)
        .map_err(|_| error("Log metadata unavailable"))?;
    let identity = store
        .async_log_identity(id)
        .map_err(|_| error("Log metadata unavailable"))?;
    let (state, project_id, harness, queued_at, lifecycle_observed_at, batch) =
        if let Some((project, batch, state, harness, accepted, observed)) = identity {
            (
                state,
                project.to_string(),
                harness,
                Some(accepted),
                Some(observed),
                Some(batch),
            )
        } else if let Some(run) = &run {
            (
                run.state,
                run.project_id.to_string(),
                run.harness.clone(),
                Some(run.created_at),
                Some(run.updated_at),
                None,
            )
        } else {
            return Err(error("Unknown run in this Fabric root."));
        };
    let workspace = run
        .as_ref()
        .map(|run| store.workspace(&run.workspace_id))
        .transpose()
        .map_err(|_| error("Log ownership unavailable"))?
        .flatten();
    let observation = store
        .observation(id)
        .map_err(|_| error("Log metadata unavailable"))?;
    let (revision, progress) = match observation {
        Some((revision, json)) => (
            revision,
            Some(
                serde_json::from_str::<Progress>(&json)
                    .map_err(|_| error("Invalid observation metadata"))?,
            ),
        ),
        None => (0, None),
    };
    let phase = match state {
        RunState::Queued => 1,
        RunState::Preparing => 2,
        RunState::Running => 3,
        RunState::Finalizing => 4,
        _ => 5,
    };
    let cursor = revision
        .checked_mul(16)
        .and_then(|value| value.checked_add(phase))
        .ok_or_else(|| error("Observation cursor overflow"))?;
    let attempts = store
        .worker_attempts(id)
        .map_err(|_| error("Attempt metadata unavailable"))?;
    let attempt = attempts.last();
    let termination = attempt
        .and_then(|attempt| attempt.termination_reason.as_deref())
        .map(|reason| match reason {
            "cancelled" => "cancelled",
            "timeout" => "timeout",
            "native_failure" => "native_failure",
            "workspace_limit" => "workspace_limit",
            "storage_pressure" => "storage_pressure",
            "storage_meter_failed" => "storage_meter_failed",
            _ => "execution_failed",
        });
    let process_observation = match progress.as_ref().map(|progress| &progress.activity) {
        Some(activity) if activity.process_exited_at_ms.is_some() => "observed_exited",
        Some(activity) if activity.process_alive_observed_at_ms.is_some() => {
            "observed_alive_at_timestamp"
        }
        _ => "not_observed",
    };
    let deadline = progress.as_ref().and_then(|progress| {
        progress
            .activity
            .process_started_at_ms?
            .checked_add(progress.timeout_seconds.checked_mul(1000)?)
    });
    let last_activity_at_ms = progress.as_ref().and_then(|progress| {
        progress
            .activity
            .last_output_at_ms
            .max(progress.activity.last_event_at_ms)
    });
    let mut report = LogRead {
        run_id: id.clone(),
        project_id,
        batch_id: batch.map(|batch| batch.to_string()),
        attempt_id: attempt.map(|_| format!("a-{id}")),
        harness,
        state,
        workspace_state: workspace.as_ref().map(|workspace| workspace.state),
        terminal: state.is_terminal(),
        next_cursor: cursor,
        changed: cursor > after,
        queued_at,
        lifecycle_observed_at,
        created_at: run.as_ref().map(|run| run.created_at),
        started_at: run.as_ref().and_then(|run| run.started_at),
        finished_at: run.as_ref().and_then(|run| run.finished_at),
        termination,
        configured_deadline_at_ms: deadline,
        verified_progress: "not_assessed",
        process_observation,
        last_activity_at_ms,
        progress,
        raw: None,
        raw_eof: false,
    };
    if let Some(stream) = raw {
        let opened = if let Some(run) = &run {
            match open_log(root, &store, run, stream) {
                Ok(file) => file,
                Err(original) => {
                    // Capture/reclaim may replace the live container between snapshots.
                    let fresh = store
                        .run(id)
                        .map_err(|_| error("Log metadata unavailable"))?;
                    match fresh.filter(|fresh| {
                        run.artifact_manifest_id.is_none() && fresh.artifact_manifest_id.is_some()
                    }) {
                        Some(fresh) => open_log(root, &store, &fresh, stream)?,
                        None => return Err(original),
                    }
                }
            }
        } else {
            None
        };
        let Some(mut file) = opened else {
            if after != 0 {
                return Err(error("Cursor exceeds available log"));
            }
            report.next_cursor = 0;
            report.changed = false;
            report.raw = Some(String::new());
            report.raw_eof = report.terminal;
            return Ok(report);
        };
        let length = file
            .metadata()
            .map_err(|_| error("Log metadata unavailable"))?
            .len();
        if after > length {
            return Err(error("Cursor exceeds available log"));
        }
        file.seek(SeekFrom::Start(after))
            .map_err(|_| error("Log seek failed"))?;
        let mut bytes = vec![0; limit.min((length - after) as usize)];
        let count = file
            .read(&mut bytes)
            .map_err(|_| error("Log read failed"))?;
        bytes.truncate(count);
        report.next_cursor = after + count as u64;
        report.changed = count != 0;
        // JSON escaping prevents raw terminal-control bytes from executing in the viewer.
        report.raw = Some(String::from_utf8_lossy(&bytes).into_owned());
        report.raw_eof = report.terminal && report.next_cursor == length;
    } else if after > cursor {
        return Err(error("Cursor exceeds current observation"));
    }
    Ok(report)
}

fn open_log(
    root: &Path,
    store: &Store,
    run: &fabric_store::Run,
    stream: RawStream,
) -> Result<Option<std::fs::File>, FabricError> {
    let (kind, name) = stream.file();
    if let Some(manifest) = &run.artifact_manifest_id {
        if manifest.as_str() != format!("a-{}", run.run_id) {
            return Err(error("Foreign artifact manifest"));
        }
        let directory = root.join("artifacts").join(manifest.as_str());
        for path in [root.join("artifacts"), directory.clone()] {
            super::workspace::private_directory(&path)?;
            if std::fs::canonicalize(&path).map_err(|_| error("Log directory unavailable"))? != path
            {
                return Err(error("Foreign log directory"));
            }
        }
        let expected = format!("artifacts/{manifest}/{name}");
        let files = store
            .artifacts(&run.run_id)
            .map_err(|_| error("Artifact metadata unavailable"))?;
        let mut matching = files.iter().filter(|file| file.kind == kind);
        let registered = matching
            .next()
            .ok_or_else(|| error("Missing registered log"))?;
        if matching.next().is_some() || registered.relative_path != expected {
            return Err(error("Foreign registered log"));
        }
        let file = super::artifacts::open(&directory.join(name))
            .map_err(|_| error("Unsafe retained log"))?;
        let metadata = file
            .metadata()
            .map_err(|_| error("Log metadata unavailable"))?;
        if metadata.len() != registered.size_bytes || metadata.mode() & 0o222 != 0 {
            return Err(error("Retained log is unsealed or changed"));
        }
        Ok(Some(file))
    } else {
        let workspace = store
            .workspace(&run.workspace_id)
            .map_err(|_| error("Log ownership unavailable"))?
            .ok_or_else(|| error("Missing owned workspace"))?;
        // Ownership is committed before the backend creates the tree and marker.
        // A pending fork has no readable logs; do not open its filesystem paths.
        if run.state == RunState::Preparing && workspace.retention_reason.is_some() {
            return Ok(None);
        }
        let owned = super::workspace::validate_owned_workspace(root, run, &workspace)
            .map_err(|_| error("Unsafe or foreign run workspace"))?;
        let container = owned
            .path()
            .parent()
            .ok_or_else(|| error("Missing log container"))?;
        let path = container.join(name);
        match std::fs::symlink_metadata(&path) {
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && run.state == RunState::Preparing =>
            {
                Ok(None)
            }
            _ => super::artifacts::open(&path)
                .map(Some)
                .map_err(|_| error("Unsafe or unavailable live log")),
        }
    }
}

fn error(message: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Storage,
        message: message.to_string(),
    }
}
