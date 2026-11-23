//! Target operations execute here against Core-owned inputs, never plugin-provided paths or PIDs.
use fabric_domain::{FabricError, RunId};
use fabric_runner::{
    CancellationToken,
    target::{Action, Committed, TargetPin},
};
use fabric_store::Store;

/// Historical non-target workspaces retain their existing Core-only recovery rules.
/// New executions always have an immutable target binding before a runnable fork exists.
pub(crate) fn drive<T>(
    store: &mut Store,
    run: &RunId,
    operation: &str,
    action: Action,
    context: serde_json::Value,
    execute: impl FnOnce(&mut Store) -> Result<(T, serde_json::Value), FabricError>,
) -> Result<Committed<T>, FabricError> {
    if let Some(json) = store
        .run_target(run)
        .map_err(fabric_runner::execution_error)?
    {
        let pin: TargetPin = serde_json::from_str(&json)
            .map_err(|_| fabric_runner::execution_error("Invalid target binding."))?;
        let target = pin.open(&CancellationToken::default())?;
        let mut committed = target.drive(operation, run.as_str(), &[action], context, |_| {
            execute(store)
        })?;
        committed.failure = committed.failure.or_else(|| target.finish().err());
        if committed.failure.is_some() {
            store
                .record_target_failure(run)
                .map_err(fabric_runner::execution_error)?;
        }
        Ok(committed)
    } else {
        execute(store).map(|(value, _)| Committed {
            value,
            failure: None,
        })
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
