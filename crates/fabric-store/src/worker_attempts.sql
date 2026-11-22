CREATE TABLE worker_attempts (
    attempt_id TEXT PRIMARY KEY NOT NULL CHECK(length(attempt_id) BETWEEN 1 AND 128 AND attempt_id NOT GLOB '*[^A-Za-z0-9_-]*'),
    run_id TEXT NOT NULL REFERENCES runs(run_id),
    harness_requested TEXT NOT NULL CHECK(length(harness_requested) BETWEEN 1 AND 128 AND harness_requested NOT GLOB '*[^a-z0-9-]*'),
    harness_resolved TEXT NOT NULL CHECK(length(harness_resolved) BETWEEN 1 AND 128 AND harness_resolved NOT GLOB '*[^a-z0-9-]*'),
    harness_version TEXT NOT NULL CHECK(length(trim(harness_version)) BETWEEN 1 AND 1024),
    model_requested TEXT,
    model_resolved TEXT,
    profile_digest TEXT,
    backend_id TEXT NOT NULL CHECK(length(trim(backend_id)) > 0),
    event_cursor INTEGER NOT NULL CHECK(event_cursor >= 0),
    created_at INTEGER NOT NULL CHECK(created_at >= 0),
    started_at INTEGER CHECK(started_at >= created_at),
    finished_at INTEGER CHECK(finished_at >= created_at AND (started_at IS NULL OR finished_at >= started_at)),
    exit_code INTEGER CHECK(exit_code BETWEEN -2147483648 AND 2147483647),
    exit_signal INTEGER CHECK(exit_signal > 0 AND exit_signal <= 2147483647),
    termination_reason TEXT CHECK(length(trim(termination_reason)) > 0),
    CHECK(exit_code IS NULL OR exit_signal IS NULL),
    CHECK((exit_code IS NULL AND exit_signal IS NULL) OR (started_at IS NOT NULL AND finished_at IS NOT NULL)),
    CHECK(termination_reason IS NULL OR finished_at IS NOT NULL),
    CHECK(finished_at IS NULL OR exit_code IS NOT NULL OR exit_signal IS NOT NULL OR termination_reason IS NOT NULL)
);
CREATE INDEX worker_attempts_by_run ON worker_attempts(run_id, attempt_id);

CREATE TRIGGER worker_attempt_created_after_run BEFORE INSERT ON worker_attempts
WHEN NEW.created_at < (SELECT created_at FROM runs WHERE run_id = NEW.run_id)
BEGIN SELECT RAISE(ABORT, 'worker attempt predates run'); END;

CREATE TRIGGER worker_attempt_immutable_evidence BEFORE UPDATE ON worker_attempts
WHEN NEW.attempt_id IS NOT OLD.attempt_id OR NEW.run_id IS NOT OLD.run_id
    OR NEW.harness_requested IS NOT OLD.harness_requested
    OR NEW.harness_resolved IS NOT OLD.harness_resolved
    OR NEW.harness_version IS NOT OLD.harness_version
    OR NEW.model_requested IS NOT OLD.model_requested
    OR NEW.model_resolved IS NOT OLD.model_resolved
    OR NEW.profile_digest IS NOT OLD.profile_digest
    OR NEW.backend_id IS NOT OLD.backend_id
    OR NEW.created_at IS NOT OLD.created_at
    OR (OLD.started_at IS NOT NULL AND NEW.started_at IS NOT OLD.started_at)
    OR NEW.event_cursor < OLD.event_cursor
    OR (OLD.finished_at IS NOT NULL AND (
        NEW.event_cursor IS NOT OLD.event_cursor
        OR NEW.started_at IS NOT OLD.started_at
        OR NEW.finished_at IS NOT OLD.finished_at
        OR NEW.exit_code IS NOT OLD.exit_code
        OR NEW.exit_signal IS NOT OLD.exit_signal
        OR NEW.termination_reason IS NOT OLD.termination_reason))
BEGIN SELECT RAISE(ABORT, 'worker attempt evidence cannot be rewritten'); END;

CREATE TRIGGER worker_attempt_no_delete BEFORE DELETE ON worker_attempts
BEGIN SELECT RAISE(ABORT, 'worker attempt evidence cannot be deleted'); END;
