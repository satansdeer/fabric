CREATE TABLE environments (
    environment_id TEXT PRIMARY KEY NOT NULL,
    owner TEXT NOT NULL CHECK(length(owner) BETWEEN 1 AND 128),
    owner_uid INTEGER NOT NULL,
    target_json TEXT NOT NULL,
    capabilities_json TEXT NOT NULL,
    resource_json TEXT,
    desired TEXT NOT NULL CHECK(desired IN ('available','destroyed')),
    runtime TEXT NOT NULL CHECK(runtime IN ('available','absent','unknown')),
    observed_at INTEGER NOT NULL CHECK(observed_at >= 0),
    observation TEXT NOT NULL,
    destroy_intent INTEGER NOT NULL DEFAULT 0 CHECK(destroy_intent IN (0,1))
);
CREATE TRIGGER environment_identity_immutable BEFORE UPDATE ON environments
WHEN NEW.environment_id != OLD.environment_id OR NEW.owner != OLD.owner
 OR NEW.owner_uid != OLD.owner_uid OR NEW.target_json != OLD.target_json
 OR NEW.capabilities_json != OLD.capabilities_json
 OR (OLD.resource_json IS NOT NULL AND NEW.resource_json IS NOT OLD.resource_json)
 OR NEW.destroy_intent < OLD.destroy_intent
 OR (OLD.desired = 'destroyed' AND NEW.desired != 'destroyed')
BEGIN SELECT RAISE(ABORT, 'immutable environment ownership'); END;
CREATE TRIGGER environment_no_delete BEFORE DELETE ON environments
BEGIN SELECT RAISE(ABORT, 'environment tombstones cannot be deleted'); END;
CREATE TABLE run_environments (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(run_id),
    environment_id TEXT NOT NULL REFERENCES environments(environment_id)
);
CREATE TRIGGER run_environment_insert BEFORE INSERT ON run_environments
WHEN NOT EXISTS (
 SELECT 1 FROM environments e JOIN run_targets t ON t.run_id = NEW.run_id
 JOIN runs r ON r.run_id = NEW.run_id
 WHERE e.environment_id = NEW.environment_id AND e.desired = 'available'
 AND e.runtime = 'available' AND e.destroy_intent = 0 AND e.resource_json IS NOT NULL
 AND t.pin = e.target_json AND r.state = 'PREPARING'
)
BEGIN SELECT RAISE(ABORT, 'environment requires ready ownership and matching prelaunch target'); END;
CREATE TRIGGER run_environment_immutable BEFORE UPDATE ON run_environments
BEGIN SELECT RAISE(ABORT, 'immutable run environment'); END;
CREATE TRIGGER run_environment_no_delete BEFORE DELETE ON run_environments
BEGIN SELECT RAISE(ABORT, 'immutable run environment'); END;
