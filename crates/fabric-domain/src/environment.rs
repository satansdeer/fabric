//! Environment identity is independent of disposable run/workspace identity.
use crate::EnvironmentId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSelection {
    pub environment_id: EnvironmentId,
    /// Local owner namespace, authenticated together with the effective UID.
    pub owner: String,
    #[serde(default)]
    pub interactive_exec_pty: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentCapabilities {
    pub version: u32,
    pub persistent_home: bool,
    pub interactive_exec_pty: bool,
    pub infrastructure_diagnostics: bool,
    pub operations: Vec<String>,
}

impl EnvironmentCapabilities {
    pub fn local() -> Self {
        Self {
            version: 1,
            persistent_home: true,
            interactive_exec_pty: false,
            infrastructure_diagnostics: false,
            operations: ["ensure", "inspect", "destroy", "workspace"]
                .map(String::from)
                .into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentDesired {
    Available,
    Destroyed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentRuntime {
    Available,
    Absent,
    Unknown,
}
