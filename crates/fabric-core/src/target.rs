//! Target operations execute here against Core-owned inputs, never plugin-provided paths or PIDs.
use fabric_domain::{FabricError, RunId, WorkspaceId};
use fabric_runner::{
    CancellationToken,
    target::{Action, ForkMode, Target, TargetPin},
};
use fabric_store::{PreparedState, Store};
use fabric_workspace::{ForkStrategy, LocalWorkspaceBackend, PreparedFork, WorkspaceBackend};
use std::path::Path;

pub(crate) struct Environment<'a> {
    pub root: &'a Path,
    pub run: &'a RunId,
    pub target: &'a Target,
}
impl WorkspaceBackend for Environment<'_> {
    fn fork(
        &self,
        state: &PreparedState,
        id: &WorkspaceId,
        limit: u64,
    ) -> Result<PreparedFork, FabricError> {
        self.target.drive(
            "prepare",
            self.run.as_str(),
            &[
                Action::Fork {
                    strategy: ForkMode::CloneOrCopy,
                },
                Action::Fork {
                    strategy: ForkMode::Copy,
                },
            ],
            serde_json::json!({"size_bytes":state.size_bytes,"copy_limit":limit}),
            |action| {
                let strategy = match action {
                    Action::Fork {
                        strategy: ForkMode::CloneOrCopy,
                    } => ForkStrategy::CloneOrCopy,
                    Action::Fork {
                        strategy: ForkMode::Copy,
                    } => ForkStrategy::Copy,
                    _ => unreachable!("validated offered action"),
                };
                let fork =
                    LocalWorkspaceBackend::new(self.root, strategy)?.fork(state, id, limit)?;
                let evidence = serde_json::json!({"size_bytes":fork.size_bytes,
                "cloned_files":fork.cloned_files,"copied_files":fork.copied_files});
                Ok((fork, evidence))
            },
        )
    }
}

/// Historical non-target workspaces retain their existing Core-only recovery rules.
/// New executions always have an immutable target binding before a runnable fork exists.
pub(crate) fn drive<T>(
    store: &Store,
    run: &RunId,
    operation: &str,
    action: Action,
    context: serde_json::Value,
    execute: impl FnOnce() -> Result<(T, serde_json::Value), FabricError>,
) -> Result<T, FabricError> {
    if let Some(json) = store
        .run_target(run)
        .map_err(fabric_runner::execution_error)?
    {
        let pin: TargetPin = serde_json::from_str(&json)
            .map_err(|_| fabric_runner::execution_error("Invalid target binding."))?;
        let target = pin.open(&CancellationToken::default())?;
        let value = target.drive(operation, run.as_str(), &[action], context, |_| execute())?;
        target.finish()?;
        Ok(value)
    } else {
        execute().map(|(value, _)| value)
    }
}

#[cfg(test)]
pub(crate) fn reference_pin() -> TargetPin {
    let executable = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fabric-local-target");
    TargetPin::from_manifest(
        serde_json::from_value(serde_json::json!({
            "id":"local","version":"0.1.0","kind":"target","protocol":{"min":1,"max":1},
            "executable":executable,"args":[],"capabilities":["target.execute"]
        }))
        .unwrap(),
        "local",
    )
    .unwrap()
}
