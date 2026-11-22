-- Durable local admission. Prompts live in the private database, never shared temporary files.
CREATE TABLE IF NOT EXISTS async_batches (
    batch_id TEXT PRIMARY KEY NOT NULL REFERENCES batches(batch_id),
    idempotency_key TEXT UNIQUE CHECK(idempotency_key IS NULL OR length(idempotency_key) BETWEEN 1 AND 256),
    request_digest TEXT NOT NULL CHECK(length(request_digest) = 64),
    concurrency INTEGER NOT NULL CHECK(concurrency BETWEEN 1 AND 128),
    options_json TEXT NOT NULL,
    fake_binary TEXT,
    created_at INTEGER NOT NULL CHECK(created_at >= 0)
);
CREATE TABLE IF NOT EXISTS async_tasks (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES batch_tasks(run_id),
    prompt BLOB NOT NULL CHECK(length(prompt) <= 65536),
    harness TEXT NOT NULL,
    plan_json TEXT NOT NULL,
    consent_stamp TEXT,
    state TEXT NOT NULL CHECK(state IN ('QUEUED','PREPARING','RUNNING','FINALIZING','SUCCEEDED','FAILED','CANCELLED','NEEDS_ATTENTION'))
);
CREATE TABLE IF NOT EXISTS async_events (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id TEXT NOT NULL REFERENCES async_batches(batch_id),
    run_id TEXT NOT NULL REFERENCES async_tasks(run_id),
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS async_events_batch ON async_events(batch_id, cursor);
CREATE TRIGGER IF NOT EXISTS async_task_accepted AFTER INSERT ON async_tasks
BEGIN
    INSERT INTO async_events(batch_id,run_id,state,created_at)
      SELECT b.batch_id,NEW.run_id,NEW.state,b.created_at FROM async_batches b
      JOIN batch_tasks t ON t.batch_id=b.batch_id WHERE t.run_id=NEW.run_id;
END;
CREATE TRIGGER IF NOT EXISTS async_task_changed AFTER UPDATE OF state ON async_tasks
WHEN OLD.state != NEW.state
BEGIN
    INSERT INTO async_events(batch_id,run_id,state,created_at)
      SELECT batch_id,NEW.run_id,NEW.state,CAST(strftime('%s','now') AS INTEGER)
      FROM batch_tasks WHERE run_id=NEW.run_id;
END;
CREATE TRIGGER IF NOT EXISTS async_run_changed AFTER UPDATE OF state ON runs
WHEN OLD.state != NEW.state AND EXISTS (SELECT 1 FROM async_tasks WHERE run_id=NEW.run_id)
BEGIN
    INSERT INTO async_events(batch_id,run_id,state,created_at)
      VALUES(NEW.batch_id,NEW.run_id,NEW.state,NEW.updated_at);
END;
-- A global cursor and per-batch retention permit reconnects without an unbounded log.
CREATE TRIGGER IF NOT EXISTS async_event_bound AFTER INSERT ON async_events
BEGIN
    DELETE FROM async_events WHERE batch_id=NEW.batch_id AND cursor <= (
      SELECT cursor FROM async_events WHERE batch_id=NEW.batch_id
      ORDER BY cursor DESC LIMIT 1 OFFSET 1024
    );
END;
CREATE TRIGGER IF NOT EXISTS immutable_async_admission BEFORE UPDATE ON async_batches
BEGIN SELECT RAISE(ABORT,'async admission is immutable'); END;
CREATE TRIGGER IF NOT EXISTS immutable_async_task BEFORE UPDATE ON async_tasks
WHEN NEW.run_id != OLD.run_id OR NEW.prompt != OLD.prompt OR NEW.harness != OLD.harness
  OR NEW.plan_json != OLD.plan_json OR NEW.consent_stamp IS NOT OLD.consent_stamp
  OR (OLD.state != 'QUEUED' AND NEW.state = 'QUEUED')
  OR (OLD.state IN ('SUCCEEDED','FAILED','CANCELLED','NEEDS_ATTENTION') AND NEW.state != OLD.state)
BEGIN SELECT RAISE(ABORT,'async task provenance and terminal state are immutable'); END;
