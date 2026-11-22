//! I/O-free IDs, lifecycle transitions, policy validation, and shared errors.

use std::fmt;

use serde::{Deserialize, Serialize};

mod capture;
mod harness;
mod ids;
mod policy;
mod profile;
mod state;
mod worker;

pub use capture::{AgentEvent, CaptureMetadata, CapturedFile, EventCounts};
pub use harness::{
    Capability, CapabilityReason, CapabilityStatus, CapabilitySupport, HarnessCapabilityManifest,
    HarnessId, HarnessPlan, HarnessRequest, HarnessVersion,
};
pub use ids::*;
pub use policy::{RetentionMode, RetentionPolicy, StoragePolicy};
pub use profile::{
    AgentProfile, CredentialConnectionRef, ModelPolicy, NativeConfigRef, ProfileEvidence,
};
pub use state::{PreparedStateStatus, RunState, WorkspaceState};
pub use worker::{ExecutionContext, SourceProvenance, SourceRef, WorkerLimits, WorkerTask};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidArguments,
    Output,
    InvalidId,
    InvalidTransition,
    InvalidPolicy,
    InvalidRecipe,
    Source,
    Storage,
    NotFound,
    UnknownHarness,
    UnsupportedCapability,
    UnsupportedVersion,
    ModelUnsupported,
    ModelUnverified,
    UnsupportedPlacement,
    InvalidProfile,
    AuthorizationRequired,
}

#[derive(Debug, Serialize, Deserialize)]
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
