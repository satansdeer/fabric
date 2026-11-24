-- A remote resource is not a controller inode or a controller deletion grant.
-- Preserve the complete local witness branch and all common prestart predicates.
DROP TRIGGER launch_receipt_prestart;
CREATE TRIGGER launch_receipt_prestart BEFORE INSERT ON run_launch_receipts
WHEN NOT EXISTS (
 SELECT 1 FROM runs r
 JOIN workspaces w ON w.workspace_id=r.workspace_id AND w.run_id=r.run_id
 JOIN worker_attempts a ON a.run_id=r.run_id
 JOIN prepared_states p ON p.state_id=r.prepared_state_id
 JOIN prepared_source_evidence s ON s.state_id=p.state_id
 WHERE r.run_id=NEW.run_id AND r.state='PREPARING'
 AND r.started_at IS NULL AND r.pid_metadata IS NULL AND r.finished_at IS NULL
 AND w.state='ACTIVE' AND w.retention_reason IS NULL AND w.state_id=p.state_id
 AND a.started_at IS NULL AND a.finished_at IS NULL
 AND json_extract(NEW.receipt_json,'$.run_id')=r.run_id
 AND json_extract(NEW.receipt_json,'$.attempt_id')=a.attempt_id
 AND json_extract(NEW.receipt_json,'$.workspace_id')=w.workspace_id
 AND json_extract(NEW.receipt_json,'$.prepared_state_id')=p.state_id
 AND json_extract(NEW.receipt_json,'$.source_commit_sha')=p.source_commit_sha
 AND json_extract(NEW.receipt_json,'$.preparation_key')=p.preparation_key
 AND json_extract(NEW.receipt_json,'$.recipe_digest')=p.recipe_digest
 AND json_extract(NEW.receipt_json,'$.source.checkout_sha256')=s.checkout_sha256
 AND json_extract(NEW.receipt_json,'$.source.prepared_sha256')=s.prepared_sha256
 AND json_extract(NEW.receipt_json,'$.actual_sha256')=s.prepared_sha256
 AND NOT EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id=w.workspace_id)
 AND (
   (json_type(NEW.receipt_json,'$.remote') IS NULL
    AND json_extract(NEW.receipt_json,'$.workspace_path')=w.managed_path
    AND json_type(NEW.receipt_json,'$.workspace_device')='integer'
    AND json_type(NEW.receipt_json,'$.workspace_inode')='integer'
    AND a.backend_id='trusted-local')
   OR
   (a.backend_id='remote.v1'
    AND json_type(NEW.receipt_json,'$.workspace_device') IS NULL
    AND json_type(NEW.receipt_json,'$.workspace_inode') IS NULL
    AND json_type(NEW.receipt_json,'$.remote')='object'
    AND json_extract(NEW.receipt_json,'$.workspace_path')=json_extract(NEW.receipt_json,'$.remote.resource.resource_id')
    AND length(json_extract(NEW.receipt_json,'$.remote.instance')) BETWEEN 1 AND 128
    AND json_extract(NEW.receipt_json,'$.remote.instance')=json_extract(NEW.receipt_json,'$.remote.resource.instance')
    AND json_extract(NEW.receipt_json,'$.remote.resource.run_id')=r.run_id
    AND length(json_extract(NEW.receipt_json,'$.remote.os')) BETWEEN 1 AND 32
    AND length(json_extract(NEW.receipt_json,'$.remote.arch')) BETWEEN 1 AND 32
    AND length(json_extract(NEW.receipt_json,'$.remote.hostname')) BETWEEN 1 AND 255
    AND EXISTS (
      SELECT 1 FROM run_targets t JOIN run_environments b ON b.run_id=t.run_id
      JOIN environments e ON e.environment_id=b.environment_id
      WHERE t.run_id=r.run_id AND t.pin=json_extract(NEW.receipt_json,'$.remote.target_json')
      AND e.target_json=t.pin AND e.desired='available' AND e.runtime='available'
      AND e.destroy_intent=0
      AND e.environment_id=json_extract(NEW.receipt_json,'$.remote.environment_id')
      AND e.environment_id=json_extract(NEW.receipt_json,'$.remote.resource.environment.environment_id')
      AND e.owner=json_extract(NEW.receipt_json,'$.remote.resource.environment.owner')
      AND json(e.resource_json)=json_extract(NEW.receipt_json,'$.remote.resource.environment.resource')
    ))
 )
) OR EXISTS (SELECT 1 FROM worker_attempts WHERE run_id=NEW.run_id
 AND (started_at IS NOT NULL OR finished_at IS NOT NULL))
BEGIN SELECT RAISE(ABORT, 'launch receipt requires matching prestart source evidence'); END;

CREATE TABLE run_remote_observations (
 run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
 evidence_json TEXT NOT NULL CHECK(json_valid(evidence_json))
);
CREATE TRIGGER remote_observation_binding BEFORE INSERT ON run_remote_observations
WHEN NOT EXISTS (SELECT 1 FROM run_targets t JOIN run_environments e ON e.run_id=t.run_id
 WHERE t.run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT, 'remote observation requires target and environment'); END;
CREATE TRIGGER remote_observation_immutable BEFORE UPDATE ON run_remote_observations
BEGIN SELECT RAISE(ABORT, 'remote observation is immutable'); END;
CREATE TRIGGER remote_observation_no_delete BEFORE DELETE ON run_remote_observations
BEGIN SELECT RAISE(ABORT, 'remote observation is immutable'); END;
