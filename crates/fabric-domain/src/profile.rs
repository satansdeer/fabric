use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CapabilityReason, ErrorKind, FabricError, HarnessId};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub harness_id: HarnessId,
    pub harness_version: String,
    #[serde(default)]
    pub native_config_refs: Vec<NativeConfigRef>,
    #[serde(default)]
    pub model_policy: ModelPolicy,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
    #[serde(default)]
    pub connection_refs: Vec<CredentialConnectionRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfigRef {
    pub kind: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConnectionRef {
    pub kind: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ModelPolicy {
    NativeDefault {},
    Exact { model: String },
}

impl Default for ModelPolicy {
    fn default() -> Self {
        Self::NativeDefault {}
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProfileEvidence {
    pub digest: String,
    pub native_config_fingerprint: String,
    pub portable: bool,
    pub diagnostics: Vec<CapabilityReason>,
}

impl AgentProfile {
    pub fn validate(&self) -> Result<(), FabricError> {
        let valid = |value: &str| {
            !value.trim().is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
        };
        let invalid = || {
            FabricError {
            kind: ErrorKind::InvalidProfile,
            message: "Profile requires bounded nonempty references and absolute UTF-8 config paths without control characters (maximum 32 entries per list, 1024 bytes per reference).".into(),
        }
        };
        if !valid(&self.harness_version)
            || [&self.skills, &self.plugins, &self.mcp]
                .iter()
                .any(|items| items.len() > 32 || items.iter().any(|value| !valid(value)))
            || self.native_config_refs.len() > 32
            || self.connection_refs.len() > 32
            || self.native_config_refs.iter().any(|reference| {
                !valid(&reference.kind)
                    || !reference.path.is_absolute()
                    || !reference.path.to_str().is_some_and(valid)
            })
            || self
                .connection_refs
                .iter()
                .any(|reference| !valid(&reference.kind) || !valid(&reference.name))
            || matches!(&self.model_policy, ModelPolicy::Exact { model } if !valid(model))
        {
            return Err(invalid());
        }
        Ok(())
    }
}
