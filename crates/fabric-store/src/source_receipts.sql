-- New evidence is never backfilled from legacy READY bytes.
CREATE TABLE prepared_source_evidence (
    state_id TEXT PRIMARY KEY NOT NULL REFERENCES prepared_states(state_id) ON DELETE CASCADE,
    checkout_sha256 TEXT NOT NULL CHECK(length(checkout_sha256)=64 AND checkout_sha256 NOT GLOB '*[^0-9a-f]*'),
    prepared_sha256 TEXT CHECK(prepared_sha256 IS NULL OR
        (length(prepared_sha256)=64 AND prepared_sha256 NOT GLOB '*[^0-9a-f]*'))
);
CREATE TRIGGER source_checkout_preparing BEFORE INSERT ON prepared_source_evidence
WHEN NEW.prepared_sha256 IS NOT NULL OR NOT EXISTS (
    SELECT 1 FROM prepared_states WHERE state_id=NEW.state_id AND status='PREPARING'
)
BEGIN SELECT RAISE(ABORT, 'checkout evidence requires fresh preparation'); END;
CREATE TRIGGER source_prepared_once BEFORE UPDATE ON prepared_source_evidence
WHEN NEW.state_id IS NOT OLD.state_id OR NEW.checkout_sha256 IS NOT OLD.checkout_sha256
    OR OLD.prepared_sha256 IS NOT NULL OR NEW.prepared_sha256 IS NULL
    OR NOT EXISTS (SELECT 1 FROM prepared_states WHERE state_id=OLD.state_id AND status='PREPARING')
BEGIN SELECT RAISE(ABORT, 'source evidence is immutable'); END;
-- Permit only the existing cache-eviction transaction to remove unused evidence.
CREATE TRIGGER source_evidence_delete BEFORE DELETE ON prepared_source_evidence
WHEN NOT EXISTS (SELECT 1 FROM cache_evictions WHERE state_id=OLD.state_id)
BEGIN SELECT RAISE(ABORT, 'source evidence requires cache eviction'); END;

CREATE TABLE run_launch_receipts (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
    receipt_json TEXT NOT NULL CHECK(json_valid(receipt_json))
);
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
      AND json_extract(NEW.receipt_json, '$.run_id')=r.run_id
      AND json_extract(NEW.receipt_json, '$.attempt_id')=a.attempt_id
      AND json_extract(NEW.receipt_json, '$.workspace_id')=w.workspace_id
      AND json_extract(NEW.receipt_json, '$.workspace_path')=w.managed_path
      AND json_type(NEW.receipt_json, '$.workspace_device')='integer'
      AND json_type(NEW.receipt_json, '$.workspace_inode')='integer'
      AND json_extract(NEW.receipt_json, '$.prepared_state_id')=p.state_id
      AND json_extract(NEW.receipt_json, '$.source_commit_sha')=p.source_commit_sha
      AND json_extract(NEW.receipt_json, '$.preparation_key')=p.preparation_key
      AND json_extract(NEW.receipt_json, '$.recipe_digest')=p.recipe_digest
      AND json_extract(NEW.receipt_json, '$.source.checkout_sha256')=s.checkout_sha256
      AND json_extract(NEW.receipt_json, '$.source.prepared_sha256')=s.prepared_sha256
      AND json_extract(NEW.receipt_json, '$.actual_sha256')=s.prepared_sha256
      AND NOT EXISTS (SELECT 1 FROM cleanup_intents WHERE workspace_id=w.workspace_id)
) OR EXISTS (SELECT 1 FROM worker_attempts WHERE run_id=NEW.run_id
    AND (started_at IS NOT NULL OR finished_at IS NOT NULL))
BEGIN SELECT RAISE(ABORT, 'launch receipt requires matching prestart source evidence'); END;
CREATE TRIGGER launch_receipt_insert_once BEFORE INSERT ON run_launch_receipts
WHEN EXISTS (SELECT 1 FROM run_launch_receipts WHERE run_id=NEW.run_id)
BEGIN SELECT RAISE(ABORT, 'launch receipt is immutable'); END;
CREATE TRIGGER launch_receipt_update BEFORE UPDATE ON run_launch_receipts
BEGIN SELECT RAISE(ABORT, 'launch receipt is immutable'); END;
CREATE TRIGGER launch_receipt_delete BEFORE DELETE ON run_launch_receipts
BEGIN SELECT RAISE(ABORT, 'launch receipt is immutable'); END;
