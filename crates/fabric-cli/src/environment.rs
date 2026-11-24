use clap::{Args, Subcommand};
use fabric_domain::{EnvironmentId, EnvironmentSelection, FabricError};
use std::path::PathBuf;

#[derive(Args)]
pub struct SelectionArgs {
    /// Explicit durable environment; never inferred from a project or run.
    #[arg(long = "environment", requires = "environment_owner")]
    pub environment_id: Option<EnvironmentId>,
    #[arg(long, requires = "environment_id")]
    pub environment_owner: Option<String>,
    /// Require a target that also negotiates the optional PTY capability.
    #[arg(long, requires = "environment_id")]
    pub interactive_exec_pty: bool,
}
impl SelectionArgs {
    pub fn selection(&self) -> Option<EnvironmentSelection> {
        self.environment_id.as_ref().map(|id| EnvironmentSelection {
            environment_id: id.clone(),
            owner: self.environment_owner.clone().expect("clap requires owner"),
            interactive_exec_pty: self.interactive_exec_pty,
        })
    }
}

#[derive(Args)]
pub struct Options {
    #[command(subcommand)]
    operation: Operation,
}
#[derive(Subcommand)]
enum Operation {
    /// Create once or verify the existing immutable owner/target binding.
    Ensure(Identity),
    /// Reconcile independently verified resource facts, including unknown state.
    Inspect(Identity),
    /// No host.local start primitive exists; returns typed unsupported-capability.
    Start(Identity),
    /// No host.local stop primitive exists; returns typed unsupported-capability.
    Stop(Identity),
    /// Permanently delete the positively owned home after inactive-run checks.
    Destroy {
        #[command(flatten)]
        identity: Identity,
        /// Must equal the exact environment ID. No implied confirmation.
        #[arg(long)]
        confirm: String,
    },
}
#[derive(Args)]
struct Identity {
    environment_id: EnvironmentId,
    #[arg(long)]
    owner: String,
    #[arg(long, default_value = "local")]
    target: String,
    #[arg(long)]
    interactive_exec_pty: bool,
    #[arg(long)]
    json: bool,
}
impl Identity {
    fn selection(&self) -> EnvironmentSelection {
        EnvironmentSelection {
            environment_id: self.environment_id.clone(),
            owner: self.owner.clone(),
            interactive_exec_pty: self.interactive_exec_pty,
        }
    }
}
impl Options {
    pub fn json(&self) -> bool {
        match &self.operation {
            Operation::Ensure(id)
            | Operation::Inspect(id)
            | Operation::Start(id)
            | Operation::Stop(id) => id.json,
            Operation::Destroy { identity, .. } => identity.json,
        }
    }
}
pub fn run(root: Option<PathBuf>, options: Options) -> Result<(), FabricError> {
    let root = root.map_or_else(fabric_project::default_root, Ok)?;
    let json = options.json();
    let report = match options.operation {
        Operation::Ensure(id) => {
            fabric_core::environment::ensure(&root, &id.selection(), &id.target)
        }
        Operation::Inspect(id) => {
            fabric_core::environment::inspect(&root, &id.selection(), &id.target)
        }
        Operation::Start(id) => {
            fabric_core::environment::start_stop(&root, &id.selection(), &id.target, true)
        }
        Operation::Stop(id) => {
            fabric_core::environment::start_stop(&root, &id.selection(), &id.target, false)
        }
        Operation::Destroy { identity, confirm } => fabric_core::environment::destroy(
            &root,
            &identity.selection(),
            &identity.target,
            &confirm,
        ),
    }?;
    super::write_inspection(&report, json)
}
