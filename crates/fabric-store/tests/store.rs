use std::sync::{Arc, Barrier};

use fabric_domain::{PreparedStateStatus, RunState, WorkspaceState};
use fabric_store::{
    PreparedState, Project, Run, RunWorkspaceTransition, Store, StoreError, WorkerAttempt,
    Workspace,
};
use rusqlite::Connection;
use tempfile::TempDir;

fn project() -> Project {
    Project {
        project_id: "project-1".parse().unwrap(),
        name: "fixture".into(),
        source_path: "/fixture/source".into(),
        default_ref: "HEAD".into(),
        created_at: 100,
    }
}

fn prepared() -> PreparedState {
    PreparedState {
        state_id: "state-1".parse().unwrap(),
        project_id: project().project_id,
        preparation_key: "preparation-key".into(),
        source_commit_sha: "commit-sha".into(),
        recipe_digest: "recipe-digest".into(),
        harness_environment_digest: Some("environment-digest".into()),
        prepared_path: "/managed/prepared/state-1".into(),
        status: PreparedStateStatus::Preparing,
        created_at: 110,
        verified_at: None,
        size_bytes: 0,
    }
}

fn pair(index: usize) -> (Run, Workspace) {
    let run = Run {
        run_id: format!("run-{index}").parse().unwrap(),
        batch_id: None,
        project_id: project().project_id,
        prepared_state_id: prepared().state_id,
        base_commit_sha: prepared().source_commit_sha,
        harness: "fake".into(),
        harness_version: Some("1".into()),
        prompt_digest: "prompt-digest".into(),
        state: RunState::Queued,
        workspace_id: format!("workspace-{index}").parse().unwrap(),
        pid_metadata: None,
        exit_code: None,
        exit_signal: None,
        created_at: 200,
        updated_at: 200,
        started_at: None,
        finished_at: None,
        error_kind: None,
        artifact_manifest_id: None,
        capture: None,
    };
    let workspace = Workspace {
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        managed_path: format!("/managed/workspaces/workspace-{index}"),
        state_id: run.prepared_state_id.clone(),
        state: WorkspaceState::Active,
        lease_expires_at: Some(300),
        usage_bytes: 42,
        retention_reason: None,
    };
    (run, workspace)
}

fn fixture() -> (TempDir, Store) {
    let root = tempfile::tempdir().unwrap();
    let mut store = Store::open(root.path().join("fabric.sqlite3")).unwrap();
    store.create_project(&project()).unwrap();
    store.create_prepared_state(&prepared()).unwrap();
    store
        .finish_preparation(
            &prepared().state_id,
            PreparedStateStatus::Ready,
            Some(120),
            42,
        )
        .unwrap();
    (root, store)
}

fn change(workspace: WorkspaceState) -> RunWorkspaceTransition {
    RunWorkspaceTransition {
        expected_run: RunState::Queued,
        next_run: RunState::Preparing,
        expected_workspace: WorkspaceState::Active,
        next_workspace: workspace,
        updated_at: 201,
    }
}

fn remove_async_schema(connection: &Connection) {
    connection
        .execute_batch(
            "DROP TRIGGER IF EXISTS async_task_accepted;
             DROP TRIGGER IF EXISTS async_task_changed;
             DROP TRIGGER IF EXISTS async_run_changed;
             DROP TRIGGER IF EXISTS async_event_bound;
             DROP TRIGGER IF EXISTS immutable_async_admission;
             DROP TRIGGER IF EXISTS immutable_async_task;
             DROP TABLE IF EXISTS async_events;
             DROP TABLE IF EXISTS async_tasks;
             DROP TABLE IF EXISTS async_batches;",
        )
        .unwrap();
}

fn remove_cleanup_schema(connection: &Connection) {
    remove_async_schema(connection);
    connection
        .execute_batch(
            "DROP TABLE IF EXISTS worker_attempts;
             DROP TABLE IF EXISTS storage_reservations;
             DROP TABLE IF EXISTS worker_leases;
             DROP TABLE IF EXISTS cache_evictions;
             DROP TRIGGER immutable_cleanup_intent_insert;
             DROP TRIGGER immutable_cleanup_intent_update;
             DROP TRIGGER immutable_cleanup_intent_delete;
             DROP TRIGGER cleanup_workspace_update;
             DROP TRIGGER cleanup_workspace_delete;
             DROP TRIGGER cleanup_workspace_insert;
             DROP TRIGGER cleanup_run_update;
             DROP TRIGGER cleanup_run_delete;
             DROP TRIGGER cleanup_run_insert;
             DROP TABLE cleanup_intents;",
        )
        .unwrap();
}

fn finalized_pair(store: &mut Store, index: usize) -> (Run, Workspace) {
    let (run, workspace) = pair(index);
    store.create_run_workspace(&run, &workspace).unwrap();
    store
        .transition_run_workspace(&run.run_id, change(WorkspaceState::Active))
        .unwrap();
    store.start_run(&run.run_id, "1", 123, 202).unwrap();
    store
        .observe_run_exit(&run.run_id, Some(0), None, None, 203)
        .unwrap();
    let empty = fabric_domain::CapturedFile {
        size_bytes: 0,
        sha256: "0".repeat(64),
    };
    let capture = fabric_domain::CaptureMetadata {
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
    let manifest = format!("manifest-{index}").parse().unwrap();
    let artifacts: Vec<_> = [
        "stdout",
        "stderr",
        "events",
        "tracked_patch",
        "untracked_archive",
        "prompt",
        "manifest",
    ]
    .into_iter()
    .map(|kind| fabric_store::Artifact {
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
    store
        .finish_run(&run.run_id, &capture, &manifest, &artifacts, 204)
        .unwrap();
    (
        store.run(&run.run_id).unwrap().unwrap(),
        store.workspace(&workspace.workspace_id).unwrap().unwrap(),
    )
}

#[test]
fn sqlite_store_opens_and_reopens_a_durable_database() {
    let (root, mut store) = fixture();
    let path = root.path().join("fabric.sqlite3");
    let (run, workspace) = pair(1);
    store.create_run_workspace(&run, &workspace).unwrap();
    drop(store);
    for _ in 0..3 {
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.project(&project().project_id).unwrap(),
            Some(project())
        );
        let mut ready = prepared();
        ready.status = PreparedStateStatus::Ready;
        ready.verified_at = Some(120);
        ready.size_bytes = 42;
        assert_eq!(
            reopened.prepared_state(&ready.state_id).unwrap(),
            Some(ready)
        );
        assert_eq!(reopened.run(&run.run_id).unwrap(), Some(run.clone()));
        assert_eq!(
            reopened.workspace(&workspace.workspace_id).unwrap(),
            Some(workspace.clone())
        );
    }
    let connection = Connection::open(&path).unwrap();
    let mode: String = connection
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(version, 10);
    let violations: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn v8_database_adds_attempt_storage_without_rewriting_legacy_evidence() {
    let (root, mut store) = fixture();
    let path = root.path().join("fabric.sqlite3");
    let (run, workspace) = finalized_pair(&mut store, 91);
    store
        .create_worker_lease("legacy-owner", "private-authority", 999)
        .unwrap();
    store
        .record_cleanup_intent(&run, &workspace, "legacy-cleanup", 301)
        .unwrap();
    let artifacts = store.artifacts(&run.run_id).unwrap();
    drop(store);
    let db = Connection::open(&path).unwrap();
    remove_async_schema(&db);
    db.execute_batch("DROP TABLE IF EXISTS worker_attempts; PRAGMA user_version=8;")
        .unwrap();
    drop(db);

    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.artifacts(&run.run_id).unwrap(), artifacts);
    assert!(reopened.worker_attempts(&run.run_id).unwrap().is_empty());
    assert_eq!(
        reopened.worker_lease("legacy-owner").unwrap(),
        Some(("private-authority".into(), 999))
    );
    assert_eq!(
        reopened.cleanup_intent(&workspace.workspace_id).unwrap(),
        Some("legacy-cleanup".into())
    );
    assert_eq!(reopened.run(&run.run_id).unwrap(), Some(run));
    assert_eq!(
        reopened.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace)
    );
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        10,
        "C-02 must preserve earlier additive migrations"
    );
    let attempts: i64 = db
        .query_row("SELECT count(*) FROM worker_attempts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(attempts, 0, "legacy runs have no observed attempts");
    let violations: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

#[test]
fn v1_database_upgrades_without_losing_existing_run_ownership() {
    let (root, mut store) = fixture();
    let path = root.path().join("fabric.sqlite3");
    let (run, workspace) = pair(1);
    store.create_run_workspace(&run, &workspace).unwrap();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    remove_cleanup_schema(&connection);
    connection
        .execute_batch("DROP TRIGGER immutable_artifact_update; DROP TRIGGER immutable_artifact_delete; DROP TRIGGER immutable_manifest; DROP TABLE artifacts; DROP TRIGGER run_requires_batch_task; DROP TABLE batch_tasks; DROP TABLE batches; ALTER TABLE runs DROP COLUMN capture_metadata; ALTER TABLE runs DROP COLUMN exit_signal; PRAGMA user_version=1;")
        .unwrap();
    drop(connection);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.run(&run.run_id).unwrap(), Some(run));
    assert_eq!(
        reopened.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace)
    );
    drop(reopened);
    let connection = Connection::open(path).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 10);
}

#[test]
fn v2_database_upgrades_without_losing_run_ownership() {
    let (root, mut store) = fixture();
    let path = root.path().join("fabric.sqlite3");
    let (run, workspace) = pair(1);
    store.create_run_workspace(&run, &workspace).unwrap();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    remove_cleanup_schema(&connection);
    connection.execute_batch(
        "DROP TRIGGER immutable_artifact_update; DROP TRIGGER immutable_artifact_delete; DROP TRIGGER immutable_manifest; DROP TABLE artifacts; DROP TRIGGER run_requires_batch_task; DROP TABLE batch_tasks; DROP TABLE batches; ALTER TABLE runs DROP COLUMN exit_signal; PRAGMA user_version=2;",
    ).unwrap();
    drop(connection);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.run(&run.run_id).unwrap(), Some(run));
    assert_eq!(
        reopened.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace)
    );
}

#[test]
fn v4_database_upgrades_without_rewriting_previous_run_evidence() {
    let (root, mut store) = fixture();
    let (run, workspace) = pair(1);
    store.create_run_workspace(&run, &workspace).unwrap();
    drop(store);
    let path = root.path().join("fabric.sqlite3");
    let db = Connection::open(&path).unwrap();
    remove_cleanup_schema(&db);
    db.execute_batch(
        "DROP TRIGGER immutable_artifact_update; DROP TRIGGER immutable_artifact_delete;
        DROP TRIGGER immutable_manifest; DROP TABLE artifacts; PRAGMA user_version=4;",
    )
    .unwrap();
    drop(db);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.run(&run.run_id).unwrap(), Some(run.clone()));
    assert_eq!(
        reopened.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace)
    );
    assert!(reopened.artifacts(&run.run_id).unwrap().is_empty());
}

#[test]
fn batch_registration_and_results_are_atomic_and_one_shot() {
    use fabric_store::BatchTask;
    let (root, mut store) = fixture();
    let id = "batch-1".parse().unwrap();
    let tasks = vec![
        BatchTask {
            task_id: "first".into(),
            run_id: "run-1".parse().unwrap(),
            prompt_file: "/first".into(),
            result: None,
            error: None,
        },
        BatchTask {
            task_id: "first".into(),
            run_id: "run-2".parse().unwrap(),
            prompt_file: "/second".into(),
            result: None,
            error: None,
        },
    ];
    assert!(
        store
            .create_batch(&id, &prepared().state_id, &tasks)
            .is_err()
    );
    assert!(store.batch_tasks(&id).unwrap().is_empty());
    let mut tasks = tasks;
    tasks[1].task_id = "second".into();
    store
        .create_batch(&id, &prepared().state_id, &tasks)
        .unwrap();
    assert_eq!(store.batch_succeeded(&id).unwrap(), None);
    assert!(store.finish_batch(&id, true).is_err());
    let (mut run, workspace) = pair(1);
    run.batch_id = Some(id.clone());
    store.create_run_workspace(&run, &workspace).unwrap();
    let (mut unregistered, workspace) = pair(3);
    unregistered.batch_id = Some(id.clone());
    assert!(
        store
            .create_run_workspace(&unregistered, &workspace)
            .is_err()
    );
    store
        .finish_batch_task(&tasks[0].run_id, Ok("{\"exit_code\":0}"))
        .unwrap();
    assert!(
        store
            .finish_batch_task(&tasks[0].run_id, Err("overwrite"))
            .is_err()
    );
    store
        .finish_batch_task(&tasks[1].run_id, Err("prelaunch failure"))
        .unwrap();
    store.finish_batch(&id, false).unwrap();
    drop(store);
    let reopened = Store::open(root.path().join("fabric.sqlite3")).unwrap();
    assert_eq!(reopened.batch_succeeded(&id).unwrap(), Some(false));
    let saved = reopened.batch_tasks(&id).unwrap();
    assert_eq!(saved[0].result.as_deref(), Some("{\"exit_code\":0}"));
    assert_eq!(saved[1].error.as_deref(), Some("prelaunch failure"));
}

fn async_admission() -> (fabric_store::AsyncBatch, Vec<fabric_store::NewAsyncTask>) {
    let batch = fabric_store::AsyncBatch {
        batch_id: "async-batch".parse().unwrap(),
        prepared_state_id: prepared().state_id,
        request_digest: "a".repeat(64),
        concurrency: 1,
        options_json: "{}".into(),
        fake_binary: Some("/private/fake-harness".into()),
    };
    let tasks = (0..2)
        .map(|index| fabric_store::NewAsyncTask {
            task: fabric_store::BatchTask {
                task_id: format!("task-{index}"),
                run_id: format!("async-run-{index}").parse().unwrap(),
                prompt_file: "durable-input".into(),
                result: None,
                error: None,
            },
            prompt: format!("private prompt {index}").into_bytes(),
            harness: "fake".into(),
            plan_json: format!("frozen plan {index}"),
            consent_stamp: Some(format!("frozen consent {index}")),
        })
        .collect();
    (batch, tasks)
}

#[test]
fn async_admission_is_atomic_idempotent_and_pins_exact_prepared_state() {
    let (root, mut store) = fixture();
    let (batch, mut tasks) = async_admission();
    tasks[1].task.task_id = tasks[0].task.task_id.clone();
    assert!(
        store
            .accept_async_batch(&batch, Some("stable-key"), &tasks, 200)
            .is_err()
    );
    assert!(store.async_idempotency("stable-key").unwrap().is_none());
    assert!(store.batch_tasks(&batch.batch_id).unwrap().is_empty());
    tasks[1].task.task_id = "task-1".into();
    assert_eq!(
        store
            .accept_async_batch(&batch, Some("stable-key"), &tasks, 200)
            .unwrap(),
        batch.batch_id
    );
    assert_eq!(
        store
            .cache_retention(&batch.prepared_state_id)
            .unwrap()
            .as_deref(),
        Some("historical_reference")
    );
    let mut duplicate = batch.clone();
    duplicate.batch_id = "different-allocation".parse().unwrap();
    assert_eq!(
        store
            .accept_async_batch(&duplicate, Some("stable-key"), &tasks, 201)
            .unwrap(),
        batch.batch_id
    );
    assert!(store.async_batch(&duplicate.batch_id).unwrap().is_none());
    duplicate.request_digest = "b".repeat(64);
    assert!(matches!(
        store.accept_async_batch(&duplicate, Some("stable-key"), &tasks, 201),
        Err(StoreError::Conflict)
    ));
    drop(store);
    let store = Store::open(root.path().join("fabric.sqlite3")).unwrap();
    let accepted = store.async_task(&tasks[0].task.run_id).unwrap();
    assert_eq!(accepted.prompt, tasks[0].prompt);
    assert_eq!(accepted.plan_json, tasks[0].plan_json);
    assert_eq!(accepted.consent_stamp, tasks[0].consent_stamp);
    let events = store.async_events(&batch.batch_id, None, 0).unwrap();
    assert_eq!(events.len(), 2);
    assert!(events[0].cursor < events[1].cursor);
}

#[test]
fn async_claim_and_queued_cancellation_survive_restart_without_requeue() {
    let (root, mut store) = fixture();
    let (batch, tasks) = async_admission();
    store.accept_async_batch(&batch, None, &tasks, 200).unwrap();
    let first = &tasks[0].task.run_id;
    let second = &tasks[1].task.run_id;
    assert!(store.claim_async_task(first).unwrap());
    assert!(!store.claim_async_task(first).unwrap());
    assert!(
        !store
            .complete_async_task(first, RunState::Cancelled, Err("not queued"), true)
            .unwrap()
    );
    assert!(
        store
            .complete_async_task(
                second,
                RunState::Cancelled,
                Err("cancelled while queued"),
                true
            )
            .unwrap()
    );
    drop(store);
    let mut store = Store::open(root.path().join("fabric.sqlite3")).unwrap();
    assert!(!store.claim_async_task(first).unwrap());
    assert!(!store.claim_async_task(second).unwrap());
    assert!(
        store
            .complete_async_task(
                first,
                RunState::NeedsAttention,
                Err("ownership lost"),
                false
            )
            .unwrap()
    );
    assert!(
        !store
            .complete_async_task(first, RunState::Succeeded, Ok("{}"), false)
            .unwrap()
    );
    assert_eq!(store.batch_succeeded(&batch.batch_id).unwrap(), Some(false));
    let states = store.async_tasks(&batch.batch_id).unwrap();
    assert_eq!(states[0].state, RunState::NeedsAttention);
    assert_eq!(states[1].state, RunState::Cancelled);
}

#[test]
fn v9_migration_adds_queue_without_rewriting_completed_run_evidence() {
    let (root, mut store) = fixture();
    let (run, workspace) = finalized_pair(&mut store, 1);
    let artifacts = store.artifacts(&run.run_id).unwrap();
    drop(store);
    let path = root.path().join("fabric.sqlite3");
    let db = Connection::open(&path).unwrap();
    remove_async_schema(&db);
    db.pragma_update(None, "user_version", 9).unwrap();
    drop(db);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.run(&run.run_id).unwrap(), Some(run.clone()));
    assert_eq!(
        store.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace)
    );
    assert_eq!(store.artifacts(&run.run_id).unwrap(), artifacts);
    let (batch, tasks) = async_admission();
    store.accept_async_batch(&batch, None, &tasks, 300).unwrap();
    assert_eq!(store.async_tasks(&batch.batch_id).unwrap().len(), 2);
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        10
    );
}

#[test]
fn foreign_keys_and_provenance_reject_orphans_and_cross_project_pairs() {
    let (root, mut store) = fixture();
    let mut orphan = prepared();
    orphan.state_id = "orphan".parse().unwrap();
    orphan.project_id = "missing-project".parse().unwrap();
    orphan.preparation_key = "other-key".into();
    orphan.prepared_path = "/managed/prepared/orphan".into();
    assert!(matches!(
        store.create_prepared_state(&orphan),
        Err(StoreError::Sqlite(_))
    ));
    assert!(store.prepared_state(&orphan.state_id).unwrap().is_none());
    let mut other_project = project();
    other_project.project_id = "other-project".parse().unwrap();
    other_project.name = "other".into();
    store.create_project(&other_project).unwrap();
    let (mut run, workspace) = pair(1);
    run.project_id = other_project.project_id;
    assert!(store.create_run_workspace(&run, &workspace).is_err());
    run.project_id = project().project_id;
    run.base_commit_sha = "wrong-commit".into();
    assert!(store.create_run_workspace(&run, &workspace).is_err());
    assert!(store.run(&run.run_id).unwrap().is_none());
    assert!(store.workspace(&workspace.workspace_id).unwrap().is_none());
    let connection = Connection::open(root.path().join("fabric.sqlite3")).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    // Deferred ownership still rejects standalone workspaces at autocommit.
    assert!(connection.execute(
        "INSERT INTO workspaces VALUES ('orphan', 'missing-run', '/managed/orphan', 'state-1', 'ACTIVE', NULL, 0, NULL)", []
    ).is_err());
    assert!(
        connection
            .execute("DELETE FROM projects WHERE project_id = 'project-1'", [])
            .is_err()
    );
}

#[test]
fn failed_second_insert_or_deferred_ownership_check_rolls_back_both_rows() {
    let (_root, mut store) = fixture();
    let (first, first_workspace) = pair(1);
    store
        .create_run_workspace(&first, &first_workspace)
        .unwrap();
    let (second, mut second_workspace) = pair(2);
    second_workspace.managed_path = first_workspace.managed_path.clone();
    assert!(
        store
            .create_run_workspace(&second, &second_workspace)
            .is_err()
    );
    assert!(store.run(&second.run_id).unwrap().is_none());
    assert!(
        store
            .workspace(&second_workspace.workspace_id)
            .unwrap()
            .is_none()
    );
    second_workspace.managed_path = "/managed/workspaces/second".into();
    second_workspace.run_id = "wrong-owner".parse().unwrap();
    assert!(
        store
            .create_run_workspace(&second, &second_workspace)
            .is_err()
    );
    assert!(store.run(&second.run_id).unwrap().is_none());
    assert!(
        store
            .workspace(&second_workspace.workspace_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(store.run(&first.run_id).unwrap(), Some(first));
}

#[test]
fn only_ready_preparations_can_be_used_and_ready_metadata_is_immutable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fabric.sqlite3");
    let mut store = Store::open(&path).unwrap();
    store.create_project(&project()).unwrap();
    store.create_prepared_state(&prepared()).unwrap();
    let (run, mut workspace) = pair(1);
    assert!(store.create_run_workspace(&run, &workspace).is_err());
    assert!(
        store
            .finish_preparation(&prepared().state_id, PreparedStateStatus::Ready, None, 42)
            .is_err()
    );
    store
        .finish_preparation(
            &prepared().state_id,
            PreparedStateStatus::Ready,
            Some(120),
            42,
        )
        .unwrap();
    store
        .finish_preparation(
            &prepared().state_id,
            PreparedStateStatus::Ready,
            Some(120),
            42,
        )
        .unwrap();
    assert!(matches!(
        store.finish_preparation(
            &prepared().state_id,
            PreparedStateStatus::Ready,
            Some(121),
            42
        ),
        Err(StoreError::Conflict)
    ));
    assert!(matches!(
        store.finish_preparation(&prepared().state_id, PreparedStateStatus::Failed, None, 42),
        Err(StoreError::Domain(_))
    ));
    let connection = Connection::open(path).unwrap();
    assert!(
        connection
            .execute(
                "UPDATE prepared_states SET size_bytes = 43 WHERE state_id = 'state-1'",
                []
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "UPDATE prepared_states SET prepared_path = '/changed' WHERE state_id = 'state-1'",
                []
            )
            .is_err()
    );
    workspace.managed_path = prepared().prepared_path;
    assert!(store.create_run_workspace(&run, &workspace).is_err());
    assert!(store.run(&run.run_id).unwrap().is_none());
    let mut duplicate = prepared();
    duplicate.state_id = "state-2".parse().unwrap();
    duplicate.prepared_path = "/managed/prepared/state-2".into();
    assert!(store.create_prepared_state(&duplicate).is_err());
    duplicate.preparation_key = "other-key".into();
    store.create_prepared_state(&duplicate).unwrap();
    store
        .finish_preparation(&duplicate.state_id, PreparedStateStatus::Failed, None, 0)
        .unwrap();
    assert!(
        store
            .finish_preparation(
                &duplicate.state_id,
                PreparedStateStatus::Ready,
                Some(130),
                0
            )
            .is_err()
    );
}

#[test]
fn concurrent_creation_commits_one_owner_and_never_duplicates_ids() {
    let (root, store) = fixture();
    drop(store);
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = root.path().join("fabric.sqlite3");
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).unwrap();
                barrier.wait();
                let (run, workspace) = pair(1);
                store.create_run_workspace(&run, &workspace).is_ok()
            })
        })
        .collect();
    let successes = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|success| *success)
        .count();
    assert_eq!(successes, 1);
    let connection = Connection::open(root.path().join("fabric.sqlite3")).unwrap();
    for table in ["runs", "workspaces"] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}

#[test]
fn concurrent_first_open_migrates_once() {
    let root = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let path = root.path().join("fabric.sqlite3");
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Store::open(path).unwrap()
            })
        })
        .collect();
    for handle in handles {
        drop(handle.join().unwrap());
    }
    assert!(Store::open(root.path().join("fabric.sqlite3")).is_ok());
}

#[test]
fn concurrent_wal_transition_waits_for_reader_then_migrates_once() {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fabric.sqlite3");
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch(
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE preserved(value TEXT);
             INSERT INTO preserved VALUES ('before WAL');
             BEGIN; SELECT * FROM preserved;",
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(5));
    let (finished, receive) = mpsc::channel();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            let finished = finished.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let opened = Store::open(path);
                finished.send(()).unwrap();
                opened
            })
        })
        .collect();
    barrier.wait();
    let early = receive.recv_timeout(Duration::from_millis(100));
    reader.execute_batch("COMMIT").unwrap();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(early, Err(RecvTimeoutError::Timeout));
    for result in results {
        drop(result.unwrap());
    }
    let mode: String = Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(
        reader
            .query_row("SELECT value FROM preserved", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "before WAL"
    );
    assert!(Store::open(path).is_ok());
}

#[test]
fn concurrent_state_cas_rejects_stale_intent_and_exact_replay_is_idempotent() {
    let (root, mut store) = fixture();
    let (run, workspace) = pair(1);
    store.create_run_workspace(&run, &workspace).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [WorkspaceState::Retained, WorkspaceState::RecoveryRequired]
        .into_iter()
        .map(|next| {
            let path = root.path().join("fabric.sqlite3");
            let barrier = barrier.clone();
            let id = run.run_id.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(path).unwrap();
                barrier.wait();
                (next, store.transition_run_workspace(&id, change(next)))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|(_, r)| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|(_, r)| matches!(r, Err(StoreError::Conflict)))
            .count(),
        1
    );
    let next = results.into_iter().find(|(_, r)| r.is_ok()).unwrap().0;
    let before = store.run(&run.run_id).unwrap().unwrap();
    let mut replay = change(next);
    replay.updated_at = 999;
    store.transition_run_workspace(&run.run_id, replay).unwrap();
    assert_eq!(store.run(&run.run_id).unwrap(), Some(before));
    assert_eq!(
        store
            .workspace(&workspace.workspace_id)
            .unwrap()
            .unwrap()
            .state,
        next
    );
    let mut illegal = change(next);
    illegal.next_run = RunState::Succeeded;
    assert!(matches!(
        store.transition_run_workspace(&run.run_id, illegal),
        Err(StoreError::Domain(_))
    ));
}

#[test]
fn transition_rolls_back_run_when_workspace_write_fails() {
    let (root, mut store) = fixture();
    let (run, workspace) = pair(1);
    store.create_run_workspace(&run, &workspace).unwrap();
    let connection = Connection::open(root.path().join("fabric.sqlite3")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER inject_workspace_failure BEFORE UPDATE ON workspaces
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .unwrap();
    assert!(
        store
            .transition_run_workspace(&run.run_id, change(WorkspaceState::Retained))
            .is_err()
    );
    assert_eq!(store.run(&run.run_id).unwrap(), Some(run));
    assert_eq!(
        store.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace)
    );
}

#[test]
fn lifecycle_transitions_persist_timestamps_and_survive_restart() {
    let (root, mut store) = fixture();
    let (run, workspace) = pair(1);
    store.create_run_workspace(&run, &workspace).unwrap();
    let mut previous = RunState::Queued;
    for (next, time) in [
        (RunState::Preparing, 201),
        (RunState::Running, 202),
        (RunState::Finalizing, 203),
        (RunState::Succeeded, 204),
    ] {
        store
            .transition_run_workspace(
                &run.run_id,
                RunWorkspaceTransition {
                    expected_run: previous,
                    next_run: next,
                    expected_workspace: WorkspaceState::Active,
                    next_workspace: WorkspaceState::Active,
                    updated_at: time,
                },
            )
            .unwrap();
        previous = next;
    }
    drop(store);
    let store = Store::open(root.path().join("fabric.sqlite3")).unwrap();
    let persisted = store.run(&run.run_id).unwrap().unwrap();
    assert_eq!(persisted.state, RunState::Succeeded);
    assert_eq!(persisted.started_at, Some(202));
    assert_eq!(persisted.finished_at, Some(204));
    assert_eq!(persisted.updated_at, 204);
}

#[test]
fn migrations_fail_closed_and_roll_back_partial_schema() {
    let root = tempfile::tempdir().unwrap();
    let future = root.path().join("future.sqlite3");
    let connection = Connection::open(&future).unwrap();
    connection.pragma_update(None, "user_version", 999).unwrap();
    assert!(matches!(
        Store::open(&future),
        Err(StoreError::UnsupportedSchema(999))
    ));
    let broken = root.path().join("broken.sqlite3");
    let connection = Connection::open(&broken).unwrap();
    connection
        .execute_batch("CREATE TABLE prepared_states (sentinel TEXT);")
        .unwrap();
    assert!(Store::open(&broken).is_err());
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 0);
    let projects: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'projects'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(projects, 0);
}

#[test]
fn cleanup_intent_freezes_snapshots_and_replays_completion_without_deleting_ownership() {
    let (root, mut store) = fixture();
    let (run, workspace) = finalized_pair(&mut store, 2);
    let (other_run, other_workspace) = pair(1);
    store
        .create_run_workspace(&other_run, &other_workspace)
        .unwrap();
    assert_eq!(
        store.workspace_ids().unwrap(),
        vec![other_workspace.workspace_id, workspace.workspace_id.clone()]
    );
    assert_eq!(store.cleanup_intent(&workspace.workspace_id).unwrap(), None);
    assert!(
        store
            .finish_cleanup(&workspace.workspace_id, "verified")
            .is_err()
    );
    assert!(
        store
            .record_cleanup_intent(&run, &workspace, "", 300)
            .is_err()
    );
    let mut changed_run = run.clone();
    changed_run.capture.as_mut().unwrap().stdout.sha256 = "1".repeat(64);
    assert!(matches!(
        store.record_cleanup_intent(&changed_run, &workspace, "verified", 300),
        Err(StoreError::Conflict)
    ));
    let mut changed_workspace = workspace.clone();
    changed_workspace.usage_bytes += 1;
    assert!(matches!(
        store.record_cleanup_intent(&run, &changed_workspace, "verified", 300),
        Err(StoreError::Conflict)
    ));
    store
        .record_cleanup_intent(&run, &workspace, "verified", 300)
        .unwrap();
    store
        .record_cleanup_intent(&run, &workspace, "verified", 300)
        .unwrap();
    assert!(matches!(
        store.record_cleanup_intent(&run, &workspace, "different", 300),
        Err(StoreError::Conflict)
    ));
    let db = Connection::open(root.path().join("fabric.sqlite3")).unwrap();
    for assignment in [
        "workspace_id = 'changed'",
        "run_id = 'changed'",
        "managed_path = '/changed'",
        "state_id = 'changed'",
        "lease_expires_at = 999",
        "retention_reason = 'changed'",
        "usage_bytes = 0",
        "state = 'RETAINED'",
        "state = 'RECLAIMED'",
    ] {
        assert!(
            db.execute(
                &format!("UPDATE workspaces SET {assignment} WHERE workspace_id = 'workspace-2'"),
                [],
            )
            .is_err(),
            "{assignment}"
        );
    }
    for sql in [
        "UPDATE runs SET prompt_digest = 'changed' WHERE run_id = 'run-2'",
        "DELETE FROM runs WHERE run_id = 'run-2'",
        "DELETE FROM workspaces WHERE workspace_id = 'workspace-2'",
        "UPDATE cleanup_intents SET authority = 'changed'",
        "DELETE FROM cleanup_intents",
        "INSERT OR REPLACE INTO cleanup_intents VALUES ('workspace-2', 'changed')",
        "INSERT OR REPLACE INTO workspaces SELECT * FROM workspaces WHERE workspace_id = 'workspace-2'",
        "INSERT OR REPLACE INTO runs SELECT * FROM runs WHERE run_id = 'run-2'",
        "INSERT OR REPLACE INTO workspaces SELECT 'replacement', run_id, managed_path, state_id, state, lease_expires_at, usage_bytes, retention_reason FROM workspaces WHERE workspace_id = 'workspace-2'",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    assert!(
        store
            .finish_cleanup(&workspace.workspace_id, "different")
            .is_err()
    );
    drop(store);
    let mut store = Store::open(root.path().join("fabric.sqlite3")).unwrap();
    assert_eq!(
        store
            .cleanup_intent(&workspace.workspace_id)
            .unwrap()
            .as_deref(),
        Some("verified")
    );
    store
        .finish_cleanup(&workspace.workspace_id, "verified")
        .unwrap();
    store
        .finish_cleanup(&workspace.workspace_id, "verified")
        .unwrap();
    let mut reclaimed = workspace;
    reclaimed.state = WorkspaceState::Reclaimed;
    reclaimed.usage_bytes = 0;
    assert_eq!(
        store.workspace(&reclaimed.workspace_id).unwrap(),
        Some(reclaimed)
    );
    assert_eq!(store.run(&run.run_id).unwrap(), Some(run));
}

#[test]
fn cleanup_requires_terminal_capture_and_expiry_alone_does_not_authorize_it() {
    let (root, mut store) = fixture();
    let (run, workspace) = finalized_pair(&mut store, 1);
    let db = Connection::open(root.path().join("fabric.sqlite3")).unwrap();
    for (run_state, workspace_state, retention, lease) in [
        ("RUNNING", "CLEANUP_PENDING", None, None),
        ("NEEDS_ATTENTION", "CLEANUP_PENDING", None, None),
        ("SUCCEEDED", "ACTIVE", None, Some(0)),
        ("SUCCEEDED", "RETAINED", None, None),
        ("SUCCEEDED", "RECOVERY_REQUIRED", None, None),
        ("SUCCEEDED", "CLEANUP_PENDING", Some("keep"), None),
        ("SUCCEEDED", "CLEANUP_PENDING", None, Some(301)),
    ] {
        db.execute("UPDATE runs SET state = ?1", [run_state])
            .unwrap();
        db.execute(
            "UPDATE workspaces SET state = ?1, retention_reason = ?2, lease_expires_at = ?3",
            rusqlite::params![workspace_state, retention, lease],
        )
        .unwrap();
        let current_run = store.run(&run.run_id).unwrap().unwrap();
        let current_workspace = store.workspace(&workspace.workspace_id).unwrap().unwrap();
        assert!(
            store
                .record_cleanup_intent(&current_run, &current_workspace, "verified", 300)
                .is_err()
        );
        assert_eq!(store.cleanup_intent(&workspace.workspace_id).unwrap(), None);
    }
    db.execute("UPDATE runs SET state = 'FAILED'", []).unwrap();
    db.execute(
        "UPDATE workspaces SET state = 'CLEANUP_PENDING', retention_reason = NULL, lease_expires_at = 300",
        [],
    )
    .unwrap();
    let current_run = store.run(&run.run_id).unwrap().unwrap();
    let current_workspace = store.workspace(&workspace.workspace_id).unwrap().unwrap();
    store
        .record_cleanup_intent(&current_run, &current_workspace, "verified", 300)
        .unwrap();
    let (missing_run, missing_workspace) = pair(3);
    store
        .create_run_workspace(&missing_run, &missing_workspace)
        .unwrap();
    db.execute(
        "UPDATE runs SET state = 'CANCELLED' WHERE run_id = 'run-3'",
        [],
    )
    .unwrap();
    db.execute(
        "UPDATE workspaces SET state = 'CLEANUP_PENDING', lease_expires_at = NULL WHERE workspace_id = 'workspace-3'",
        [],
    )
    .unwrap();
    assert!(
        store
            .record_cleanup_intent(
                &store.run(&missing_run.run_id).unwrap().unwrap(),
                &store
                    .workspace(&missing_workspace.workspace_id)
                    .unwrap()
                    .unwrap(),
                "verified",
                300,
            )
            .is_err()
    );
    let (incomplete_run, incomplete_workspace) = finalized_pair(&mut store, 4);
    db.execute_batch(
        "DROP TRIGGER immutable_artifact_delete;
         DELETE FROM artifacts WHERE run_id = 'run-4' AND kind = 'untracked_archive';",
    )
    .unwrap();
    assert!(
        store
            .record_cleanup_intent(&incomplete_run, &incomplete_workspace, "verified", 300)
            .is_err()
    );
}

#[test]
fn read_only_inspection_neither_creates_migrates_nor_writes_v5_v6_or_v7() {
    let (root, mut store) = fixture();
    let path = root.path().join("fabric.sqlite3");
    let (run, workspace) = finalized_pair(&mut store, 1);
    let wal = path.with_extension("sqlite3-wal");
    let shm = path.with_extension("sqlite3-shm");
    let wal_before = std::fs::read(&wal).unwrap();
    let readonly = Store::open_existing_read_only(&path).unwrap();
    assert_eq!(readonly.run(&run.run_id).unwrap(), Some(run.clone()));
    assert_eq!(
        readonly.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace.clone())
    );
    drop(readonly);
    assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
    assert!(shm.is_file());
    drop(store);
    let escaped_path = root.path().join("inspection ?#%.sqlite3");
    std::fs::rename(&path, &escaped_path).unwrap();
    let path = escaped_path;
    for version in [10, 9, 8, 7, 6, 5] {
        let db = Connection::open(&path).unwrap();
        if version == 9 {
            remove_async_schema(&db);
            db.pragma_update(None, "user_version", 9).unwrap();
        }
        if version == 8 {
            db.execute_batch("DROP TABLE worker_attempts; PRAGMA user_version=8;")
                .unwrap();
        }
        if version == 7 {
            db.execute_batch("DROP TABLE worker_leases; PRAGMA user_version=7;")
                .unwrap();
        }
        if version == 6 {
            db.execute_batch("DROP TABLE storage_reservations; DROP TABLE cache_evictions; PRAGMA user_version=6;").unwrap();
        }
        if version == 5 {
            remove_cleanup_schema(&db);
            db.pragma_update(None, "user_version", 5).unwrap();
        }
        db.pragma_update(None, "journal_mode", "DELETE").unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();
        let readonly = Store::open_existing_read_only(&path).unwrap();
        assert_eq!(readonly.run(&run.run_id).unwrap(), Some(run.clone()));
        assert!(readonly.worker_attempts(&run.run_id).unwrap().is_empty());
        assert_eq!(
            readonly.workspace(&workspace.workspace_id).unwrap(),
            Some(workspace.clone())
        );
        assert_eq!(
            readonly.cleanup_intent(&workspace.workspace_id).unwrap(),
            None
        );
        assert!(
            readonly
                .create_project(&Project {
                    project_id: "other-project".parse().unwrap(),
                    name: "other-project".into(),
                    ..project()
                })
                .is_err()
        );
        drop(readonly);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("sqlite3-wal").exists());
        assert!(!path.with_extension("sqlite3-shm").exists());
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            version
        );
    }
    let missing = root.path().join("missing.sqlite3");
    assert!(Store::open_existing_read_only(&missing).is_err());
    assert!(!missing.exists());
    let migrated = Store::open(&path).unwrap();
    assert_eq!(migrated.run(&run.run_id).unwrap(), Some(run));
    assert_eq!(
        migrated.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace)
    );
}

#[test]
fn read_only_inspection_refuses_symlink_hardlink_and_public_database_sidecars() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    for suffix in ["", "-wal", "-shm"] {
        for kind in ["symlink", "hardlink", "public"] {
            let (root, mut store) = fixture();
            finalized_pair(&mut store, 1);
            let path = root.path().join("fabric.sqlite3");
            let component = root.path().join(format!("fabric.sqlite3{suffix}"));
            let saved = root.path().join("saved");
            let sentinel = root.path().join("outside");
            std::fs::write(&sentinel, b"outside sentinel").unwrap();
            match kind {
                "symlink" => {
                    std::fs::rename(&component, &saved).unwrap();
                    symlink(&sentinel, &component).unwrap();
                }
                "hardlink" => std::fs::hard_link(&component, &saved).unwrap(),
                "public" => {
                    std::fs::set_permissions(&component, std::fs::Permissions::from_mode(0o644))
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                Store::open_existing_read_only(&path).is_err(),
                "accepted {kind} SQLite component {suffix}"
            );
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside sentinel");
            match kind {
                "symlink" => {
                    std::fs::remove_file(&component).unwrap();
                    std::fs::rename(&saved, &component).unwrap();
                }
                "hardlink" => std::fs::remove_file(&saved).unwrap(),
                "public" => {
                    std::fs::set_permissions(&component, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
                _ => unreachable!(),
            }
        }
    }
}

fn attempt(run: &Run, id: &str) -> WorkerAttempt {
    WorkerAttempt {
        attempt_id: id.into(),
        run_id: run.run_id.clone(),
        harness_requested: "future-harness".parse().unwrap(),
        harness_resolved: "future-harness".parse().unwrap(),
        harness_version: "future-cli 2.1+exact".into(),
        model_requested: None,
        model_resolved: None,
        profile_digest: None,
        backend_id: "local/trusted-local".into(),
        event_cursor: 0,
        created_at: 200,
        started_at: None,
        finished_at: None,
        exit_code: None,
        exit_signal: None,
        termination_reason: None,
    }
}

#[test]
fn worker_attempts_preserve_durable_provenance_progress_and_terminal_evidence() {
    let (root, mut store) = fixture();
    let path = root.path().join("fabric.sqlite3");
    let (run, workspace) = pair(301);
    let (other_run, other_workspace) = pair(302);
    store.create_run_workspace(&run, &workspace).unwrap();
    store
        .create_run_workspace(&other_run, &other_workspace)
        .unwrap();
    store
        .create_worker_lease("owner", "original-authority", 999)
        .unwrap();
    assert!(store.worker_attempts(&run.run_id).unwrap().is_empty());
    let mut observed = attempt(&run, "attempt-b");
    observed.model_requested = Some("requested-model".into());
    observed.model_resolved = Some("observed-model".into());
    observed.profile_digest = Some("nonsecret-profile-digest".into());
    store.record_worker_attempt(&observed).unwrap();
    store.record_worker_attempt(&observed).unwrap();
    observed.started_at = Some(201);
    observed.event_cursor = 5;
    store.record_worker_attempt(&observed).unwrap();
    let mutations: &[fn(&mut WorkerAttempt)] = &[
        |value| value.run_id = "run-302".parse().unwrap(),
        |value| value.harness_requested = "another-harness".parse().unwrap(),
        |value| value.harness_resolved = "another-harness".parse().unwrap(),
        |value| value.harness_version = "different-version".into(),
        |value| value.model_requested = None,
        |value| value.model_resolved = None,
        |value| value.profile_digest = None,
        |value| value.backend_id = "another-backend".into(),
        |value| value.created_at += 1,
        |value| value.started_at = None,
        |value| value.started_at = Some(200),
        |value| value.started_at = Some(202),
        |value| value.event_cursor = 4,
        |value| value.event_cursor = u64::MAX,
        |value| value.finished_at = Some(200),
        |value| value.exit_code = Some(0),
    ];
    for mutate in mutations {
        let mut changed = observed.clone();
        mutate(&mut changed);
        assert!(
            store.record_worker_attempt(&changed).is_err(),
            "{changed:?}"
        );
        assert_eq!(
            store.worker_attempts(&run.run_id).unwrap(),
            [observed.clone()]
        );
    }
    observed.finished_at = Some(203);
    observed.exit_code = Some(0);
    observed.event_cursor = 9;
    store.record_worker_attempt(&observed).unwrap();
    store.record_worker_attempt(&observed).unwrap();
    let terminal_mutations: &[fn(&mut WorkerAttempt)] = &[
        |value| value.event_cursor += 1,
        |value| value.finished_at = Some(204),
        |value| value.finished_at = None,
        |value| value.exit_code = Some(1),
        |value| value.exit_code = None,
        |value| {
            value.exit_code = None;
            value.exit_signal = Some(15);
        },
        |value| value.termination_reason = Some("invented-failure".into()),
    ];
    for mutate in terminal_mutations {
        let mut changed = observed.clone();
        mutate(&mut changed);
        assert!(
            store.record_worker_attempt(&changed).is_err(),
            "{changed:?}"
        );
    }
    let mut not_launched = attempt(&run, "attempt-a");
    not_launched.finished_at = Some(202);
    not_launched.termination_reason = Some("launch_failed".into());
    store.record_worker_attempt(&not_launched).unwrap();
    let expected = vec![not_launched, observed];
    assert_eq!(store.worker_attempts(&run.run_id).unwrap(), expected);
    assert!(store.worker_attempts(&other_run.run_id).unwrap().is_empty());
    assert_eq!(store.run(&run.run_id).unwrap(), Some(run.clone()));
    assert_eq!(
        store.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace.clone())
    );
    assert_eq!(
        store.worker_lease("owner").unwrap(),
        Some(("original-authority".into(), 999))
    );
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.worker_attempts(&run.run_id).unwrap(), expected);
    assert_eq!(reopened.run(&run.run_id).unwrap(), Some(run));
    assert_eq!(
        reopened.workspace(&workspace.workspace_id).unwrap(),
        Some(workspace)
    );
    let db = Connection::open(&path).unwrap();
    assert!(db.execute("DELETE FROM worker_attempts", []).is_err());
    assert!(
        db.execute("UPDATE worker_attempts SET event_cursor=0", [])
            .is_err()
    );
    assert_eq!(
        reopened
            .worker_attempts(&"run-301".parse().unwrap())
            .unwrap(),
        expected
    );
}

#[test]
fn worker_attempts_reject_orphans_invalid_times_and_unobserved_terminal_values() {
    let (_root, mut store) = fixture();
    let (run, workspace) = pair(303);
    store.create_run_workspace(&run, &workspace).unwrap();
    let initial = attempt(&run, "attempt");
    let mutations: &[fn(&mut WorkerAttempt)] = &[
        |value| value.attempt_id = "../outside".into(),
        |value| value.run_id = "missing-run".parse().unwrap(),
        |value| value.harness_version.clear(),
        |value| value.harness_version = "version\ninjected".into(),
        |value| value.backend_id.clear(),
        |value| value.event_cursor = u64::MAX,
        |value| value.created_at = -1,
        |value| value.created_at = 199,
        |value| value.started_at = Some(199),
        |value| value.finished_at = Some(201),
        |value| {
            value.started_at = Some(202);
            value.finished_at = Some(201);
            value.exit_code = Some(0);
        },
        |value| value.exit_code = Some(0),
        |value| value.exit_signal = Some(15),
        |value| value.termination_reason = Some("not-finished".into()),
        |value| {
            value.finished_at = Some(202);
            value.exit_code = Some(0);
        },
        |value| {
            value.started_at = Some(201);
            value.finished_at = Some(202);
            value.exit_signal = Some(0);
        },
        |value| {
            value.started_at = Some(201);
            value.finished_at = Some(202);
            value.exit_code = Some(0);
            value.exit_signal = Some(15);
        },
    ];
    for mutate in mutations {
        let mut changed = initial.clone();
        mutate(&mut changed);
        assert!(
            store.record_worker_attempt(&changed).is_err(),
            "{changed:?}"
        );
        assert!(store.worker_attempts(&run.run_id).unwrap().is_empty());
    }
    store.record_worker_attempt(&initial).unwrap();
    let mut interrupted = initial;
    interrupted.started_at = Some(201);
    interrupted.finished_at = Some(202);
    interrupted.termination_reason = Some("supervisor_lost".into());
    store.record_worker_attempt(&interrupted).unwrap();
    let saved = store.worker_attempts(&run.run_id).unwrap();
    assert_eq!(saved, [interrupted]);
    assert_eq!(saved[0].exit_code, None);
    assert_eq!(saved[0].exit_signal, None);
}

#[test]
fn worker_attempts_read_only_consumers_see_committed_live_wal_without_writes() {
    let (root, mut writer) = fixture();
    let path = root.path().join("fabric.sqlite3");
    let (run, workspace) = pair(304);
    writer.create_run_workspace(&run, &workspace).unwrap();
    let observed = attempt(&run, "attempt-live");
    writer.record_worker_attempt(&observed).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let base = std::fs::read(&path).unwrap();
    let mut progressed = observed;
    progressed.started_at = Some(201);
    progressed.event_cursor = 17;
    writer.record_worker_attempt(&progressed).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), base);
    let wal = path.with_extension("sqlite3-wal");
    let wal_before = std::fs::read(&wal).unwrap();
    assert!(!wal_before.is_empty());
    let reader = Store::open_existing_read_only(&path).unwrap();
    assert_eq!(
        reader.worker_attempts(&run.run_id).unwrap(),
        [progressed.clone()]
    );
    assert!(reader.record_worker_attempt(&progressed).is_err());
    drop(reader);
    assert_eq!(std::fs::read(&path).unwrap(), base);
    assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
}
