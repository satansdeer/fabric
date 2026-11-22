use std::sync::{Arc, Barrier};

use fabric_domain::{PreparedStateStatus, RunState, WorkspaceState};
use fabric_store::{
    PreparedState, Project, Run, RunWorkspaceTransition, Store, StoreError, Workspace,
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
        batch_id: Some("batch-1".parse().unwrap()),
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
    assert_eq!(version, 2);
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
fn v1_database_upgrades_without_losing_existing_run_ownership() {
    let (root, mut store) = fixture();
    let path = root.path().join("fabric.sqlite3");
    let (run, workspace) = pair(1);
    store.create_run_workspace(&run, &workspace).unwrap();
    drop(store);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch("ALTER TABLE runs DROP COLUMN capture_metadata; PRAGMA user_version=1;")
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
    assert_eq!(version, 2);
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
