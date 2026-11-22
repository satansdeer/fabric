//! Harness-independent worker intent; native configuration remains harness-owned.
use serde::{Deserialize, Serialize};

use crate::{CredentialConnectionRef, HarnessId, NativeConfigRef, PreparedStateId};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerTask {
    pub task_id: String,
    pub harness_id: HarnessId,
    pub prompt: String,
    #[serde(default)]
    pub execution_context: Option<ExecutionContext>,
    #[serde(default)]
    pub source_ref: Option<SourceRef>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub limits: Option<WorkerLimits>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionContext {
    Native {},
    Explicit {
        harness_version: String,
        #[serde(default)]
        native_config_refs: Vec<NativeConfigRef>,
        #[serde(default)]
        connection_refs: Vec<CredentialConnectionRef>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SourceRef {
    CurrentHead {},
    Prepared { state_id: String },
    Commit { sha: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceProvenance {
    pub source_ref: SourceRef,
    pub prepared_state_id: PreparedStateId,
    pub base_commit_sha: String,
    pub checkout_dirty: Option<bool>,
    pub dirty_changes_excluded: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerLimits {
    pub timeout_seconds: Option<u64>,
    pub copy_limit_bytes: Option<u64>,
    pub max_log_bytes: Option<u64>,
    pub max_artifact_bytes: Option<u64>,
    pub max_cost_usd: Option<f64>,
}
