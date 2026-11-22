use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{ErrorKind, FabricError};

macro_rules! harness_evidence {
    ($name:ident, $validate:expr, $kind:ident, $message:literal) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn validate(value: &str) -> Result<(), FabricError> {
                if !($validate)(value) {
                    return Err(FabricError {
                        kind: ErrorKind::$kind,
                        message: $message.into(),
                    });
                }
                Ok(())
            }
        }

        impl TryFrom<String> for $name {
            type Error = FabricError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::validate(&value)?;
                Ok(Self(value))
            }
        }

        impl FromStr for $name {
            type Err = FabricError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::validate(value)?;
                Ok(Self(value.to_owned()))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

// Syntax is extensible; adapter registration, not this type, decides availability.
harness_evidence!(
    HarnessId,
    |value: &str| !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
    InvalidId,
    "HarnessId must contain 1–128 lowercase ASCII letters, digits or '-'"
);

// Preserve exact probe evidence rather than interpreting vendor version formats.
harness_evidence!(
    HarnessVersion,
    |value: &str| !value.trim().is_empty()
        && value.len() <= 1024
        && !value.chars().any(char::is_control),
    UnsupportedVersion,
    "HarnessVersion must contain 1–1024 bytes of nonempty version evidence without control characters"
);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "headless.run")]
    HeadlessRun,
    #[serde(rename = "events.structured")]
    EventsStructured,
    #[serde(rename = "model.override")]
    ModelOverride,
    #[serde(rename = "session.resume")]
    SessionResume,
    #[serde(rename = "session.followup")]
    SessionFollowup,
    #[serde(rename = "profile.import")]
    ProfileImport,
    #[serde(rename = "skills.import")]
    SkillsImport,
    #[serde(rename = "mcp.configure")]
    McpConfigure,
    #[serde(rename = "execution.cancel")]
    ExecutionCancel,
    #[serde(rename = "usage.report")]
    UsageReport,
    #[serde(rename = "tool.approval.noninteractive")]
    ToolApprovalNoninteractive,
    #[serde(rename = "workspace.external")]
    WorkspaceExternal,
    #[serde(rename = "filesystem.patch.capture")]
    FilesystemPatchCapture,
}

impl Capability {
    pub const ALL: [Self; 13] = [
        Self::HeadlessRun,
        Self::EventsStructured,
        Self::ModelOverride,
        Self::SessionResume,
        Self::SessionFollowup,
        Self::ProfileImport,
        Self::SkillsImport,
        Self::McpConfigure,
        Self::ExecutionCancel,
        Self::UsageReport,
        Self::ToolApprovalNoninteractive,
        Self::WorkspaceExternal,
        Self::FilesystemPatchCapture,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::HeadlessRun => "headless.run",
            Self::EventsStructured => "events.structured",
            Self::ModelOverride => "model.override",
            Self::SessionResume => "session.resume",
            Self::SessionFollowup => "session.followup",
            Self::ProfileImport => "profile.import",
            Self::SkillsImport => "skills.import",
            Self::McpConfigure => "mcp.configure",
            Self::ExecutionCancel => "execution.cancel",
            Self::UsageReport => "usage.report",
            Self::ToolApprovalNoninteractive => "tool.approval.noninteractive",
            Self::WorkspaceExternal => "workspace.external",
            Self::FilesystemPatchCapture => "filesystem.patch.capture",
        }
    }
}

impl FromStr for Capability {
    type Err = FabricError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|capability| capability.as_str() == value)
            .ok_or_else(|| FabricError {
                kind: ErrorKind::InvalidArguments,
                message: format!("Unknown capability {value:?}."),
            })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilitySupport {
    Native,
    FabricManaged,
    Unsupported,
    RequiresConfiguration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityReason {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityStatus {
    pub capability: Capability,
    pub support: CapabilitySupport,
    pub reason: CapabilityReason,
    pub minimum_version: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HarnessCapabilityManifest {
    pub harness_id: HarnessId,
    pub exact_version: String,
    pub supported_versions: String,
    pub capabilities: Vec<CapabilityStatus>,
}

impl HarnessCapabilityManifest {
    pub fn unsupported(
        harness_id: HarnessId,
        exact_version: String,
        supported_versions: String,
    ) -> Self {
        Self {
            harness_id,
            exact_version,
            supported_versions,
            capabilities: Capability::ALL
                .into_iter()
                .map(|capability| CapabilityStatus {
                    capability,
                    support: CapabilitySupport::Unsupported,
                    reason: CapabilityReason {
                        code: "not_attested".into(),
                        message: "The adapter has not attested this capability.".into(),
                    },
                    minimum_version: None,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "UncheckedHarnessRequest")]
pub struct HarnessRequest {
    #[serde(default)]
    pub profile: Option<crate::AgentProfile>,
    pub model: Option<String>,
    pub required_capabilities: Vec<Capability>,
    pub placement: String,
    pub access: String,
    pub hard_read_only: bool,
    pub resume: Option<String>,
    pub max_cost_usd: Option<f64>,
}

impl Default for HarnessRequest {
    fn default() -> Self {
        Self {
            profile: None,
            model: None,
            required_capabilities: Vec::new(),
            placement: "local".into(),
            access: "edit".into(),
            hard_read_only: false,
            resume: None,
            max_cost_usd: None,
        }
    }
}

impl HarnessRequest {
    pub fn validate(&self) -> Result<(), FabricError> {
        if let Some(profile) = &self.profile {
            profile.validate()?;
        }
        let invalid = |message: &str| FabricError {
            kind: ErrorKind::InvalidArguments,
            message: message.into(),
        };
        if !matches!(self.placement.as_str(), "local" | "remote" | "byoc") {
            return Err(invalid("Placement must be local, remote or byoc."));
        }
        if !matches!(self.access.as_str(), "edit" | "inspect" | "publish") {
            return Err(invalid("Access must be edit, inspect or publish."));
        }
        for (name, value) in [("Model", &self.model), ("Resume reference", &self.resume)] {
            if value.as_ref().is_some_and(|value| {
                value.trim().is_empty() || value.len() > 1024 || value.chars().any(char::is_control)
            }) {
                return Err(FabricError {
                    kind: ErrorKind::InvalidArguments,
                    message: format!(
                        "{name} must contain 1–1024 bytes without control characters."
                    ),
                });
            }
        }
        if self
            .max_cost_usd
            .is_some_and(|cost| !cost.is_finite() || cost <= 0.0)
        {
            return Err(invalid("Max cost must be finite and positive."));
        }
        Ok(())
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct UncheckedHarnessRequest {
    profile: Option<crate::AgentProfile>,
    model: Option<String>,
    required_capabilities: Vec<Capability>,
    #[serde(default = "local_placement")]
    placement: String,
    #[serde(default = "edit_access")]
    access: String,
    hard_read_only: bool,
    resume: Option<String>,
    max_cost_usd: Option<f64>,
}

fn local_placement() -> String {
    "local".into()
}

fn edit_access() -> String {
    "edit".into()
}

impl TryFrom<UncheckedHarnessRequest> for HarnessRequest {
    type Error = FabricError;

    fn try_from(value: UncheckedHarnessRequest) -> Result<Self, Self::Error> {
        let request = Self {
            profile: value.profile,
            model: value.model,
            required_capabilities: value.required_capabilities,
            placement: value.placement,
            access: value.access,
            hard_read_only: value.hard_read_only,
            resume: value.resume,
            max_cost_usd: value.max_cost_usd,
        };
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HarnessPlan {
    pub request: HarnessRequest,
    pub manifest: HarnessCapabilityManifest,
    pub model_resolved: Option<String>,
    #[serde(default)]
    pub profile: crate::ProfileEvidence,
}
