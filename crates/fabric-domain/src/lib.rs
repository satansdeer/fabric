//! I/O-free IDs, lifecycle transitions, policy validation, and shared errors.

use std::fmt;

use serde::Serialize;

mod ids;
mod policy;
mod state;

pub use ids::*;
pub use policy::{RetentionMode, RetentionPolicy, StoragePolicy};
pub use state::{PreparedStateStatus, RunState, WorkspaceState};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidArguments,
    Output,
    InvalidId,
    InvalidTransition,
    InvalidPolicy,
}

#[derive(Debug, Serialize)]
pub struct FabricError {
    pub kind: ErrorKind,
    pub message: String,
}

impl fmt::Display for FabricError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FabricError {}
