//! Durable local records. Committed ownership is required before filesystem use.

use std::{fmt, fs::OpenOptions, path::Path, str::FromStr, time::Duration};

use fabric_domain::{
    ArtifactManifestId, BatchId, CaptureMetadata, FabricError, HarnessId, HarnessVersion,
    PreparedStateId, PreparedStateStatus, ProjectId, RunId, RunState, WorkspaceId, WorkspaceState,
};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

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
            1..=9 => {}
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
        if !matches!(version, 5..=9) {
            return Err(StoreError::UnsupportedSchema(version));
        }
        Ok(Self { connection })
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
            RunState::Succeeded | RunState::Failed | RunState::Cancelled
        ) || run.workspace_id != workspace.workspace_id
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
    pub fn create_batch(
        &mut self,
        id: &BatchId,
        state: &PreparedStateId,
        tasks: &[BatchTask],
    ) -> Result<()> {
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
        Ok(self.connection.query_row(
            "SELECT CASE
             WHEN status != 'READY' THEN 'not_ready'
             WHEN EXISTS (SELECT 1 FROM runs WHERE prepared_state_id = ?1)
               OR EXISTS (SELECT 1 FROM batches WHERE prepared_state_id = ?1)
               OR EXISTS (SELECT 1 FROM workspaces WHERE state_id = ?1) THEN 'historical_reference'
             WHEN ?2 THEN 'admitted_worker'
             WHEN NOT EXISTS (SELECT 1 FROM prepared_states newer
               WHERE newer.project_id = prepared_states.project_id AND newer.status = 'READY'
                 AND newer.source_commit_sha = prepared_states.source_commit_sha
                 AND (newer.verified_at > prepared_states.verified_at
                   OR (newer.verified_at = prepared_states.verified_at AND newer.state_id > prepared_states.state_id)))
               THEN 'selected_state' ELSE NULL END
             FROM prepared_states WHERE state_id = ?1", params![id.as_str(), reserved], |row| row.get(0))?)
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
             error_kind = COALESCE(?5, CASE WHEN ?1 = 0 THEN NULL ELSE 'harness_exit' END)
             WHERE run_id = ?3 AND state = 'RUNNING' AND ?2 >= updated_at",
            params![exit, now, id.as_str(), signal, reason],
        )?;
        if count != 1 {
            return Err(StoreError::Conflict);
        }
        Ok(())
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
        let prefix = format!("artifacts/{manifest}/");
        if ![
            "stdout",
            "stderr",
            "events",
            "tracked_patch",
            "untracked_archive",
            "prompt",
            "manifest",
        ]
        .iter()
        .all(|kind| artifacts.iter().filter(|file| file.kind == *kind).count() == 1)
            || artifacts.iter().any(|file| {
                file.sha256.len() != 64
                    || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || !file.relative_path.starts_with(&prefix)
                    || Path::new(&file.relative_path)
                        .components()
                        .any(|part| !matches!(part, std::path::Component::Normal(_)))
            })
        {
            return Err(StoreError::InvalidRecord(
                "Incomplete or invalid artifact manifest",
            ));
        }
        let encoded = serde_json::to_string(capture)
            .map_err(|_| StoreError::InvalidRecord("Cannot encode capture metadata"))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
               WHEN error_kind IN ('timeout', 'cancelled', 'workspace_limit', 'storage_pressure', 'storage_meter_failed', 'native_failure') THEN error_kind
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
                     finished_at = ?1, error_kind = 'workspace_fork' WHERE run_id = ?2",
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
