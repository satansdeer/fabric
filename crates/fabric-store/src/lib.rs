//! Durable local records. Committed ownership is required before filesystem use.

use std::{fmt, fs::OpenOptions, path::Path, str::FromStr, time::Duration};

use fabric_domain::{
    ArtifactManifestId, BatchId, CaptureMetadata, EnvironmentId, FabricError, HarnessId, HarnessVersion,
    PreparedStateId, PreparedStateStatus, ProjectId, RunId, RunState, SourceProvenance,
    WorkspaceId, WorkspaceState,
};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

mod environments;
mod queue;
pub use environments::EnvironmentRecord;
use queue::insert_task_source;
pub use queue::{
    AsyncBatch, AsyncEvent, AsyncLogIdentity, AsyncTask, AsyncTaskStatus, NewAsyncTask,
};

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Domain(FabricError),
    NotFound,
    Conflict,
    InvalidRecord(&'static str),
    UnsupportedSchema(i64),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "database: {error}"),
            Self::Io(error) => write!(f, "database file: {error}"),
            Self::Domain(error) => error.fmt(f),
            Self::NotFound => f.write_str("record not found"),
            Self::Conflict => f.write_str("record changed; reload before retrying"),
            Self::InvalidRecord(reason) => f.write_str(reason),
            Self::UnsupportedSchema(version) => write!(f, "unsupported database schema {version}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<FabricError> for StoreError {
    fn from(value: FabricError) -> Self {
        Self::Domain(value)
    }
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Project {
    pub project_id: ProjectId,
    pub name: String,
    pub source_path: String,
    pub default_ref: String,
    /// Unix seconds, as with all timestamps in this crate.
    pub created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PreparedState {
    pub state_id: PreparedStateId,
    pub project_id: ProjectId,
    pub preparation_key: String,
    pub source_commit_sha: String,
    pub recipe_digest: String,
    pub harness_environment_digest: Option<String>,
    pub prepared_path: String,
    pub status: PreparedStateStatus,
    pub created_at: i64,
    pub verified_at: Option<i64>,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Run {
    pub run_id: RunId,
    pub batch_id: Option<BatchId>,
    pub project_id: ProjectId,
    pub prepared_state_id: PreparedStateId,
    pub base_commit_sha: String,
    pub harness: String,
    pub harness_version: Option<String>,
    pub prompt_digest: String,
    pub state: RunState,
    pub workspace_id: WorkspaceId,
    /// Opaque supervisor metadata; never sufficient authority to signal a PID.
    pub pid_metadata: Option<String>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error_kind: Option<String>,
    pub artifact_manifest_id: Option<ArtifactManifestId>,
    pub capture: Option<CaptureMetadata>,
}

/// Durable evidence for one invocation; independent of Run and workspace lifetime.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkerAttempt {
    pub attempt_id: String,
    pub run_id: RunId,
    pub harness_requested: HarnessId,
    pub harness_resolved: HarnessId,
    pub harness_version: String,
    pub model_requested: Option<String>,
    pub model_resolved: Option<String>,
    pub profile_digest: Option<String>,
    pub backend_id: String,
    pub event_cursor: u64,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub termination_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Workspace {
    pub workspace_id: WorkspaceId,
    pub run_id: RunId,
    pub managed_path: String,
    pub state_id: PreparedStateId,
    pub state: WorkspaceState,
    /// Expiry is not permission to delete or proof of process identity.
    pub lease_expires_at: Option<i64>,
    pub usage_bytes: u64,
    pub retention_reason: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct RunWorkspaceTransition {
    pub expected_run: RunState,
    pub next_run: RunState,
    pub expected_workspace: WorkspaceState,
    pub next_workspace: WorkspaceState,
    pub updated_at: i64,
}

pub struct Store {
    connection: Connection,
}

#[derive(Debug)]
pub struct BatchTask {
    pub task_id: String,
    pub run_id: RunId,
    pub prompt_file: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// Verified immutable files, relative to the canonical Fabric root.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Artifact {
    pub kind: String,
    pub relative_path: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Immutable prelaunch evidence needed to finish capture without replaying execution.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureContext {
    pub profile: fabric_domain::ProfileEvidence,
    pub artifact_limit_bytes: u64,
}

/// Manager-pinned checkout and effective recipe output, established while PREPARING.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEvidence {
    pub checkout_sha256: String,
    pub prepared_sha256: String,
}

/// Retained independently of the writable workspace and captured result artifacts.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchReceipt {
    pub run_id: RunId,
    pub attempt_id: String,
    pub workspace_id: WorkspaceId,
    pub workspace_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_inode: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteLaunchWitness>,
    pub prepared_state_id: PreparedStateId,
    pub source_commit_sha: String,
    pub preparation_key: String,
    pub recipe_digest: String,
    pub source: SourceEvidence,
    pub actual_sha256: String,
}

/// Observed remote source, never a controller inode, path, PID or deletion grant.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteLaunchWitness {
    pub target_json: String,
    pub environment_id: EnvironmentId,
    pub resource: serde_json::Value,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    pub instance: String,
}

impl Store {
    /// Opens a file-backed WAL database. The parent directory must already exist.
    /// New database files are owner-only on Unix; callers own storage-root validation.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        use std::time::Instant;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path.as_ref()) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(StoreError::Io(error)),
        }
        let mut connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let busy_timeout = Duration::from_secs(5);
        connection.busy_timeout(busy_timeout)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        // A journal-mode upgrade can return BUSY without invoking SQLite's busy
        // handler. Retry only this idempotent transition, sharing one deadline.
        let deadline = Instant::now() + busy_timeout;
        let mode: String = loop {
            connection.busy_timeout(deadline.saturating_duration_since(Instant::now()))?;
            match connection.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)) {
                Ok(mode) => break mode,
                Err(error)
                    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) =>
                {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(error.into());
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                    if Instant::now() >= deadline {
                        return Err(error.into());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        };
        connection.busy_timeout(busy_timeout)?;
        if mode != "wal" {
            return Err(StoreError::InvalidRecord(
                "file-backed WAL database required",
            ));
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = transaction.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => transaction.execute_batch(include_str!("schema.sql"))?,
            1..=19 => {}
            other => return Err(StoreError::UnsupportedSchema(other)),
        }
        if version < 2 {
            transaction.execute_batch("ALTER TABLE runs ADD COLUMN capture_metadata TEXT;")?;
            transaction.pragma_update(None, "user_version", 2)?;
        }
        if version < 3 {
            transaction.execute_batch(
                "CREATE TABLE batches (
                    batch_id TEXT PRIMARY KEY NOT NULL,
                    prepared_state_id TEXT NOT NULL REFERENCES prepared_states(state_id),
                    succeeded INTEGER CHECK(succeeded IN (0, 1))
                 );
                 CREATE TABLE batch_tasks (
                    batch_id TEXT NOT NULL REFERENCES batches(batch_id),
                    position INTEGER NOT NULL CHECK(position >= 0),
                    task_id TEXT NOT NULL CHECK(length(task_id) > 0),
                    run_id TEXT NOT NULL UNIQUE,
                    prompt_file TEXT NOT NULL CHECK(length(prompt_file) > 0),
                    result TEXT,
                    error TEXT,
                    PRIMARY KEY(batch_id, task_id),
                    UNIQUE(batch_id, position),
                    CHECK(result IS NULL OR error IS NULL)
                 );
                 CREATE TRIGGER run_requires_batch_task BEFORE INSERT ON runs
                 WHEN NEW.batch_id IS NOT NULL AND NOT EXISTS (
                    SELECT 1 FROM batch_tasks t JOIN batches b ON b.batch_id = t.batch_id
                    WHERE t.run_id = NEW.run_id AND t.batch_id = NEW.batch_id
                      AND b.prepared_state_id = NEW.prepared_state_id
                 )
                 BEGIN SELECT RAISE(ABORT, 'run requires matching batch task and prepared state'); END;",
            )?;
            transaction.pragma_update(None, "user_version", 3)?;
        }
        if version < 4 {
            transaction.execute_batch("ALTER TABLE runs ADD COLUMN exit_signal INTEGER;")?;
            transaction.pragma_update(None, "user_version", 4)?;
        }
        if version < 5 {
            transaction.execute_batch(
                "CREATE TABLE artifacts (
                    run_id TEXT NOT NULL REFERENCES runs(run_id),
                    kind TEXT NOT NULL,
                    relative_path TEXT NOT NULL UNIQUE,
                    sha256 TEXT NOT NULL CHECK(length(sha256) = 64),
                    size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
                    PRIMARY KEY(run_id, kind)
                 );
                 CREATE TRIGGER immutable_artifact_update BEFORE UPDATE ON artifacts
                 BEGIN SELECT RAISE(ABORT, 'artifacts are immutable'); END;
                 CREATE TRIGGER immutable_artifact_delete BEFORE DELETE ON artifacts
                 BEGIN SELECT RAISE(ABORT, 'artifacts are immutable'); END;
                 CREATE TRIGGER immutable_manifest BEFORE UPDATE OF artifact_manifest_id ON runs
                 WHEN (OLD.artifact_manifest_id IS NOT NULL
                    AND NEW.artifact_manifest_id IS NOT OLD.artifact_manifest_id)
                   OR (OLD.artifact_manifest_id IS NULL AND NEW.artifact_manifest_id IS NOT NULL
                       AND (OLD.state != 'FINALIZING'
                           OR NEW.state NOT IN ('SUCCEEDED', 'FAILED', 'CANCELLED')
                           OR NOT EXISTS (SELECT 1 FROM artifacts
                               WHERE run_id = NEW.run_id AND kind = 'manifest'
                                 AND relative_path = 'artifacts/' || NEW.artifact_manifest_id || '/manifest.json')))
                 BEGIN SELECT RAISE(ABORT, 'manifest association is immutable'); END;",
            )?;
            transaction.pragma_update(None, "user_version", 5)?;
        }
        if version < 6 {
            transaction.execute_batch(
                "CREATE TABLE cleanup_intents (
                    workspace_id TEXT PRIMARY KEY NOT NULL REFERENCES workspaces(workspace_id),
                    authority TEXT NOT NULL CHECK(length(authority) > 0)
                 );
                 CREATE TRIGGER immutable_cleanup_intent_insert BEFORE INSERT ON cleanup_intents
                 WHEN EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id = NEW.workspace_id)
                 BEGIN SELECT RAISE(ABORT, 'cleanup intent is immutable'); END;
                 CREATE TRIGGER immutable_cleanup_intent_update BEFORE UPDATE ON cleanup_intents
                 BEGIN SELECT RAISE(ABORT, 'cleanup intent is immutable'); END;
                 CREATE TRIGGER immutable_cleanup_intent_delete BEFORE DELETE ON cleanup_intents
                 BEGIN SELECT RAISE(ABORT, 'cleanup intent is immutable'); END;
                 CREATE TRIGGER cleanup_workspace_update BEFORE UPDATE ON workspaces
                 WHEN EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id = OLD.workspace_id)
                   AND NOT (
                     NEW.workspace_id IS OLD.workspace_id AND NEW.run_id IS OLD.run_id
                     AND NEW.managed_path IS OLD.managed_path AND NEW.state_id IS OLD.state_id
                     AND NEW.lease_expires_at IS OLD.lease_expires_at
                     AND NEW.retention_reason IS OLD.retention_reason
                     AND ((NEW.state IS OLD.state AND NEW.usage_bytes IS OLD.usage_bytes)
                       OR (OLD.state = 'CLEANUP_PENDING' AND NEW.state = 'RECLAIMED'
                           AND NEW.usage_bytes = 0)))
                 BEGIN SELECT RAISE(ABORT, 'workspace has cleanup intent'); END;
                 CREATE TRIGGER cleanup_workspace_delete BEFORE DELETE ON workspaces
                 WHEN EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id = OLD.workspace_id)
                 BEGIN SELECT RAISE(ABORT, 'workspace has cleanup intent'); END;
                 CREATE TRIGGER cleanup_workspace_insert BEFORE INSERT ON workspaces
                 WHEN EXISTS (SELECT 1 FROM cleanup_intents c
                   JOIN workspaces w ON w.workspace_id = c.workspace_id
                   WHERE w.workspace_id = NEW.workspace_id OR w.run_id = NEW.run_id
                     OR w.managed_path = NEW.managed_path)
                 BEGIN SELECT RAISE(ABORT, 'workspace has cleanup intent'); END;
                 CREATE TRIGGER cleanup_run_update BEFORE UPDATE ON runs
                 WHEN EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id = OLD.workspace_id)
                 BEGIN SELECT RAISE(ABORT, 'run has cleanup intent'); END;
                 CREATE TRIGGER cleanup_run_delete BEFORE DELETE ON runs
                 WHEN EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id = OLD.workspace_id)
                 BEGIN SELECT RAISE(ABORT, 'run has cleanup intent'); END;
                 CREATE TRIGGER cleanup_run_insert BEFORE INSERT ON runs
                 WHEN EXISTS (SELECT 1 FROM cleanup_intents c
                   JOIN runs r ON r.workspace_id = c.workspace_id
                   WHERE r.run_id = NEW.run_id OR r.workspace_id = NEW.workspace_id)
                 BEGIN SELECT RAISE(ABORT, 'run has cleanup intent'); END;",
            )?;
            transaction.pragma_update(None, "user_version", 6)?;
        }
        if version < 7 {
            transaction.execute_batch(
                "CREATE TABLE storage_reservations (
                    token TEXT PRIMARY KEY NOT NULL,
                    state_id TEXT REFERENCES prepared_states(state_id),
                    bytes INTEGER NOT NULL CHECK(bytes > 0)
                 );
                 CREATE TABLE cache_evictions (
                    state_id TEXT PRIMARY KEY NOT NULL,
                    container_name TEXT NOT NULL UNIQUE,
                    authority TEXT NOT NULL
                 );
                 CREATE TRIGGER immutable_cache_eviction_update BEFORE UPDATE ON cache_evictions
                 BEGIN SELECT RAISE(ABORT, 'cache eviction intent is immutable'); END;",
            )?;
            transaction.pragma_update(None, "user_version", 7)?;
        }
        if version < 8 {
            transaction.execute_batch(
                "CREATE TABLE worker_leases (
                    token TEXT PRIMARY KEY NOT NULL,
                    authority TEXT NOT NULL,
                    expires_at INTEGER NOT NULL
                 );",
            )?;
            transaction.pragma_update(None, "user_version", 8)?;
        }
        if version < 9 {
            transaction.execute_batch(include_str!("worker_attempts.sql"))?;
            transaction.pragma_update(None, "user_version", 9)?;
        }
        if version < 10 {
            transaction.execute_batch(include_str!("async_queue.sql"))?;
            transaction.pragma_update(None, "user_version", 10)?;
        }
        if version < 11 {
            transaction.execute_batch(include_str!("task_sources.sql"))?;
            transaction.pragma_update(None, "user_version", 11)?;
        }
        if version < 12 {
            transaction.execute_batch(
                "CREATE TABLE run_capture_contexts (
                    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
                    context_json TEXT NOT NULL CHECK(json_valid(context_json))
                      CHECK(json_type(context_json) IS 'object')
                      CHECK(json_type(context_json, '$.profile') IS 'object')
                      CHECK(json_type(context_json, '$.artifact_limit_bytes') IS 'integer')
                      CHECK(typeof(json_extract(context_json, '$.artifact_limit_bytes')) = 'integer')
                      CHECK(json_extract(context_json, '$.artifact_limit_bytes') > 0)
                      CHECK(json_type(context_json, '$.profile.digest') IS 'text')
                      CHECK(length(json_extract(context_json, '$.profile.digest')) = 64)
                      CHECK(json_extract(context_json, '$.profile.digest') NOT GLOB '*[^0-9a-f]*')
                      CHECK(json_type(context_json, '$.profile.native_config_fingerprint') IS 'text')
                      CHECK(length(json_extract(context_json, '$.profile.native_config_fingerprint')) = 64)
                      CHECK(json_extract(context_json, '$.profile.native_config_fingerprint') NOT GLOB '*[^0-9a-f]*')
                      CHECK(json_type(context_json, '$.profile.portable') IS 'true'
                         OR json_type(context_json, '$.profile.portable') IS 'false')
                      CHECK(json_type(context_json, '$.profile.diagnostics') IS 'array')
                 );
                 CREATE TRIGGER valid_capture_context BEFORE INSERT ON run_capture_contexts
                 WHEN (SELECT count(*) FROM json_each(NEW.context_json)) != 2
                   OR EXISTS (SELECT 1 FROM json_each(NEW.context_json)
                        WHERE key NOT IN ('profile', 'artifact_limit_bytes'))
                   OR (SELECT count(*) FROM json_each(NEW.context_json, '$.profile')) != 4
                   OR EXISTS (SELECT 1 FROM json_each(NEW.context_json, '$.profile')
                        WHERE key NOT IN ('digest', 'native_config_fingerprint', 'portable', 'diagnostics'))
                 BEGIN SELECT RAISE(ABORT, 'invalid capture context fields'); END;
                 CREATE TRIGGER valid_capture_context_evidence BEFORE INSERT ON run_capture_contexts
                 WHEN NOT EXISTS (
                    SELECT 1 FROM runs r JOIN workspaces w ON w.workspace_id = r.workspace_id
                    JOIN worker_attempts a ON a.run_id = r.run_id
                    WHERE r.run_id = NEW.run_id AND r.state = 'PREPARING'
                      AND r.started_at IS NULL AND r.finished_at IS NULL
                      AND r.pid_metadata IS NULL AND r.exit_code IS NULL AND r.exit_signal IS NULL
                      AND r.error_kind IS NULL AND r.capture_metadata IS NULL
                      AND r.artifact_manifest_id IS NULL
                      AND w.run_id = r.run_id AND w.state_id = r.prepared_state_id
                      AND w.state = 'ACTIVE' AND w.retention_reason IS NULL
                      AND a.harness_resolved = r.harness
                      AND a.profile_digest = json_extract(NEW.context_json, '$.profile.digest')
                      AND a.started_at IS NULL AND a.finished_at IS NULL
                      AND NOT EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id = w.workspace_id)
                 ) OR EXISTS (
                    SELECT 1 FROM worker_attempts a WHERE a.run_id = NEW.run_id
                      AND (a.profile_digest IS NOT json_extract(NEW.context_json, '$.profile.digest')
                        OR a.started_at IS NOT NULL OR a.finished_at IS NOT NULL)
                 )
                 BEGIN SELECT RAISE(ABORT, 'capture context requires matching prelaunch evidence'); END;
                 CREATE TRIGGER immutable_capture_context_insert BEFORE INSERT ON run_capture_contexts
                 WHEN EXISTS (SELECT 1 FROM run_capture_contexts WHERE run_id = NEW.run_id)
                 BEGIN SELECT RAISE(ABORT, 'capture context is immutable'); END;
                 CREATE TRIGGER immutable_capture_context_update BEFORE UPDATE ON run_capture_contexts
                 BEGIN SELECT RAISE(ABORT, 'capture context is immutable'); END;
                 CREATE TRIGGER immutable_capture_context_delete BEFORE DELETE ON run_capture_contexts
                 BEGIN SELECT RAISE(ABORT, 'capture context is immutable'); END;
                 DROP TRIGGER immutable_manifest;
                 CREATE TRIGGER immutable_manifest BEFORE UPDATE OF artifact_manifest_id ON runs
                 WHEN (OLD.artifact_manifest_id IS NOT NULL
                    AND NEW.artifact_manifest_id IS NOT OLD.artifact_manifest_id)
                   OR (OLD.artifact_manifest_id IS NULL AND NEW.artifact_manifest_id IS NOT NULL
                     AND (NOT EXISTS (SELECT 1 FROM artifacts
                            WHERE run_id = NEW.run_id AND kind = 'manifest'
                              AND relative_path = 'artifacts/' || NEW.artifact_manifest_id || '/manifest.json')
                       OR NOT (
                         (OLD.state = 'FINALIZING' AND NEW.state IN ('SUCCEEDED', 'FAILED', 'CANCELLED'))
                         OR (OLD.state = 'NEEDS_ATTENTION' AND NEW.state = OLD.state
                           AND OLD.capture_metadata IS NOT NULL
                           AND NEW.capture_metadata IS OLD.capture_metadata
                           AND NEW.run_id IS OLD.run_id AND NEW.batch_id IS OLD.batch_id
                           AND NEW.project_id IS OLD.project_id
                           AND NEW.prepared_state_id IS OLD.prepared_state_id
                           AND NEW.base_commit_sha IS OLD.base_commit_sha
                           AND NEW.harness IS OLD.harness AND NEW.harness_version IS OLD.harness_version
                           AND NEW.prompt_digest IS OLD.prompt_digest
                           AND NEW.workspace_id IS OLD.workspace_id
                           AND NEW.pid_metadata IS OLD.pid_metadata
                           AND NEW.exit_code IS OLD.exit_code AND NEW.exit_signal IS OLD.exit_signal
                           AND NEW.created_at IS OLD.created_at AND NEW.updated_at IS OLD.updated_at
                           AND NEW.started_at IS OLD.started_at AND NEW.finished_at IS OLD.finished_at
                           AND NEW.error_kind IS OLD.error_kind)
                       )))
                 BEGIN SELECT RAISE(ABORT, 'manifest association is immutable'); END;",
            )?;
            transaction.pragma_update(None, "user_version", 12)?;
        }
        if version < 13 {
            transaction.execute_batch(
                "CREATE TABLE run_observations (
                    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
                    revision INTEGER NOT NULL CHECK(revision > 0),
                    metadata TEXT NOT NULL CHECK(length(CAST(metadata AS BLOB)) <= 8192)
                      CHECK(json_valid(metadata))
                 );",
            )?;
            transaction.pragma_update(None, "user_version", 13)?;
        }
        if version < 14 {
            transaction.execute_batch(
                "CREATE TABLE run_targets (
                    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
                    pin TEXT NOT NULL CHECK(length(CAST(pin AS BLOB)) <= 16384) CHECK(json_valid(pin))
                 );
                 CREATE TRIGGER target_prelaunch BEFORE INSERT ON run_targets
                 WHEN NOT EXISTS (SELECT 1 FROM runs WHERE run_id = NEW.run_id
                    AND state = 'PREPARING' AND started_at IS NULL AND pid_metadata IS NULL)
                 BEGIN SELECT RAISE(ABORT, 'target binding requires prelaunch run'); END;
                 CREATE TRIGGER immutable_target_update BEFORE UPDATE ON run_targets
                 BEGIN SELECT RAISE(ABORT, 'target binding is immutable'); END;
                 CREATE TRIGGER immutable_target_delete BEFORE DELETE ON run_targets
                 BEGIN SELECT RAISE(ABORT, 'target binding is immutable'); END;",
            )?;
            transaction.pragma_update(None, "user_version", 14)?;
        }
        if version < 15 {
            transaction.execute_batch(
                "DROP TRIGGER cleanup_run_update;
                 CREATE TRIGGER cleanup_run_update BEFORE UPDATE ON runs
                 WHEN EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id = OLD.workspace_id)
                   AND NOT (
                     NEW.state IS CASE WHEN OLD.state = 'SUCCEEDED' THEN 'FAILED' ELSE OLD.state END
                     AND NEW.error_kind = 'target_protocol'
                     AND NEW.run_id IS OLD.run_id AND NEW.batch_id IS OLD.batch_id
                     AND NEW.project_id IS OLD.project_id
                     AND NEW.prepared_state_id IS OLD.prepared_state_id
                     AND NEW.base_commit_sha IS OLD.base_commit_sha
                     AND NEW.harness IS OLD.harness AND NEW.harness_version IS OLD.harness_version
                     AND NEW.prompt_digest IS OLD.prompt_digest
                     AND NEW.workspace_id IS OLD.workspace_id AND NEW.pid_metadata IS OLD.pid_metadata
                     AND NEW.exit_code IS OLD.exit_code AND NEW.exit_signal IS OLD.exit_signal
                     AND NEW.created_at IS OLD.created_at AND NEW.updated_at IS OLD.updated_at
                     AND NEW.started_at IS OLD.started_at AND NEW.finished_at IS OLD.finished_at
                     AND NEW.artifact_manifest_id IS OLD.artifact_manifest_id
                     AND NEW.capture_metadata IS OLD.capture_metadata)
                 BEGIN SELECT RAISE(ABORT, 'run has cleanup intent'); END;",
            )?;
            transaction.pragma_update(None, "user_version", 15)?;
        }
        if version < 16 {
            transaction.execute_batch(
                "CREATE TABLE connector_deliveries (
                    namespace TEXT NOT NULL,
                    nonce TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    accepted_at INTEGER NOT NULL,
                    batch_id TEXT NOT NULL REFERENCES async_batches(batch_id),
                    PRIMARY KEY(namespace, nonce)
                 );
                 CREATE INDEX connector_rate ON connector_deliveries(namespace, accepted_at);",
            )?;
            transaction.pragma_update(None, "user_version", 16)?;
        }
        if version < 17 {
            transaction.execute_batch(include_str!("source_receipts.sql"))?;
            transaction.pragma_update(None, "user_version", 17)?;
        }
        if version < 18 {
            transaction.execute_batch(include_str!("environments.sql"))?;
            transaction.pragma_update(None, "user_version", 18)?;
        }
        if version < 19 {
            transaction.execute_batch(include_str!("remote_receipts.sql"))?;
            transaction.pragma_update(None, "user_version", 19)?;
        }
        transaction.commit()?;
        Ok(Self { connection })
    }

    /// Inspect an existing database without creating or migrating it.
    /// SQLite may coordinate locks/shared memory, but database writes are forbidden.
    /// Ordinary read-only mode includes committed WAL data instead of ignoring it.
    pub fn open_existing_read_only(path: impl AsRef<Path>) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let supplied = path.as_ref();
        let parent = supplied
            .parent()
            .filter(|path| !path.as_os_str().is_empty());
        let path = parent
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()
            .map_err(StoreError::Io)?
            .join(supplied.file_name().ok_or(StoreError::InvalidRecord(
                "read_only_unsafe_database: database file name is missing",
            ))?);
        let path = path.as_path();
        let validate = |path: &Path, required: bool| -> Result<()> {
            let file = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
            {
                Ok(file) => file,
                Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(());
                }
                Err(error) => return Err(StoreError::Io(error)),
            };
            let actual = file.metadata().map_err(StoreError::Io)?;
            let named = std::fs::symlink_metadata(path).map_err(StoreError::Io)?;
            // SAFETY: geteuid has no preconditions.
            if !actual.is_file()
                || actual.uid() != unsafe { libc::geteuid() }
                || actual.nlink() != 1
                || actual.mode() & 0o077 != 0
                || !named.is_file()
                || (actual.dev(), actual.ino()) != (named.dev(), named.ino())
            {
                return Err(StoreError::InvalidRecord(
                    "read_only_unsafe_database: SQLite files must be private owned regular files",
                ));
            }
            Ok(())
        };
        let sidecars = ["-wal", "-shm"].map(|suffix| {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            std::path::PathBuf::from(name)
        });
        validate(path, true)?;
        for sidecar in &sidecars {
            validate(sidecar, false)?;
        }
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        validate(path, true)?;
        for sidecar in &sidecars {
            validate(sidecar, false)?;
        }
        if !matches!(version, 5..=19) {
            return Err(StoreError::UnsupportedSchema(version));
        }
        Ok(Self { connection })
    }

    /// One bounded metadata snapshot per run, independent of raw capture retention.
    pub fn record_observation(&self, run: &RunId, metadata: &str) -> Result<()> {
        if metadata.len() > 8192 {
            return Err(StoreError::InvalidRecord("observation exceeds 8 KiB"));
        }
        self.connection.execute(
            "INSERT INTO run_observations(run_id,revision,metadata) VALUES(?1,1,?2)
             ON CONFLICT(run_id) DO UPDATE SET revision=revision+1,metadata=excluded.metadata",
            params![run.as_str(), metadata],
        )?;
        Ok(())
    }

    pub fn observation(&self, run: &RunId) -> Result<Option<(u64, String)>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 13 {
            return Ok(None);
        }
        Ok(self
            .connection
            .query_row(
                "SELECT revision,metadata FROM run_observations WHERE run_id=?1",
                [run.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn record_run_target(&self, run: &RunId, pin: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO run_targets(run_id,pin) VALUES (?1,?2)",
            params![run.as_str(), pin],
        )?;
        Ok(())
    }

    pub fn run_target(&self, run: &RunId) -> Result<Option<String>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 14 {
            return Ok(None);
        }
        Ok(self
            .connection
            .query_row(
                "SELECT pin FROM run_targets WHERE run_id=?1",
                [run.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Insert observed invocation evidence or monotonically advance an existing attempt.
    /// Exact replays are idempotent; SQLite enforces immutable provenance and terminal evidence.
    pub fn record_worker_attempt(&self, attempt: &WorkerAttempt) -> Result<()> {
        if attempt.attempt_id.is_empty()
            || attempt.attempt_id.len() > 128
            || !attempt
                .attempt_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(StoreError::InvalidRecord("invalid worker attempt ID"));
        }
        HarnessVersion::validate(&attempt.harness_version)?;
        if attempt.backend_id.trim().is_empty() || attempt.event_cursor > i64::MAX as u64 {
            return Err(StoreError::InvalidRecord(
                "invalid worker attempt backend or cursor",
            ));
        }
        self.connection.execute(
            "INSERT INTO worker_attempts (
                attempt_id, run_id, harness_requested, harness_resolved, harness_version,
                model_requested, model_resolved, profile_digest, backend_id, event_cursor,
                created_at, started_at, finished_at, exit_code, exit_signal, termination_reason
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(attempt_id) DO UPDATE SET
                run_id=excluded.run_id, harness_requested=excluded.harness_requested,
                harness_resolved=excluded.harness_resolved, harness_version=excluded.harness_version,
                model_requested=excluded.model_requested, model_resolved=excluded.model_resolved,
                profile_digest=excluded.profile_digest, backend_id=excluded.backend_id,
                event_cursor=excluded.event_cursor, created_at=excluded.created_at,
                started_at=excluded.started_at, finished_at=excluded.finished_at,
                exit_code=excluded.exit_code, exit_signal=excluded.exit_signal,
                termination_reason=excluded.termination_reason",
            params![
                attempt.attempt_id, attempt.run_id.as_str(), attempt.harness_requested.as_str(),
                attempt.harness_resolved.as_str(), attempt.harness_version, attempt.model_requested,
                attempt.model_resolved, attempt.profile_digest, attempt.backend_id,
                attempt.event_cursor, attempt.created_at, attempt.started_at, attempt.finished_at,
                attempt.exit_code, attempt.exit_signal, attempt.termination_reason,
            ],
        )?;
        Ok(())
    }

    /// Legacy runs have no invented invocation history, including on older read-only schemas.
    pub fn worker_attempts(&self, run_id: &RunId) -> Result<Vec<WorkerAttempt>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 9 {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT attempt_id, run_id, harness_requested, harness_resolved, harness_version,
                model_requested, model_resolved, profile_digest, backend_id, event_cursor,
                created_at, started_at, finished_at, exit_code, exit_signal, termination_reason
             FROM worker_attempts WHERE run_id = ?1 ORDER BY attempt_id",
        )?;
        Ok(statement
            .query_map([run_id.as_str()], |row| {
                Ok(WorkerAttempt {
                    attempt_id: row.get(0)?,
                    run_id: read_id(row, 1)?,
                    harness_requested: read_id(row, 2)?,
                    harness_resolved: read_id(row, 3)?,
                    harness_version: row.get(4)?,
                    model_requested: row.get(5)?,
                    model_resolved: row.get(6)?,
                    profile_digest: row.get(7)?,
                    backend_id: row.get(8)?,
                    event_cursor: row.get(9)?,
                    created_at: row.get(10)?,
                    started_at: row.get(11)?,
                    finished_at: row.get(12)?,
                    exit_code: row.get(13)?,
                    exit_signal: row.get(14)?,
                    termination_reason: row.get(15)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn run_ids(&self) -> Result<Vec<RunId>> {
        let mut statement = self
            .connection
            .prepare("SELECT run_id FROM runs ORDER BY run_id")?;
        Ok(statement
            .query_map([], |row| read_id(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn workspace_ids(&self) -> Result<Vec<WorkspaceId>> {
        let mut statement = self
            .connection
            .prepare("SELECT workspace_id FROM workspaces ORDER BY workspace_id")?;
        Ok(statement
            .query_map([], |row| read_id(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn cleanup_intent(&self, id: &WorkspaceId) -> Result<Option<String>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 5 {
            return Ok(None);
        }
        Ok(self
            .connection
            .query_row(
                "SELECT authority FROM cleanup_intents WHERE workspace_id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Commit caller-verified filesystem authority against exact durable snapshots.
    /// An expired lease alone never grants permission to reclaim a workspace.
    pub fn record_cleanup_intent(
        &mut self,
        run: &Run,
        workspace: &Workspace,
        authority: &str,
        now: i64,
    ) -> Result<()> {
        if authority.is_empty() {
            return Err(StoreError::InvalidRecord(
                "Cleanup authority must not be empty",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if Self::query_run(&transaction, &run.run_id)?.as_ref() != Some(run)
            || Self::query_workspace(&transaction, &workspace.workspace_id)?.as_ref()
                != Some(workspace)
        {
            return Err(StoreError::Conflict);
        }
        if !matches!(
            run.state,
            RunState::Succeeded | RunState::Failed | RunState::Cancelled | RunState::NeedsAttention
        ) || (run.state == RunState::NeedsAttention && run.capture.is_none())
            || run.workspace_id != workspace.workspace_id
            || workspace.run_id != run.run_id
            || workspace.state_id != run.prepared_state_id
            || workspace.state != WorkspaceState::CleanupPending
            || workspace.retention_reason.is_some()
            || workspace
                .lease_expires_at
                .is_some_and(|expiry| expiry > now)
        {
            return Err(StoreError::InvalidRecord(
                "Workspace is not cleanup eligible",
            ));
        }
        let manifest = run
            .artifact_manifest_id
            .as_ref()
            .ok_or(StoreError::InvalidRecord(
                "Cleanup requires an artifact manifest",
            ))?;
        let complete: bool = transaction.query_row(
            "SELECT
               (SELECT count(*) FROM artifacts WHERE run_id = ?1 AND kind IN
                 ('stdout', 'stderr', 'events', 'tracked_patch', 'untracked_archive', 'prompt', 'manifest')) = 7
               AND EXISTS (SELECT 1 FROM artifacts WHERE run_id = ?1 AND kind = 'manifest'
                 AND relative_path = 'artifacts/' || ?2 || '/manifest.json')",
            params![run.run_id.as_str(), manifest.as_str()],
            |row| row.get(0),
        )?;
        if !complete {
            return Err(StoreError::InvalidRecord(
                "Cleanup requires complete artifacts",
            ));
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT authority FROM cleanup_intents WHERE workspace_id = ?1",
                [workspace.workspace_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            Some(existing) if existing != authority => return Err(StoreError::Conflict),
            Some(_) => {}
            None => {
                transaction.execute(
                    "INSERT INTO cleanup_intents (workspace_id, authority) VALUES (?1, ?2)",
                    params![workspace.workspace_id.as_str(), authority],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Preserve ownership records after deletion; matching completion replay is a no-op.
    pub fn finish_cleanup(&mut self, id: &WorkspaceId, authority: &str) -> Result<()> {
        if self.connection.execute(
            "UPDATE workspaces SET state = 'RECLAIMED', usage_bytes = 0
             WHERE workspace_id = ?1
               AND (state = 'CLEANUP_PENDING' OR (state = 'RECLAIMED' AND usage_bytes = 0))
               AND EXISTS (SELECT 1 FROM cleanup_intents
                           WHERE workspace_id = ?1 AND authority = ?2)",
            params![id.as_str(), authority],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    /// Register every task and stable Run ID before admitting any worker.
    /// Run IDs intentionally have no foreign key: prelaunch errors may have no workspace.
    pub fn create_batch_with_sources(
        &mut self,
        id: &BatchId,
        state: &PreparedStateId,
        tasks: &[BatchTask],
        sources: &[SourceProvenance],
    ) -> Result<()> {
        if !sources.is_empty() && sources.len() != tasks.len() {
            return Err(StoreError::InvalidRecord("every task requires a source"));
        }
        if tasks.is_empty() {
            return Err(StoreError::InvalidRecord("batch requires tasks"));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO batches (batch_id, prepared_state_id) VALUES (?1, ?2)",
            params![id.as_str(), state.as_str()],
        )?;
        for (position, task) in tasks.iter().enumerate() {
            transaction.execute(
                "INSERT INTO batch_tasks (batch_id, position, task_id, run_id, prompt_file)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.as_str(),
                    position as i64,
                    task.task_id,
                    task.run_id.as_str(),
                    task.prompt_file
                ],
            )?;
            insert_task_source(&transaction, &task.run_id, state, sources.get(position))?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_batch_task(
        &self,
        run: &RunId,
        outcome: std::result::Result<&str, &str>,
    ) -> Result<()> {
        let (result, error) = match outcome {
            Ok(value) => (Some(value), None),
            Err(value) => (None, Some(value)),
        };
        if self.connection.execute(
            "UPDATE batch_tasks SET result = ?1, error = ?2
             WHERE run_id = ?3 AND result IS NULL AND error IS NULL",
            params![result, error, run.as_str()],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    pub fn batch_tasks(&self, id: &BatchId) -> Result<Vec<BatchTask>> {
        let mut statement = self.connection.prepare(
            "SELECT task_id, run_id, prompt_file, result, error FROM batch_tasks
             WHERE batch_id = ?1 ORDER BY position",
        )?;
        Ok(statement
            .query_map([id.as_str()], |row| {
                Ok(BatchTask {
                    task_id: row.get(0)?,
                    run_id: read_id(row, 1)?,
                    prompt_file: row.get(2)?,
                    result: row.get(3)?,
                    error: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn batch_for_run(&self, run: &RunId) -> Result<Option<BatchId>> {
        Ok(self
            .connection
            .query_row(
                "SELECT batch_id FROM batch_tasks WHERE run_id = ?1",
                [run.as_str()],
                |row| read_id(row, 0),
            )
            .optional()?)
    }

    pub fn finish_batch(&self, id: &BatchId, succeeded: bool) -> Result<()> {
        if self.connection.execute(
            "UPDATE batches SET succeeded = ?1 WHERE batch_id = ?2 AND succeeded IS NULL
             AND NOT EXISTS (SELECT 1 FROM batch_tasks WHERE batch_id = ?2
                             AND result IS NULL AND error IS NULL)",
            params![succeeded, id.as_str()],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    pub fn batch_succeeded(&self, id: &BatchId) -> Result<Option<bool>> {
        self.connection
            .query_row(
                "SELECT succeeded FROM batches WHERE batch_id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn create_project(&self, project: &Project) -> Result<()> {
        self.connection.execute(
            "INSERT INTO projects (project_id, name, source_path, default_ref, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                project.project_id.as_str(),
                project.name,
                project.source_path,
                project.default_ref,
                project.created_at
            ],
        )?;
        Ok(())
    }

    pub fn project(&self, id: &ProjectId) -> Result<Option<Project>> {
        Ok(self
            .connection
            .query_row(
                "SELECT project_id, name, source_path, default_ref, created_at
             FROM projects WHERE project_id = ?1",
                [id.as_str()],
                read_project,
            )
            .optional()?)
    }

    pub fn project_by_name(&self, name: &str) -> Result<Option<Project>> {
        Ok(self
            .connection
            .query_row(
                "SELECT project_id, name, source_path, default_ref, created_at
                 FROM projects WHERE name = ?1",
                [name],
                read_project,
            )
            .optional()?)
    }

    pub fn prepared_states(&self) -> Result<Vec<PreparedState>> {
        let mut statement = self.connection.prepare(
            "SELECT state_id, project_id, preparation_key, source_commit_sha, recipe_digest,
             harness_environment_digest, prepared_path, status, created_at, verified_at, size_bytes
             FROM prepared_states ORDER BY verified_at, state_id",
        )?;
        Ok(statement
            .query_map([], read_prepared_state)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Historical provenance and pinned batches are never disposable caches.
    pub fn cache_retention(&self, id: &PreparedStateId) -> Result<Option<String>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        let reserved = version >= 7
            && self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM storage_reservations WHERE state_id = ?1)",
                [id.as_str()],
                |row| row.get::<_, bool>(0),
            )?;
        let task_pinned = version >= 11
            && self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM batch_task_sources WHERE prepared_state_id = ?1)",
                [id.as_str()],
                |row| row.get::<_, bool>(0),
            )?;
        Ok(self.connection.query_row(
            "SELECT CASE
             WHEN status != 'READY' THEN 'not_ready'
             WHEN EXISTS (SELECT 1 FROM runs WHERE prepared_state_id = ?1)
               OR EXISTS (SELECT 1 FROM batches WHERE prepared_state_id = ?1)
               OR EXISTS (SELECT 1 FROM workspaces WHERE state_id = ?1) OR ?3 THEN 'historical_reference'
             WHEN ?2 THEN 'admitted_worker'
             WHEN NOT EXISTS (SELECT 1 FROM prepared_states newer
               WHERE newer.project_id = prepared_states.project_id AND newer.status = 'READY'
                 AND newer.source_commit_sha = prepared_states.source_commit_sha
                 AND (newer.verified_at > prepared_states.verified_at
                   OR (newer.verified_at = prepared_states.verified_at AND newer.state_id > prepared_states.state_id)))
               THEN 'selected_state' ELSE NULL END
             FROM prepared_states WHERE state_id = ?1", params![id.as_str(), reserved, task_pinned], |row| row.get(0))?)
    }

    pub fn reserved_storage_bytes(&self) -> Result<u64> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 7 {
            return Ok(0);
        }
        Ok(self.connection.query_row(
            "SELECT COALESCE(sum(bytes), 0) FROM storage_reservations",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn reserve_storage(
        &self,
        token: &str,
        state: Option<&PreparedStateId>,
        bytes: u64,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO storage_reservations VALUES (?1, ?2, ?3)",
            params![token, state.map(PreparedStateId::as_str), bytes],
        )?;
        Ok(())
    }

    pub fn release_storage(&self, token: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM storage_reservations WHERE token = ?1", [token])?;
        Ok(())
    }

    /// The caller holds the root admission/cleanup lock and the recorded owner lock.
    pub fn create_worker_lease(&self, token: &str, authority: &str, expires: i64) -> Result<()> {
        self.connection.execute(
            "INSERT INTO worker_leases VALUES (?1, ?2, ?3)",
            params![token, authority, expires],
        )?;
        Ok(())
    }

    pub fn update_worker_lease(&self, token: &str, authority: &str) -> Result<()> {
        if self.connection.execute(
            "UPDATE worker_leases SET authority = ?1 WHERE token = ?2",
            params![authority, token],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    pub fn worker_lease(&self, token: &str) -> Result<Option<(String, i64)>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 8 {
            return Ok(None);
        }
        Ok(self
            .connection
            .query_row(
                "SELECT authority, expires_at FROM worker_leases WHERE token = ?1",
                [token],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn worker_lease_tokens(&self) -> Result<Vec<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT token FROM worker_leases ORDER BY token")?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// No exit, finish time or artifact is invented. Cleanup intents are immutable.
    /// Only the caller's verified inactive owner lock permits reservation release.
    pub fn reconcile_interrupted(&mut self, token: &str, inactive: bool, now: i64) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if inactive {
            transaction.execute("DELETE FROM storage_reservations WHERE token = ?1", [token])?;
        }
        let changed = transaction.execute(
            "UPDATE runs SET state = 'NEEDS_ATTENTION', error_kind = COALESCE(error_kind, 'interrupted'),
             updated_at = max(updated_at, ?2)
             WHERE run_id = ?1 AND state IN ('QUEUED', 'PREPARING', 'RUNNING', 'FINALIZING')
             AND NOT EXISTS (SELECT 1 FROM cleanup_intents c WHERE c.workspace_id = runs.workspace_id)",
            params![token, now],
        )?;
        if changed != 0 {
            transaction.execute(
                "UPDATE workspaces SET state = 'RECOVERY_REQUIRED',
                 retention_reason = 'Interrupted supervisor; recovery required. Inspect workspace and captured evidence; no process exit or complete capture is assumed.'
                 WHERE run_id = ?1 AND state != 'RECLAIMED'
                 AND NOT EXISTS (SELECT 1 FROM cleanup_intents c WHERE c.workspace_id = workspaces.workspace_id)",
                [token],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Root cleanup lease must cover validation, metadata cutover and physical deletion.
    /// Delete only an unreferenced row; a same-key preparation can then be rebuilt.
    pub fn begin_cache_eviction(
        &mut self,
        state: &PreparedState,
        name: &str,
        authority: &str,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String, String)> = transaction
            .query_row(
                "SELECT status, prepared_path FROM prepared_states WHERE state_id=?1",
                [state.state_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if current != Some(("READY".into(), state.prepared_path.clone())) {
            return Err(StoreError::Conflict);
        }
        transaction.execute(
            "INSERT INTO cache_evictions VALUES (?1, ?2, ?3)",
            params![state.state_id.as_str(), name, authority],
        )?;
        if transaction.execute(
            "DELETE FROM prepared_states WHERE state_id=?1 AND NOT EXISTS
             (SELECT 1 FROM storage_reservations WHERE state_id=?1)",
            [state.state_id.as_str()],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn cache_evictions(&self) -> Result<Vec<(String, String, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT state_id, container_name, authority FROM cache_evictions ORDER BY state_id",
        )?;
        Ok(statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn finish_cache_eviction(&self, id: &str, authority: &str) -> Result<()> {
        if self.connection.execute(
            "DELETE FROM cache_evictions WHERE state_id=?1 AND authority=?2",
            params![id, authority],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }
    pub fn create_prepared_state(&self, state: &PreparedState) -> Result<()> {
        if state.status != PreparedStateStatus::Preparing || state.verified_at.is_some() {
            return Err(StoreError::InvalidRecord(
                "new prepared state must be PREPARING and unverified",
            ));
        }
        self.connection.execute(
            "INSERT INTO prepared_states (state_id, project_id, preparation_key, source_commit_sha,
             recipe_digest, harness_environment_digest, prepared_path, status, created_at,
             verified_at, size_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'PREPARING', ?8, NULL, ?9)",
            params![state.state_id.as_str(), state.project_id.as_str(), state.preparation_key,
                state.source_commit_sha, state.recipe_digest, state.harness_environment_digest,
                state.prepared_path, state.created_at, state.size_bytes],
        )?;
        Ok(())
    }

    pub fn prepared_state(&self, id: &PreparedStateId) -> Result<Option<PreparedState>> {
        Ok(self
            .connection
            .query_row(
                "SELECT state_id, project_id, preparation_key, source_commit_sha, recipe_digest,
             harness_environment_digest, prepared_path, status, created_at, verified_at, size_bytes
             FROM prepared_states WHERE state_id = ?1",
                [id.as_str()],
                read_prepared_state,
            )
            .optional()?)
    }

    pub fn prepared_state_by_key(
        &self,
        project_id: &ProjectId,
        preparation_key: &str,
    ) -> Result<Option<PreparedState>> {
        Ok(self
            .connection
            .query_row(
                "SELECT state_id, project_id, preparation_key, source_commit_sha, recipe_digest,
                 harness_environment_digest, prepared_path, status, created_at, verified_at, size_bytes
                 FROM prepared_states WHERE project_id = ?1 AND preparation_key = ?2",
                params![project_id.as_str(), preparation_key],
                read_prepared_state,
            )
            .optional()?)
    }

    /// Latest verified preparation for the source's current committed HEAD.
    pub fn ready_state_for_commit(
        &self,
        project_id: &ProjectId,
        commit: &str,
    ) -> Result<Option<PreparedState>> {
        Ok(self
            .connection
            .query_row(
                "SELECT state_id, project_id, preparation_key, source_commit_sha, recipe_digest,
             harness_environment_digest, prepared_path, status, created_at, verified_at, size_bytes
             FROM prepared_states WHERE project_id = ?1 AND source_commit_sha = ?2
             AND status = 'READY' ORDER BY verified_at DESC, state_id DESC LIMIT 1",
                params![project_id.as_str(), commit],
                read_prepared_state,
            )
            .optional()?)
    }

    /// Called only after a child was actually spawned. A stale supervisor cannot
    /// start the same run twice; PID metadata alone never authorizes signalling.
    pub fn start_run(&self, id: &RunId, version: &str, pid: u32, now: i64) -> Result<()> {
        let count = self.connection.execute(
            "UPDATE runs SET state = 'RUNNING', harness_version = ?1, pid_metadata = ?2,
             started_at = ?3, updated_at = ?3 WHERE run_id = ?4 AND state = 'PREPARING'
             AND ?3 >= updated_at AND EXISTS (SELECT 1 FROM workspaces w
             WHERE w.run_id = runs.run_id AND w.state = 'ACTIVE' AND w.retention_reason IS NULL)",
            params![
                version,
                format!("local-process-group:{pid}"),
                now,
                id.as_str()
            ],
        )?;
        if count != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    /// No remote PID is controller signal authority. The immutable prestart
    /// receipt and target/environment pins are required before RUNNING.
    pub fn start_remote_run(&self, id: &RunId, version: &str, at: i64) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE runs SET state='RUNNING', harness_version=?2,
             pid_metadata='remote.v1', started_at=?3, updated_at=?3
             WHERE run_id=?1 AND state='PREPARING' AND ?3 >= updated_at
             AND EXISTS (SELECT 1 FROM run_launch_receipts l WHERE l.run_id=?1
               AND json_type(l.receipt_json,'$.remote')='object')
             AND EXISTS (SELECT 1 FROM workspaces w WHERE w.run_id=?1
               AND w.state='ACTIVE' AND w.retention_reason IS NULL)",
            params![id.as_str(), version, at],
        )?;
        if changed != 1 { return Err(StoreError::Conflict); }
        Ok(())
    }

    /// Persist the observed wait result and supervision/native failure before finalization.
    pub fn observe_run_exit(
        &self,
        id: &RunId,
        exit: Option<i32>,
        signal: Option<i32>,
        reason: Option<&str>,
        now: i64,
    ) -> Result<()> {
        if !matches!(
            reason,
            None | Some(
                "timeout"
                    | "cancelled"
                    | "workspace_limit"
                    | "storage_pressure"
                    | "storage_meter_failed"
                    | "native_failure"
            )
        ) {
            return Err(StoreError::InvalidRecord("Invalid termination reason"));
        }
        let count = self.connection.execute(
            "UPDATE runs SET state = 'FINALIZING', exit_code = ?1, updated_at = ?2,
             exit_signal = ?4,
             error_kind = CASE WHEN error_kind = 'target_protocol' THEN error_kind
               ELSE COALESCE(?5, CASE WHEN ?1 = 0 THEN NULL ELSE 'harness_exit' END) END
             WHERE run_id = ?3 AND ?2 >= updated_at
               AND (state = 'RUNNING' OR (state = 'PREPARING' AND EXISTS (
                 SELECT 1 FROM worker_attempts a WHERE a.run_id = runs.run_id
                   AND a.finished_at IS NOT NULL AND a.exit_code IS ?1 AND a.exit_signal IS ?4)))",
            params![exit, now, id.as_str(), signal, reason],
        )?;
        if count != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    /// Record only the actual matching prelaunch profile; never reconstruct legacy evidence.
    pub fn record_capture_context(&self, id: &RunId, context: &CaptureContext) -> Result<()> {
        validate_capture_context(context)?;
        let encoded = serde_json::to_string(context)
            .map_err(|_| StoreError::InvalidRecord("Cannot encode capture context"))?;
        self.connection.execute(
            "INSERT INTO run_capture_contexts (run_id, context_json) VALUES (?1, ?2)",
            params![id.as_str(), encoded],
        )?;
        Ok(())
    }

    /// Older schemas/runs intentionally have no fabricated capture context.
    pub fn capture_context(&self, id: &RunId) -> Result<Option<CaptureContext>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 12 {
            return Ok(None);
        }
        let encoded: Option<String> = self
            .connection
            .query_row(
                "SELECT context_json FROM run_capture_contexts WHERE run_id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        encoded
            .map(|encoded| {
                let context = serde_json::from_str(&encoded)
                    .map_err(|_| StoreError::InvalidRecord("Invalid capture context"))?;
                validate_capture_context(&context)?;
                Ok(context)
            })
            .transpose()
    }
    /// Pin the checkout before recipes; never derive missing evidence from a READY tree.
    pub fn pin_checkout(&self, id: &PreparedStateId, digest: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO prepared_source_evidence(state_id, checkout_sha256) VALUES (?1, ?2)",
            params![id.as_str(), digest],
        )?;
        Ok(())
    }

    pub fn pin_prepared_content(&self, id: &PreparedStateId, digest: &str) -> Result<()> {
        if self.connection.execute(
            "UPDATE prepared_source_evidence SET prepared_sha256=?2
             WHERE state_id=?1 AND prepared_sha256 IS NULL",
            params![id.as_str(), digest],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    pub fn source_evidence(&self, id: &PreparedStateId) -> Result<Option<SourceEvidence>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 17 {
            return Ok(None);
        }
        Ok(self
            .connection
            .query_row(
                "SELECT checkout_sha256, prepared_sha256 FROM prepared_source_evidence
             WHERE state_id=?1 AND prepared_sha256 IS NOT NULL",
                [id.as_str()],
                |r| {
                    Ok(SourceEvidence {
                        checkout_sha256: r.get(0)?,
                        prepared_sha256: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// The launch boundary is checked twice. Only identical repeated evidence is reusable.
    pub fn record_launch_receipt(&self, receipt: &LaunchReceipt) -> Result<()> {
        if let Some(existing) = self.launch_receipt(&receipt.run_id)? {
            return if existing == *receipt {
                Ok(())
            } else {
                Err(StoreError::Conflict)
            };
        }
        let encoded = serde_json::to_string(receipt)
            .map_err(|_| StoreError::InvalidRecord("Cannot encode launch receipt"))?;
        self.connection.execute(
            "INSERT INTO run_launch_receipts(run_id, receipt_json) VALUES (?1, ?2)",
            params![receipt.run_id.as_str(), encoded],
        )?;
        Ok(())
    }

    pub fn launch_receipt(&self, id: &RunId) -> Result<Option<LaunchReceipt>> {
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 17 {
            return Ok(None);
        }
        let encoded: Option<String> = self
            .connection
            .query_row(
                "SELECT receipt_json FROM run_launch_receipts WHERE run_id=?1",
                [id.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        encoded
            .map(|json| {
                serde_json::from_str(&json)
                    .map_err(|_| StoreError::InvalidRecord("Invalid launch receipt"))
            })
            .transpose()
    }

    pub fn record_remote_observation(&self, id: &RunId, evidence: &serde_json::Value) -> Result<()> {
        let encoded = serde_json::to_string(evidence)
            .map_err(|_| StoreError::InvalidRecord("Invalid remote observation"))?;
        if encoded.len() > 4096 { return Err(StoreError::InvalidRecord("Remote observation bounds")); }
        self.connection.execute("INSERT INTO run_remote_observations(run_id,evidence_json) VALUES (?1,?2)",
            params![id.as_str(),encoded])?;
        Ok(())
    }

    pub fn remote_observation(&self, id: &RunId) -> Result<Option<serde_json::Value>> {
        let version: i64 = self.connection.pragma_query_value(None,"user_version",|r|r.get(0))?;
        if version < 19 { return Ok(None); }
        let encoded: Option<String> = self.connection.query_row(
            "SELECT evidence_json FROM run_remote_observations WHERE run_id=?1",
            [id.as_str()],|r|r.get(0)).optional()?;
        encoded.map(|s|serde_json::from_str(&s)
            .map_err(|_|StoreError::InvalidRecord("Invalid remote observation"))).transpose()
    }

    /// The bounded output files have already been synced by the runner.
    pub fn record_capture(&self, id: &RunId, capture: &CaptureMetadata) -> Result<()> {
        let encoded = serde_json::to_string(capture)
            .map_err(|_| StoreError::InvalidRecord("Cannot encode capture metadata"))?;
        if self.connection.execute(
            "UPDATE runs SET capture_metadata = ?1 WHERE run_id = ?2
             AND state = 'FINALIZING' AND capture_metadata IS NULL",
            params![encoded, id.as_str()],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }
    /// Caller holds the root/run leases and has verified the pinned inactive remote
    /// result and downloaded bytes. Recovery never invents a terminal success.
    pub fn record_recovered_capture(
        &mut self, expected: &Run, capture: &CaptureMetadata,
        exit_code: Option<i32>, exit_signal: Option<i32>, reason: Option<&str>,
    ) -> Result<()> {
        if expected.state != RunState::NeedsAttention || expected.capture.is_some()
            || expected.artifact_manifest_id.is_some()
            || exit_code.is_some() == exit_signal.is_some()
            || expected.exit_code.is_some_and(|code| Some(code) != exit_code)
            || expected.exit_signal.is_some_and(|signal| Some(signal) != exit_signal)
            || !matches!(reason, None | Some("timeout" | "cancelled" | "workspace_limit" | "storage_pressure" | "storage_meter_failed" | "native_failure")) {
            return Err(StoreError::InvalidRecord("Remote recovery result differs"));
        }
        let encoded = serde_json::to_string(capture).map_err(|_|StoreError::InvalidRecord("Invalid capture"))?;
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if Self::query_run(&transaction,&expected.run_id)?.as_ref() != Some(expected) {
            return Err(StoreError::Conflict);
        }
        if transaction.execute(
            "UPDATE runs SET capture_metadata=?2, exit_code=?3, exit_signal=?4,
             error_kind=CASE WHEN error_kind IS NOT NULL AND error_kind NOT IN ('harness_execution','artifact_capture') THEN error_kind
               WHEN ?5 IS NOT NULL THEN ?5 WHEN ?3 IS NOT 0 THEN 'harness_exit' ELSE error_kind END
             WHERE run_id=?1 AND state='NEEDS_ATTENTION' AND capture_metadata IS NULL
             AND EXISTS (SELECT 1 FROM workspaces w WHERE w.run_id=?1 AND w.state='RECOVERY_REQUIRED')
             AND NOT EXISTS (SELECT 1 FROM cleanup_intents c JOIN workspaces w USING(workspace_id) WHERE w.run_id=?1)
             AND EXISTS (SELECT 1 FROM run_launch_receipts l WHERE l.run_id=?1 AND json_type(l.receipt_json,'$.remote')='object')",
            params![expected.run_id.as_str(),encoded,exit_code,exit_signal,reason],
        )? != 1 { return Err(StoreError::Conflict); }
        transaction.commit()?;
        Ok(())
    }

    /// Register verified promoted files and terminal completion in one transaction.
    /// Complete tracked and non-ignored untracked capture precedes cleanup eligibility.
    pub fn finish_run(
        &mut self,
        id: &RunId,
        capture: &CaptureMetadata,
        manifest: &ArtifactManifestId,
        artifacts: &[Artifact],
        now: i64,
    ) -> Result<()> {
        validate_artifacts(manifest, artifacts)?;
        let encoded = serde_json::to_string(capture)
            .map_err(|_| StoreError::InvalidRecord("Cannot encode capture metadata"))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_artifacts(&transaction, id, artifacts)?;
        let count = transaction.execute(
            "UPDATE runs SET state = CASE WHEN error_kind = 'cancelled' THEN 'CANCELLED'
             WHEN error_kind IS NOT NULL THEN 'FAILED'
             WHEN exit_code = 0 THEN 'SUCCEEDED' ELSE 'FAILED' END,
             updated_at = ?1, finished_at = ?1, capture_metadata = ?3, artifact_manifest_id = ?4
             WHERE run_id = ?2 AND state = 'FINALIZING' AND ?1 >= updated_at
               AND artifact_manifest_id IS NULL",
            params![now, id.as_str(), encoded, manifest.as_str()],
        )?;
        if count != 1 {
            return Err(StoreError::Conflict);
        }
        let count = transaction.execute(
            "UPDATE workspaces SET state = 'CLEANUP_PENDING', lease_expires_at = NULL,
             retention_reason = NULL
             WHERE run_id = ?1 AND state = 'ACTIVE'",
            [id.as_str()],
        )?;
        if count != 1 {
            return Err(StoreError::Conflict);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Associate caller-verified recovery files without inventing process completion.
    /// Filesystem ownership, inactivity and full manifest verification remain caller obligations.
    pub fn recover_artifacts(
        &mut self,
        expected_run: &Run,
        expected_workspace: &Workspace,
        manifest: &ArtifactManifestId,
        artifacts: &[Artifact],
    ) -> Result<()> {
        validate_artifacts(manifest, artifacts)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if Self::query_run(&transaction, &expected_run.run_id)?.as_ref() != Some(expected_run)
            || Self::query_workspace(&transaction, &expected_workspace.workspace_id)?.as_ref()
                != Some(expected_workspace)
        {
            return Err(StoreError::Conflict);
        }
        if expected_run.state != RunState::NeedsAttention
            || expected_run.capture.is_none()
            || expected_run.artifact_manifest_id.is_some()
            || expected_workspace.state != WorkspaceState::RecoveryRequired
            || expected_run.workspace_id != expected_workspace.workspace_id
            || expected_workspace.run_id != expected_run.run_id
            || expected_workspace.state_id != expected_run.prepared_state_id
        {
            return Err(StoreError::InvalidRecord(
                "Workspace is not recovery eligible",
            ));
        }
        let has_intent: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id = ?1)",
            [expected_workspace.workspace_id.as_str()],
            |row| row.get(0),
        )?;
        if has_intent {
            return Err(StoreError::Conflict);
        }
        insert_artifacts(&transaction, &expected_run.run_id, artifacts)?;
        if transaction.execute(
            "UPDATE runs SET artifact_manifest_id = ?1 WHERE run_id = ?2",
            params![manifest.as_str(), expected_run.run_id.as_str()],
        )? != 1
            || transaction.execute(
                "UPDATE workspaces SET state = 'CLEANUP_PENDING',
                 lease_expires_at = NULL, retention_reason = NULL WHERE workspace_id = ?1",
                [expected_workspace.workspace_id.as_str()],
            )? != 1
        {
            return Err(StoreError::Conflict);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn artifacts(&self, id: &RunId) -> Result<Vec<Artifact>> {
        let mut statement = self.connection.prepare(
            "SELECT kind, relative_path, sha256, size_bytes FROM artifacts
             WHERE run_id = ?1 ORDER BY kind",
        )?;
        Ok(statement
            .query_map([id.as_str()], |row| {
                Ok(Artifact {
                    kind: row.get(0)?,
                    relative_path: row.get(1)?,
                    sha256: row.get(2)?,
                    size_bytes: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?)
    }

    /// Failure-only annotation, including after cleanup intent. Never changes
    /// ownership, process/capture/artifact evidence, timestamps or deletion authority.
    pub fn record_target_failure(&self, id: &RunId) -> Result<()> {
        if self.connection.execute(
            "UPDATE runs SET error_kind = 'target_protocol',
             state = CASE WHEN state = 'SUCCEEDED' THEN 'FAILED' ELSE state END
             WHERE run_id = ?1 AND EXISTS (SELECT 1 FROM run_targets WHERE run_id = ?1)",
            [id.as_str()],
        )? != 1
        {
            return Err(StoreError::Conflict);
        }
        Ok(())
    }

    /// Setup/launch/supervision errors are uncertain, never invented process exits.
    pub fn fail_run_execution(
        &mut self,
        id: &RunId,
        version: &str,
        reason: &str,
        now: i64,
        cancelled: bool,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE runs SET state = 'FINALIZING', updated_at = ?1
             WHERE run_id = ?2 AND state = 'RUNNING' AND ?1 >= updated_at",
            params![now, id.as_str()],
        )?;
        let count = transaction.execute(
            "UPDATE runs SET state = CASE WHEN state = 'PREPARING' AND ?4 THEN 'CANCELLED'
             ELSE 'NEEDS_ATTENTION' END, harness_version = ?1,
             updated_at = ?2, finished_at = ?2,
             error_kind = CASE WHEN state = 'PREPARING' AND ?4 THEN 'cancelled'
               WHEN error_kind IN ('timeout', 'cancelled', 'workspace_limit', 'storage_pressure', 'storage_meter_failed', 'native_failure', 'target_protocol') THEN error_kind
               WHEN capture_metadata IS NOT NULL THEN 'artifact_capture'
               ELSE 'harness_execution' END
             WHERE run_id = ?3 AND state IN ('PREPARING', 'FINALIZING')
             AND ?2 >= updated_at",
            params![version, now, id.as_str(), cancelled],
        )?;
        if count != 1 {
            return Err(StoreError::Conflict);
        }
        transaction.execute(
            "UPDATE workspaces SET state = 'RECOVERY_REQUIRED', retention_reason = ?1
             WHERE run_id = ?2 AND state = 'ACTIVE'",
            params![reason, id.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Publishes completion exactly once; replay must have identical verification metadata.
    pub fn finish_preparation(
        &mut self,
        id: &PreparedStateId,
        next: PreparedStateStatus,
        verified_at: Option<i64>,
        size_bytes: u64,
    ) -> Result<()> {
        if next == PreparedStateStatus::Preparing
            || (next == PreparedStateStatus::Ready) != verified_at.is_some()
        {
            return Err(StoreError::InvalidRecord(
                "READY requires verification; FAILED must be unverified",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, verified, size): (PreparedStateStatus, Option<i64>, u64) = transaction
            .query_row(
                "SELECT status, verified_at, size_bytes FROM prepared_states WHERE state_id = ?1",
                [id.as_str()],
                |r| Ok((read_state(r, 0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        let mut validated = current;
        validated.transition(next)?;
        if current == next {
            if verified != verified_at || size != size_bytes {
                return Err(StoreError::Conflict);
            }
        } else {
            transaction.execute(
                "UPDATE prepared_states SET status = ?1, verified_at = ?2, size_bytes = ?3
                 WHERE state_id = ?4 AND status = 'PREPARING'",
                params![next.sql_name(), verified_at, size_bytes, id.as_str()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Both rows commit together, including deferred, reciprocal ownership constraints.
    /// No filesystem operation is performed or authorized by this method.
    pub fn create_run_workspace(&mut self, run: &Run, workspace: &Workspace) -> Result<()> {
        if run.state != RunState::Queued
            || workspace.state != WorkspaceState::Active
            || run.capture.is_some()
            || run.exit_signal.is_some()
            || run.artifact_manifest_id.is_some()
        {
            return Err(StoreError::InvalidRecord(
                "new run/workspace must be QUEUED/ACTIVE without capture metadata",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO runs (run_id, batch_id, project_id, prepared_state_id, base_commit_sha,
             harness, harness_version, prompt_digest, state, workspace_id, pid_metadata, exit_code,
             created_at, updated_at, started_at, finished_at, error_kind, artifact_manifest_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'QUEUED', ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![run.run_id.as_str(), run.batch_id.as_ref().map(BatchId::as_str),
                run.project_id.as_str(), run.prepared_state_id.as_str(), run.base_commit_sha,
                run.harness, run.harness_version, run.prompt_digest, run.workspace_id.as_str(),
                run.pid_metadata, run.exit_code, run.created_at, run.updated_at, run.started_at,
                run.finished_at, run.error_kind, run.artifact_manifest_id.as_ref().map(ArtifactManifestId::as_str)],
        )?;
        transaction.execute(
            "INSERT INTO workspaces (workspace_id, run_id, managed_path, state_id, state,
             lease_expires_at, usage_bytes, retention_reason)
             VALUES (?1, ?2, ?3, ?4, 'ACTIVE', ?5, ?6, ?7)",
            params![
                workspace.workspace_id.as_str(),
                workspace.run_id.as_str(),
                workspace.managed_path,
                workspace.state_id.as_str(),
                workspace.lease_expires_at,
                workspace.usage_bytes,
                workspace.retention_reason
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn run(&self, id: &RunId) -> Result<Option<Run>> {
        Self::query_run(&self.connection, id)
    }

    fn query_run(connection: &Connection, id: &RunId) -> Result<Option<Run>> {
        Ok(connection
            .query_row(
                "SELECT run_id, batch_id, project_id, prepared_state_id, base_commit_sha, harness,
             harness_version, prompt_digest, state, workspace_id, pid_metadata, exit_code,
             created_at, updated_at, started_at, finished_at, error_kind, artifact_manifest_id,
             capture_metadata, exit_signal
             FROM runs WHERE run_id = ?1",
                [id.as_str()],
                |r| {
                    Ok(Run {
                        run_id: read_id(r, 0)?,
                        batch_id: read_optional_id(r, 1)?,
                        project_id: read_id(r, 2)?,
                        prepared_state_id: read_id(r, 3)?,
                        base_commit_sha: r.get(4)?,
                        harness: r.get(5)?,
                        harness_version: r.get(6)?,
                        prompt_digest: r.get(7)?,
                        state: read_state(r, 8)?,
                        workspace_id: read_id(r, 9)?,
                        pid_metadata: r.get(10)?,
                        exit_code: r.get(11)?,
                        exit_signal: r.get(19)?,
                        created_at: r.get(12)?,
                        updated_at: r.get(13)?,
                        started_at: r.get(14)?,
                        finished_at: r.get(15)?,
                        error_kind: r.get(16)?,
                        artifact_manifest_id: read_optional_id(r, 17)?,
                        capture: r
                            .get::<_, Option<String>>(18)?
                            .map(|value| {
                                serde_json::from_str(&value)
                                    .map_err(|error| conversion_error(18, error))
                            })
                            .transpose()?,
                    })
                },
            )
            .optional()?)
    }

    pub fn workspace(&self, id: &WorkspaceId) -> Result<Option<Workspace>> {
        Self::query_workspace(&self.connection, id)
    }

    fn query_workspace(connection: &Connection, id: &WorkspaceId) -> Result<Option<Workspace>> {
        Ok(connection
            .query_row(
                "SELECT workspace_id, run_id, managed_path, state_id, state, lease_expires_at,
             usage_bytes, retention_reason FROM workspaces WHERE workspace_id = ?1",
                [id.as_str()],
                |r| {
                    Ok(Workspace {
                        workspace_id: read_id(r, 0)?,
                        run_id: read_id(r, 1)?,
                        managed_path: r.get(2)?,
                        state_id: read_id(r, 3)?,
                        state: read_state(r, 4)?,
                        lease_expires_at: r.get(5)?,
                        usage_bytes: r.get(6)?,
                        retention_reason: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    /// Finish the ownership-only fork phase. Failure preserves both rows and
    /// the reservation lease; neither outcome grants permission to delete.
    pub fn finish_workspace_fork(
        &mut self,
        id: &RunId,
        outcome: std::result::Result<u64, &str>,
        updated_at: i64,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (run, workspace): (RunState, WorkspaceState) = transaction
            .query_row(
                "SELECT r.state, w.state FROM runs r JOIN workspaces w
                 ON w.workspace_id = r.workspace_id WHERE r.run_id = ?1",
                [id.as_str()],
                |r| Ok((read_state(r, 0)?, read_state(r, 1)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        if (run, workspace) != (RunState::Preparing, WorkspaceState::Active) {
            return Err(StoreError::Conflict);
        }
        match outcome {
            Ok(bytes) => {
                transaction.execute(
                    "UPDATE workspaces SET usage_bytes = ?1, retention_reason = NULL
                     WHERE run_id = ?2",
                    params![bytes, id.as_str()],
                )?;
            }
            Err(reason) => {
                let mut next = run;
                next.transition(RunState::NeedsAttention)?;
                let mut next = workspace;
                next.transition(WorkspaceState::RecoveryRequired)?;
                transaction.execute(
                    "UPDATE runs SET state = 'NEEDS_ATTENTION', updated_at = ?1,
                     finished_at = ?1, error_kind = COALESCE(error_kind, 'workspace_fork') WHERE run_id = ?2",
                    params![updated_at, id.as_str()],
                )?;
                transaction.execute(
                    "UPDATE workspaces SET state = 'RECOVERY_REQUIRED', retention_reason = ?1
                     WHERE run_id = ?2",
                    params![reason, id.as_str()],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Atomic compare-and-set. Exact state replay is a no-op; stale competing intent fails.
    /// Workspace lifecycle changes never grant cleanup authorization.
    pub fn transition_run_workspace(
        &mut self,
        id: &RunId,
        change: RunWorkspaceTransition,
    ) -> Result<()> {
        let mut run = change.expected_run;
        run.transition(change.next_run)?;
        let mut workspace = change.expected_workspace;
        workspace.transition(change.next_workspace)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current_run, current_workspace): (RunState, WorkspaceState) = transaction.query_row(
            "SELECT r.state, w.state FROM runs r JOIN workspaces w ON w.workspace_id = r.workspace_id
             WHERE r.run_id = ?1",
            [id.as_str()],
            |r| Ok((read_state(r, 0)?, read_state(r, 1)?)),
        ).optional()?.ok_or(StoreError::NotFound)?;
        if (current_run, current_workspace) == (change.next_run, change.next_workspace) {
            return Ok(());
        }
        if (current_run, current_workspace) != (change.expected_run, change.expected_workspace) {
            return Err(StoreError::Conflict);
        }
        transaction.execute(
            "UPDATE runs SET state = ?1, updated_at = ?2,
             started_at = CASE WHEN ?1 = 'RUNNING' THEN COALESCE(started_at, ?2) ELSE started_at END,
             finished_at = CASE WHEN ?3 THEN COALESCE(finished_at, ?2) ELSE finished_at END
             WHERE run_id = ?4 AND state = ?5",
            params![change.next_run.sql_name(), change.updated_at, change.next_run.is_terminal(),
                id.as_str(), change.expected_run.sql_name()],
        )?;
        transaction.execute(
            "UPDATE workspaces SET state = ?1 WHERE run_id = ?2 AND state = ?3",
            params![
                change.next_workspace.sql_name(),
                id.as_str(),
                change.expected_workspace.sql_name()
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn validate_capture_context(context: &CaptureContext) -> Result<()> {
    let digest = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if context.artifact_limit_bytes == 0
        || context.artifact_limit_bytes > i64::MAX as u64
        || !digest(&context.profile.digest)
        || !digest(&context.profile.native_config_fingerprint)
    {
        return Err(StoreError::InvalidRecord("Invalid capture context"));
    }
    Ok(())
}

fn validate_artifacts(manifest: &ArtifactManifestId, artifacts: &[Artifact]) -> Result<()> {
    let prefix = format!("artifacts/{manifest}/");
    let kinds = [
        "stdout",
        "stderr",
        "events",
        "tracked_patch",
        "untracked_archive",
        "prompt",
        "manifest",
    ];
    if !kinds
        .iter()
        .all(|kind| artifacts.iter().filter(|file| file.kind == *kind).count() == 1)
        || artifacts.iter().any(|file| {
            file.sha256.len() != 64
                || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                || file.size_bytes > i64::MAX as u64
                || !file.relative_path.starts_with(&prefix)
                || Path::new(&file.relative_path)
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                || (file.kind == "manifest"
                    && file.relative_path.strip_prefix(&prefix) != Some("manifest.json"))
        })
    {
        return Err(StoreError::InvalidRecord(
            "Incomplete or invalid artifact manifest",
        ));
    }
    Ok(())
}

fn insert_artifacts(
    transaction: &rusqlite::Transaction<'_>,
    id: &RunId,
    artifacts: &[Artifact],
) -> Result<()> {
    for file in artifacts {
        transaction.execute(
            "INSERT INTO artifacts (run_id, kind, relative_path, sha256, size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.as_str(),
                file.kind,
                file.relative_path,
                file.sha256,
                file.size_bytes
            ],
        )?;
    }
    Ok(())
}

fn conversion_error(
    index: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(error))
}

fn read_id<T: FromStr<Err = FabricError>>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let value = row.get_ref(index)?.as_str()?;
    value
        .parse()
        .map_err(|error| conversion_error(index, error))
}

fn read_optional_id<T: FromStr<Err = FabricError>>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    if row.get_ref(index)?.data_type() == rusqlite::types::Type::Null {
        Ok(None)
    } else {
        read_id(row, index).map(Some)
    }
}

trait SqlState: Sized {
    fn sql_name(self) -> &'static str;
    fn from_sql_name(value: &str) -> Option<Self>;
}

macro_rules! sql_states {
    ($type:ty, $($variant:ident => $name:literal),+ $(,)?) => {
        impl SqlState for $type {
            fn sql_name(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }
            fn from_sql_name(value: &str) -> Option<Self> {
                match value { $($name => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

sql_states!(PreparedStateStatus, Preparing => "PREPARING", Ready => "READY", Failed => "FAILED");
sql_states!(RunState, Queued => "QUEUED", Preparing => "PREPARING", Running => "RUNNING",
    Finalizing => "FINALIZING", Succeeded => "SUCCEEDED", Failed => "FAILED",
    Cancelled => "CANCELLED", NeedsAttention => "NEEDS_ATTENTION");
sql_states!(WorkspaceState, Active => "ACTIVE", CleanupPending => "CLEANUP_PENDING",
    Reclaimed => "RECLAIMED", Retained => "RETAINED", RecoveryRequired => "RECOVERY_REQUIRED");

fn read_state<T: SqlState>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    T::from_sql_name(row.get_ref(index)?.as_str()?).ok_or_else(|| {
        conversion_error(
            index,
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid persisted state"),
        )
    })
}

fn read_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        project_id: read_id(row, 0)?,
        name: row.get(1)?,
        source_path: row.get(2)?,
        default_ref: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn read_prepared_state(row: &Row<'_>) -> rusqlite::Result<PreparedState> {
    Ok(PreparedState {
        state_id: read_id(row, 0)?,
        project_id: read_id(row, 1)?,
        preparation_key: row.get(2)?,
        source_commit_sha: row.get(3)?,
        recipe_digest: row.get(4)?,
        harness_environment_digest: row.get(5)?,
        prepared_path: row.get(6)?,
        status: read_state(row, 7)?,
        created_at: row.get(8)?,
        verified_at: row.get(9)?,
        size_bytes: row.get(10)?,
    })
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn schema12_preserves_v11_evidence_and_restricts_recovery_associations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fabric.db");
        let mut store = Store::open(&path).unwrap();
        store
            .connection
            .execute_batch(
                "INSERT INTO projects VALUES ('p', 'project', '/source', 'HEAD', 100);
             INSERT INTO prepared_states VALUES
               ('s', 'p', 'key', 'commit', 'recipe', NULL, '/prepared', 'READY', 100, 100, 0);",
            )
            .unwrap();
        fn pair(store: &Store, id: &str) -> RunId {
            let transaction = store.connection.unchecked_transaction().unwrap();
            transaction
                .execute(
                    "INSERT INTO runs (run_id, project_id, prepared_state_id, base_commit_sha,
                  harness, prompt_digest, state, workspace_id, created_at, updated_at)
                 VALUES (?1, 'p', 's', 'commit', 'fake', 'prompt', 'PREPARING', ?1, 100, 100)",
                    [id],
                )
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO workspaces (workspace_id, run_id, managed_path, state_id,
                  state, usage_bytes) VALUES (?1, ?1, ?1, 's', 'ACTIVE', 42)",
                    [id],
                )
                .unwrap();
            transaction.commit().unwrap();
            id.parse().unwrap()
        }
        let legacy = pair(&store, "legacy");
        let empty = fabric_domain::CapturedFile {
            size_bytes: 0,
            sha256: "0".repeat(64),
        };
        let capture = CaptureMetadata {
            limit_bytes: 100,
            persisted_bytes: 0,
            truncated: false,
            stdout_bytes_seen: 0,
            stderr_bytes_seen: 0,
            stdout: empty.clone(),
            stderr: empty.clone(),
            events: empty,
            event_counts: fabric_domain::EventCounts::default(),
            normalized_event_count: 0,
            native_failure: None,
        };
        store.start_run(&legacy, "1", 123, 101).unwrap();
        store
            .observe_run_exit(&legacy, Some(7), None, None, 102)
            .unwrap();
        store.record_capture(&legacy, &capture).unwrap();
        store
            .fail_run_execution(&legacy, "1", "capture interrupted", 103, false)
            .unwrap();
        // Reconstruct the actual v11 schema boundary, not legacy context evidence.
        store.connection.execute_batch(
            "DROP TABLE run_environments;
             DROP TABLE environments;
             DROP TABLE run_launch_receipts;
             DROP TABLE prepared_source_evidence;
             DROP TABLE connector_deliveries;
             DROP TABLE run_targets;
             DROP TABLE run_observations;
             DROP TABLE run_capture_contexts;
             DROP TRIGGER immutable_manifest;
             CREATE TRIGGER immutable_manifest BEFORE UPDATE OF artifact_manifest_id ON runs
             WHEN (OLD.artifact_manifest_id IS NOT NULL
                AND NEW.artifact_manifest_id IS NOT OLD.artifact_manifest_id)
               OR (OLD.artifact_manifest_id IS NULL AND NEW.artifact_manifest_id IS NOT NULL
                   AND (OLD.state != 'FINALIZING'
                       OR NEW.state NOT IN ('SUCCEEDED', 'FAILED', 'CANCELLED')
                       OR NOT EXISTS (SELECT 1 FROM artifacts
                           WHERE run_id = NEW.run_id AND kind = 'manifest'
                             AND relative_path = 'artifacts/' || NEW.artifact_manifest_id || '/manifest.json')))
             BEGIN SELECT RAISE(ABORT, 'manifest association is immutable'); END;
             PRAGMA user_version = 11;",
        ).unwrap();
        let before = store.run(&legacy).unwrap().unwrap();
        let workspace = store.workspace(&before.workspace_id).unwrap().unwrap();
        assert!(store.capture_context(&legacy).unwrap().is_none());
        drop(store);
        let readonly = Store::open_existing_read_only(&path).unwrap();
        assert!(readonly.capture_context(&legacy).unwrap().is_none());
        drop(readonly);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.run(&legacy).unwrap().as_ref(), Some(&before));
        assert_eq!(
            store.workspace(&workspace.workspace_id).unwrap().as_ref(),
            Some(&workspace)
        );
        assert!(store.capture_context(&legacy).unwrap().is_none());
        let context = CaptureContext {
            profile: fabric_domain::ProfileEvidence {
                digest: "a".repeat(64),
                native_config_fingerprint: "b".repeat(64),
                portable: true,
                diagnostics: Vec::new(),
            },
            artifact_limit_bytes: 1024,
        };
        assert!(store.record_capture_context(&legacy, &context).is_err());
        let modern = pair(&store, "modern");
        assert!(store.record_capture_context(&modern, &context).is_err());
        let attempt = WorkerAttempt {
            attempt_id: "attempt".into(),
            run_id: modern.clone(),
            harness_requested: "fake".parse().unwrap(),
            harness_resolved: "fake".parse().unwrap(),
            harness_version: "1".into(),
            model_requested: None,
            model_resolved: None,
            profile_digest: Some(context.profile.digest.clone()),
            backend_id: "trusted-local".into(),
            event_cursor: 0,
            created_at: 100,
            started_at: None,
            finished_at: None,
            exit_code: None,
            exit_signal: None,
            termination_reason: None,
        };
        store.record_worker_attempt(&attempt).unwrap();
        let mut mismatch = context.clone();
        mismatch.profile.digest = "c".repeat(64);
        assert!(store.record_capture_context(&modern, &mismatch).is_err());
        store.record_capture_context(&modern, &context).unwrap();
        assert_eq!(
            serde_json::to_value(store.capture_context(&modern).unwrap().unwrap()).unwrap(),
            serde_json::to_value(&context).unwrap(),
        );
        assert!(store.record_capture_context(&modern, &context).is_err());
        for sql in [
            "UPDATE run_capture_contexts SET context_json = context_json",
            "DELETE FROM run_capture_contexts",
            "INSERT OR REPLACE INTO run_capture_contexts SELECT * FROM run_capture_contexts",
        ] {
            assert!(store.connection.execute(sql, []).is_err(), "{sql}");
        }
        let manifest: ArtifactManifestId = "recovered".parse().unwrap();
        let files: Vec<_> = [
            "stdout",
            "stderr",
            "events",
            "tracked_patch",
            "untracked_archive",
            "prompt",
            "manifest",
        ]
        .into_iter()
        .map(|kind| Artifact {
            kind: kind.into(),
            relative_path: format!(
                "artifacts/{manifest}/{}",
                if kind == "manifest" {
                    "manifest.json"
                } else {
                    kind
                }
            ),
            sha256: "0".repeat(64),
            size_bytes: 0,
        })
        .collect();
        let mut stale = before.clone();
        stale.updated_at += 1;
        assert!(matches!(
            store.recover_artifacts(&stale, &workspace, &manifest, &files),
            Err(StoreError::Conflict),
        ));
        assert!(
            store
                .recover_artifacts(&before, &workspace, &manifest, &files[..6])
                .is_err()
        );
        assert!(store.artifacts(&legacy).unwrap().is_empty());
        // A manifest row alone cannot authorize recovery without persisted capture.
        store.connection.execute(
            "INSERT INTO artifacts VALUES ('modern', 'manifest', 'artifacts/recovered/manifest.json', ?1, 0)",
            ["0".repeat(64)],
        ).unwrap();
        store
            .connection
            .execute(
                "UPDATE runs SET state = 'NEEDS_ATTENTION' WHERE run_id = 'modern'",
                [],
            )
            .unwrap();
        assert!(
            store
                .connection
                .execute(
                    "UPDATE runs SET artifact_manifest_id = 'recovered' WHERE run_id = 'modern'",
                    [],
                )
                .is_err()
        );
        store
            .connection
            .execute(
                "UPDATE artifacts SET sha256 = sha256 WHERE run_id = 'modern'",
                [],
            )
            .unwrap_err();
        // The preceding immutable row deliberately occupies the same manifest path.
        assert!(
            store
                .recover_artifacts(&before, &workspace, &manifest, &files)
                .is_err()
        );
        assert_eq!(store.run(&legacy).unwrap().as_ref(), Some(&before));
        let manifest: ArtifactManifestId = "legacy-recovered".parse().unwrap();
        let files: Vec<_> = files
            .into_iter()
            .map(|mut file| {
                file.relative_path = file.relative_path.replacen(
                    "artifacts/recovered/",
                    "artifacts/legacy-recovered/",
                    1,
                );
                file
            })
            .collect();
        {
            let transaction = store.connection.unchecked_transaction().unwrap();
            insert_artifacts(&transaction, &legacy, &files).unwrap();
            assert!(
                transaction
                    .execute(
                        "UPDATE runs SET artifact_manifest_id = 'legacy-recovered',
                 updated_at = updated_at + 1 WHERE run_id = 'legacy'",
                        [],
                    )
                    .is_err()
            );
            assert!(
                transaction
                    .execute(
                        "UPDATE runs SET artifact_manifest_id = 'legacy-recovered',
                 exit_code = 0 WHERE run_id = 'legacy'",
                        [],
                    )
                    .is_err()
            );
            assert!(
                transaction
                    .execute(
                        "UPDATE runs SET artifact_manifest_id = 'legacy-recovered',
                 capture_metadata = NULL WHERE run_id = 'legacy'",
                        [],
                    )
                    .is_err()
            );
            transaction.rollback().unwrap();
        }
        store
            .recover_artifacts(&before, &workspace, &manifest, &files)
            .unwrap();
        let mut recovered = before.clone();
        recovered.artifact_manifest_id = Some(manifest);
        assert_eq!(store.run(&legacy).unwrap().as_ref(), Some(&recovered));
        let reclaimed = store.workspace(&workspace.workspace_id).unwrap().unwrap();
        assert_eq!(reclaimed.state, WorkspaceState::CleanupPending);
        assert_eq!(reclaimed.lease_expires_at, None);
        assert_eq!(reclaimed.retention_reason, None);
        assert_eq!(store.artifacts(&legacy).unwrap().len(), 7);
        assert!(
            store
                .connection
                .execute(
                    "UPDATE runs SET artifact_manifest_id = 'replacement' WHERE run_id = 'legacy'",
                    [],
                )
                .is_err()
        );
        assert!(
            store
                .connection
                .execute("DELETE FROM artifacts WHERE run_id = 'legacy'", [],)
                .is_err()
        );
        store
            .record_cleanup_intent(&recovered, &reclaimed, "verified", 200)
            .unwrap();
        store
            .finish_cleanup(&workspace.workspace_id, "verified")
            .unwrap();
        assert_eq!(store.run(&legacy).unwrap().as_ref(), Some(&recovered));
        assert!(
            store
                .recover_artifacts(
                    &before,
                    &workspace,
                    recovered.artifact_manifest_id.as_ref().unwrap(),
                    &files
                )
                .is_err()
        );
    }
}
