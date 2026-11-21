CREATE TABLE projects (
    project_id TEXT PRIMARY KEY NOT NULL CHECK(length(project_id) BETWEEN 1 AND 128 AND project_id NOT GLOB '*[^A-Za-z0-9_-]*'),
    name TEXT NOT NULL UNIQUE CHECK(length(name) > 0),
    source_path TEXT NOT NULL CHECK(length(source_path) > 0),
    default_ref TEXT NOT NULL CHECK(length(default_ref) > 0),
    created_at INTEGER NOT NULL CHECK(created_at >= 0)
);

CREATE TABLE prepared_states (
    state_id TEXT PRIMARY KEY NOT NULL CHECK(length(state_id) BETWEEN 1 AND 128 AND state_id NOT GLOB '*[^A-Za-z0-9_-]*'),
    project_id TEXT NOT NULL REFERENCES projects(project_id),
    preparation_key TEXT NOT NULL CHECK(length(preparation_key) > 0),
    source_commit_sha TEXT NOT NULL CHECK(length(source_commit_sha) > 0),
    recipe_digest TEXT NOT NULL CHECK(length(recipe_digest) > 0),
    harness_environment_digest TEXT,
    prepared_path TEXT NOT NULL UNIQUE CHECK(length(prepared_path) > 0),
    status TEXT NOT NULL CHECK(status IN ('PREPARING', 'READY', 'FAILED')),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    verified_at INTEGER CHECK(verified_at >= created_at),
    size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
    CHECK(status != 'READY' OR verified_at IS NOT NULL),
    UNIQUE(project_id, preparation_key),
    UNIQUE(state_id, project_id, source_commit_sha)
);

CREATE TRIGGER immutable_ready_state BEFORE UPDATE ON prepared_states
WHEN OLD.status = 'READY'
BEGIN SELECT RAISE(ABORT, 'READY prepared states are immutable'); END;

CREATE TABLE runs (
    run_id TEXT PRIMARY KEY NOT NULL CHECK(length(run_id) BETWEEN 1 AND 128 AND run_id NOT GLOB '*[^A-Za-z0-9_-]*'),
    batch_id TEXT CHECK(batch_id IS NULL OR (length(batch_id) BETWEEN 1 AND 128 AND batch_id NOT GLOB '*[^A-Za-z0-9_-]*')),
    project_id TEXT NOT NULL REFERENCES projects(project_id),
    prepared_state_id TEXT NOT NULL,
    base_commit_sha TEXT NOT NULL,
    harness TEXT NOT NULL CHECK(length(harness) > 0),
    harness_version TEXT,
    prompt_digest TEXT NOT NULL CHECK(length(prompt_digest) > 0),
    state TEXT NOT NULL CHECK(state IN ('QUEUED', 'PREPARING', 'RUNNING', 'FINALIZING', 'SUCCEEDED', 'FAILED', 'CANCELLED', 'NEEDS_ATTENTION')),
    workspace_id TEXT NOT NULL UNIQUE,
    pid_metadata TEXT,
    exit_code INTEGER,
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    updated_at INTEGER NOT NULL CHECK(updated_at >= created_at),
    started_at INTEGER CHECK(started_at >= created_at),
    finished_at INTEGER CHECK(finished_at >= created_at),
    error_kind TEXT,
    artifact_manifest_id TEXT CHECK(artifact_manifest_id IS NULL OR (length(artifact_manifest_id) BETWEEN 1 AND 128 AND artifact_manifest_id NOT GLOB '*[^A-Za-z0-9_-]*')),
    UNIQUE(run_id, workspace_id, prepared_state_id),
    FOREIGN KEY(prepared_state_id, project_id, base_commit_sha) REFERENCES prepared_states(state_id, project_id, source_commit_sha),
    FOREIGN KEY(workspace_id, run_id, prepared_state_id) REFERENCES workspaces(workspace_id, run_id, state_id) DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE workspaces (
    workspace_id TEXT PRIMARY KEY NOT NULL CHECK(length(workspace_id) BETWEEN 1 AND 128 AND workspace_id NOT GLOB '*[^A-Za-z0-9_-]*'),
    run_id TEXT NOT NULL UNIQUE,
    managed_path TEXT NOT NULL UNIQUE CHECK(length(managed_path) > 0),
    state_id TEXT NOT NULL REFERENCES prepared_states(state_id),
    state TEXT NOT NULL CHECK(state IN ('ACTIVE', 'CLEANUP_PENDING', 'RECLAIMED', 'RETAINED', 'RECOVERY_REQUIRED')),
    lease_expires_at INTEGER CHECK(lease_expires_at >= 0),
    usage_bytes INTEGER NOT NULL CHECK(usage_bytes >= 0),
    retention_reason TEXT,
    UNIQUE(workspace_id, run_id, state_id),
    FOREIGN KEY(run_id, workspace_id, state_id) REFERENCES runs(run_id, workspace_id, prepared_state_id) DEFERRABLE INITIALLY DEFERRED
);

CREATE TRIGGER run_requires_ready_state BEFORE INSERT ON runs
WHEN NOT EXISTS (SELECT 1 FROM prepared_states WHERE state_id = NEW.prepared_state_id AND status = 'READY')
BEGIN SELECT RAISE(ABORT, 'run requires a READY prepared state'); END;

CREATE TRIGGER workspace_not_prepared_path BEFORE INSERT ON workspaces
WHEN EXISTS (SELECT 1 FROM prepared_states WHERE prepared_path = NEW.managed_path)
BEGIN SELECT RAISE(ABORT, 'prepared path cannot be a writable workspace'); END;
