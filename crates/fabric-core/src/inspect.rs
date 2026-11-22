//! Persisted metadata only: no migration, reconciliation, probes or artifact reads.
use std::path::Path;

use fabric_domain::{ArtifactManifestId, ErrorKind, FabricError, RunId};
use fabric_store::{Artifact, PreparedState, Project, Run, Store, Workspace};
use serde::Serialize;

#[derive(Serialize)]
pub struct RunReport {
    pub run: Run,
    pub workspace: Workspace,
    pub prepared_state: PreparedState,
}

#[derive(Serialize)]
pub struct ArtifactReport {
    pub run_id: RunId,
    pub artifact_manifest_id: Option<ArtifactManifestId>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Serialize)]
pub struct ProjectReport {
    #[serde(flatten)]
    pub project: Project,
    pub source_commit_sha: String,
    pub prepared_states: Vec<PreparedState>,
}

pub fn list(root: &Path) -> Result<Vec<Run>, FabricError> {
    let store = open(root)?;
    store
        .run_ids()
        .map_err(storage_error)?
        .into_iter()
        .map(|id| required_run(&store, &id))
        .collect()
}

pub fn show(root: &Path, id: &RunId) -> Result<RunReport, FabricError> {
    let store = open(root)?;
    let run = required_run(&store, id)?;
    let workspace = store
        .workspace(&run.workspace_id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Run has no persisted workspace record."))?;
    let prepared_state = store
        .prepared_state(&run.prepared_state_id)
        .map_err(storage_error)?
        .ok_or_else(|| storage_error("Run has no persisted prepared-state record."))?;
    Ok(RunReport {
        run,
        workspace,
        prepared_state,
    })
}

pub fn artifacts(root: &Path, id: &RunId) -> Result<ArtifactReport, FabricError> {
    let store = open(root)?;
    let run = required_run(&store, id)?;
    Ok(ArtifactReport {
        artifacts: store.artifacts(id).map_err(storage_error)?,
        run_id: run.run_id,
        artifact_manifest_id: run.artifact_manifest_id,
    })
}

pub fn project(root: &Path, name: &str) -> Result<ProjectReport, FabricError> {
    let not_found = || FabricError {
        kind: ErrorKind::NotFound,
        message: format!("Unknown project {name:?}; register it with `fabric project add`."),
    };
    if !root.join("fabric.db").exists() {
        return Err(not_found());
    }
    let store = open(root)?;
    let project = store
        .project_by_name(name)
        .map_err(storage_error)?
        .ok_or_else(not_found)?;
    let prepared_states = store
        .prepared_states()
        .map_err(storage_error)?
        .into_iter()
        .filter(|state| state.project_id == project.project_id)
        .collect();
    Ok(ProjectReport {
        source_commit_sha: project.default_ref.clone(),
        project,
        prepared_states,
    })
}

fn required_run(store: &Store, id: &RunId) -> Result<Run, FabricError> {
    store
        .run(id)
        .map_err(storage_error)?
        .ok_or_else(|| FabricError {
            kind: ErrorKind::NotFound,
            message: format!("Unknown run {id} in this Fabric root; use `fabric runs list`."),
        })
}

fn open(root: &Path) -> Result<Store, FabricError> {
    super::workspace::private_directory(root)?;
    Store::open_existing_read_only(root.join("fabric.db")).map_err(storage_error)
}

fn storage_error(error: impl std::fmt::Display) -> FabricError {
    FabricError {
        kind: ErrorKind::Storage,
        message: error.to_string(),
    }
}
