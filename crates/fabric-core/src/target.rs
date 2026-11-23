//! Target operations execute here against Core-owned inputs, never plugin-provided paths or PIDs.
use std::path::Path;
use fabric_domain::{FabricError, RunId, WorkspaceId};
use fabric_runner::{CancellationToken, target::{Target, TargetPin}};
use fabric_store::{PreparedState, Store};
use fabric_workspace::{WorkspaceBackend, PreparedFork, LocalWorkspaceBackend, ForkStrategy};

pub(crate) struct Environment<'a> {
    pub root: &'a Path,
    pub run: &'a RunId,
    pub target: &'a Target,
}
impl WorkspaceBackend for Environment<'_> {
    fn fork(&self, state: &PreparedState, id: &WorkspaceId, limit: u64) -> Result<PreparedFork, FabricError> {
        self.target.operation("prepare", self.run.as_str())?;
        LocalWorkspaceBackend::new(self.root, ForkStrategy::CloneOrCopy)?.fork(state, id, limit)
    }
}

/// Historical non-target workspaces retain their existing Core-only recovery rules.
/// New executions always have an immutable target binding before a runnable fork exists.
pub(crate) fn operation(store: &Store, run: &RunId, operation: &str) -> Result<(), FabricError> {
    if let Some(json) = store.run_target(run).map_err(fabric_runner::execution_error)? {
        let pin: TargetPin = serde_json::from_str(&json).map_err(|_| fabric_runner::execution_error("Invalid target binding."))?;
        let target = pin.open(&CancellationToken::default())?;
        target.operation(operation, run.as_str())?;
        target.finish()?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn reference_pin() -> TargetPin {
    let executable = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("fabric-local-target");
    TargetPin::from_manifest(serde_json::from_value(serde_json::json!({
        "id":"local","version":"0.1.0","kind":"target","protocol":{"min":1,"max":1},
        "executable":executable,"args":[],"capabilities":["target.execute"]
    })).unwrap(), "local").unwrap()
}
