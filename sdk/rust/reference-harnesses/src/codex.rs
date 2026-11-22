//! Source-pinned Codex exec adapter. Live provider conformance is opt-in, not implied.
use std::path::PathBuf;

use fabric_domain::{
    AgentEvent, Capability, CapabilitySupport, FabricError, HarnessCapabilityManifest, HarnessId,
    HarnessPlan, HarnessRequest,
};

use crate::{AgentSpec, CommandSpec, HarnessAdapter, HarnessInfo, NativeEvent, execution_error};

const SUPPORTED_VERSION: &str = "codex-cli 0.171.0";

#[path = "codex_profile.rs"]
mod profile;

pub struct CodexHarness {
    binary: PathBuf,
}

impl CodexHarness {
    pub fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
    pub fn discover() -> Result<Self, FabricError> {
        let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
        let binary = crate::discover_executable("codex", &path).ok_or_else(|| {
            execution_error("Codex executable not found on PATH. Install the pinned Codex CLI and add its executable directory to PATH; use fabric doctor to inspect discovery.")
        })?;
        Ok(Self { binary })
    }
}

impl HarnessAdapter for CodexHarness {
    fn id(&self) -> HarnessId {
        "codex".parse().expect("registered harness ID")
    }

    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let version = crate::probe_version(
            &self.binary,
            crate::profile::runtime_environment(&self.binary)?,
        )?;
        crate::planning::check_version("codex", &version, SUPPORTED_VERSION)?;
        Ok(HarnessInfo { version })
    }

    fn native_context(&self) -> Result<crate::NativeContext, FabricError> {
        profile::discover(self.id(), self.probe()?.version)
    }

    fn capabilities(&self, info: &HarnessInfo) -> Result<HarnessCapabilityManifest, FabricError> {
        let mut manifest = crate::planning::local_manifest(
            self.id(),
            info,
            SUPPORTED_VERSION,
            CapabilitySupport::Native,
        )?;
        for status in &mut manifest.capabilities {
            let (support, code, message) = match status.capability {
                Capability::UsageReport => (
                    CapabilitySupport::Native,
                    "pinned_turn_completed_usage",
                    "Pinned exec turn.completed reports token usage when available; cost remains unknown.",
                ),
                Capability::ToolApprovalNoninteractive => (
                    CapabilitySupport::Native,
                    "pinned_exec_approval_never",
                    "Pinned headless exec defaults to never asking for approval with native workspace-write policy; this is not a Fabric hard-readonly sandbox.",
                ),
                Capability::ProfileImport => (
                    CapabilitySupport::RequiresConfiguration,
                    "explicit_native_profile_required",
                    "Only an explicitly selected native-config-dir is supported; trusted-local configuration is not a portable snapshot.",
                ),
                Capability::ModelOverride => (
                    CapabilitySupport::Unsupported,
                    "MODEL_UNVERIFIED",
                    "MODEL_UNVERIFIED: the pinned exec stream does not attest the resolved model; an argv model request is not observation.",
                ),
                Capability::SessionResume | Capability::SessionFollowup => (
                    CapabilitySupport::Unsupported,
                    "ephemeral_exec_only",
                    "The adapter launches ephemeral exec only; native session continuation is not implemented or attested.",
                ),
                _ => continue,
            };
            status.support = support;
            status.reason.code = code.into();
            status.reason.message = message.into();
            status.minimum_version =
                (status.support == CapabilitySupport::Native).then(|| SUPPORTED_VERSION.into());
        }
        Ok(manifest)
    }

    fn plan(
        &self,
        request: &HarnessRequest,
        info: &HarnessInfo,
    ) -> Result<HarnessPlan, FabricError> {
        let mut manifest = self.capabilities(info)?;
        let request = crate::profile::effective_request(request, &manifest)?;
        let (profile, _) = profile::resolve(&request, &manifest, false)?;
        if !request
            .profile
            .as_ref()
            .expect("resolved profile")
            .native_config_refs
            .is_empty()
        {
            let status = manifest
                .capabilities
                .iter_mut()
                .find(|status| status.capability == Capability::ProfileImport)
                .expect("complete manifest");
            status.support = CapabilitySupport::Native;
            status.reason.code = "explicit_native_trusted_local".into();
            status.reason.message = "Explicit native-config-dir selected; trusted-local, not a portable or atomic configuration snapshot.".into();
            status.minimum_version = Some(SUPPORTED_VERSION.into());
        }
        crate::planning::negotiate_profile(&request, manifest, false, profile)
    }

    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        let plan = crate::profile::revalidate(self, spec, SUPPORTED_VERSION)?;
        let (profile, mut environment) = profile::resolve(&plan.request, &plan.manifest, true)?;
        if profile.digest != spec.profile.digest {
            return Err(crate::profile::invalid(
                "Codex native config changed while constructing the command; plan again.",
            ));
        }
        environment.extend(crate::profile::runtime_environment(&self.binary)?);
        let prompt = std::fs::read_to_string(spec.prompt_file)
            .map_err(|_| execution_error("Codex prompt must be a readable UTF-8 file."))?;
        if prompt.contains('\0') {
            return Err(execution_error(
                "Codex prompt cannot contain NUL in an argv value.",
            ));
        }
        let mut args = vec![
            "exec".into(),
            "--json".into(),
            "--ephemeral".into(),
            "--color".into(),
            "never".into(),
            "--sandbox".into(),
            "workspace-write".into(),
        ];
        if plan
            .request
            .profile
            .as_ref()
            .expect("resolved profile")
            .native_config_refs
            .is_empty()
        {
            args.push("--ignore-user-config".into());
        }
        args.extend(["--".into(), prompt.into()]);
        Ok(CommandSpec {
            program: self.binary.clone(),
            args,
            environment,
        })
    }

    fn normalize_event(&self, event: NativeEvent<'_>) -> Option<AgentEvent> {
        // rust-v0.171.0 exec_events.rs: snapshots are cumulative, only completed text is final.
        let NativeEvent::Json(value) = event else {
            return None;
        };
        match value["type"].as_str()? {
            "item.completed" if value["item"]["type"] == "agent_message" => Some(AgentEvent::Output {
                stream: "stdout".into(),
                text: value["item"]["text"].as_str()?.into(),
            }),
            event @ ("item.started" | "item.updated" | "item.completed") => {
                let name = match value["item"]["type"].as_str()? {
                    "command_execution" => "command_execution",
                    "mcp_tool_call" => "mcp_tool_call",
                    "file_change" => "file_change",
                    _ => return None,
                };
                Some(AgentEvent::Tool {
                    name: name.into(),
                    phase: match event {
                        "item.started" => "start",
                        "item.updated" => "update",
                        _ => "end",
                    }.into(),
                })
            }
            "turn.completed" => {
                let usage = value.get("usage")?.as_object()?;
                Some(AgentEvent::Usage {
                    input_tokens: usage.get("input_tokens").and_then(serde_json::Value::as_u64),
                    output_tokens: usage.get("output_tokens").and_then(serde_json::Value::as_u64),
                    cached_input_tokens: usage.get("cached_input_tokens").and_then(serde_json::Value::as_u64),
                    cost_usd: None,
                })
            }
            "turn.failed" | "error" => Some(AgentEvent::Failure {
                code: "codex_terminal_error".into(),
                message: "Codex reported a terminal native failure; inspect the bounded raw artifact locally.".into(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn exact_version_probe_rejects_other_binaries_without_echoing_untrusted_output() {
        for version in [
            "codex-cli 0.170.0",
            "codex-cli 0.171.1",
            "codex-cli 0.171.0 extra",
            "codex 0.171.0",
            "private-version-secret",
        ] {
            let root = tempfile::tempdir().unwrap();
            let binary = root.path().join("codex");
            std::fs::write(&binary, format!(
                "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --version ] || exit 71\nprintf '%s\\n' '{version}'\n",
            )).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let harness = CodexHarness { binary };
            let error = harness.probe().err().unwrap();
            assert_eq!(error.kind, fabric_domain::ErrorKind::UnsupportedVersion);
            assert!(!format!("{error:?}").contains(version));
        }
    }
}
