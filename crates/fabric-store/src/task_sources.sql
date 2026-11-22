CREATE TABLE batch_task_sources (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES batch_tasks(run_id),
    prepared_state_id TEXT NOT NULL REFERENCES prepared_states(state_id),
    provenance_json TEXT
);
INSERT INTO batch_task_sources(run_id, prepared_state_id)
SELECT t.run_id, b.prepared_state_id FROM batch_tasks t JOIN batches b USING(batch_id);
CREATE INDEX batch_task_sources_state ON batch_task_sources(prepared_state_id);
CREATE TRIGGER immutable_task_source_update BEFORE UPDATE ON batch_task_sources
BEGIN SELECT RAISE(ABORT, 'accepted task source is immutable'); END;
CREATE TRIGGER immutable_task_source_delete BEFORE DELETE ON batch_task_sources
BEGIN SELECT RAISE(ABORT, 'accepted task source is immutable'); END;
CREATE TRIGGER valid_task_source BEFORE INSERT ON batch_task_sources
WHEN NOT EXISTS (
    SELECT 1 FROM batch_tasks t JOIN batches b USING(batch_id)
    JOIN prepared_states base ON base.state_id = b.prepared_state_id
    JOIN prepared_states selected ON selected.state_id = NEW.prepared_state_id
    WHERE t.run_id = NEW.run_id AND selected.project_id = base.project_id
      AND selected.status = 'READY'
      AND (NEW.provenance_json IS NULL OR (
          json_extract(NEW.provenance_json, '$.prepared_state_id') = NEW.prepared_state_id
          AND json_extract(NEW.provenance_json, '$.base_commit_sha') = selected.source_commit_sha
      ))
)
BEGIN SELECT RAISE(ABORT, 'task source requires matching READY project state'); END;
DROP TRIGGER run_requires_batch_task;
CREATE TRIGGER run_requires_batch_task BEFORE INSERT ON runs
WHEN NEW.batch_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM batch_tasks t JOIN batch_task_sources s USING(run_id)
    WHERE t.run_id = NEW.run_id AND t.batch_id = NEW.batch_id
      AND s.prepared_state_id = NEW.prepared_state_id
)
BEGIN SELECT RAISE(ABORT, 'run requires matching batch task and prepared state'); END;
