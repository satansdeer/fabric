//! Deliberately different, version-pinned fixture launch and wire protocols.
use std::path::PathBuf;

use fabric_domain::{
    AgentEvent, Capability, CapabilitySupport, FabricError, HarnessCapabilityManifest, HarnessId,
    HarnessPlan, HarnessRequest,
};

use crate::{AgentSpec, CommandSpec, EventFormat, HarnessAdapter, HarnessInfo, NativeEvent};

pub struct FakeAHarness {
    binary: PathBuf,
}

impl FakeAHarness {
    pub const VERSION: &'static str = "fabric-fake-a 1";
    pub const DEFAULT_MODEL: &'static str = "fixture-a-default";

    pub fn new(binary: PathBuf) -> Result<Self, FabricError> {
        if !binary.is_absolute() {
            return Err(crate::execution_error(
                "Harness executable must be an absolute path.",
            ));
        }
        Ok(Self { binary })
    }
}

impl HarnessAdapter for FakeAHarness {
    fn id(&self) -> HarnessId {
        "fake-a".parse().expect("registered harness ID")
    }

    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let version = crate::probe_version_args(
            &self.binary,
            vec!["--protocol-a".into(), "--version".into()],
            Vec::new(),
        )?;
        crate::planning::check_version("fake-a", &version, Self::VERSION)?;
        Ok(HarnessInfo { version })
    }

    fn capabilities(&self, info: &HarnessInfo) -> Result<HarnessCapabilityManifest, FabricError> {
        let mut manifest = crate::planning::local_manifest(
            self.id(),
            info,
            Self::VERSION,
            CapabilitySupport::FabricManaged,
        )?;
        for status in &mut manifest.capabilities {
            if matches!(
                status.capability,
                Capability::ModelOverride | Capability::UsageReport
            ) {
                status.support = CapabilitySupport::Native;
                status.reason.code = "tested_fixture_protocol".into();
                status.reason.message = "The fixture enforces the exact model argument and emits deterministic native usage.".into();
                status.minimum_version = Some(Self::VERSION.into());
            }
        }
        Ok(manifest)
    }

    fn plan(
        &self,
        request: &HarnessRequest,
        info: &HarnessInfo,
    ) -> Result<HarnessPlan, FabricError> {
        let mut plan = crate::negotiate(request, self.capabilities(info)?, false)?;
        plan.model_resolved = Some(
            plan.request
                .model
                .as_deref()
                .unwrap_or(Self::DEFAULT_MODEL)
                .into(),
        );
        Ok(plan)
    }

    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        let plan = crate::profile::revalidate(self, spec, Self::VERSION)?;
        Ok(CommandSpec {
            program: self.binary.clone(),
            args: vec![
                "--protocol-a".into(),
                "--input".into(),
                spec.prompt_file.into(),
                "--model".into(),
                plan.request
                    .model
                    .as_deref()
                    .unwrap_or(Self::DEFAULT_MODEL)
                    .into(),
            ],
            environment: Vec::new(),
        })
    }

    fn event_format(&self) -> EventFormat {
        EventFormat::JsonLines
    }

    fn normalize_event(&self, event: NativeEvent<'_>) -> Option<AgentEvent> {
        let NativeEvent::Json(value) = event else {
            return None;
        };
        match value["kind"].as_str()? {
            "output" => Some(AgentEvent::Output {
                stream: "stdout".into(),
                text: value["body"].as_str()?.into(),
            }),
            "tool" => Some(AgentEvent::Tool {
                name: value["name"].as_str()?.into(),
                phase: value["phase"].as_str()?.into(),
            }),
            "usage" => Some(AgentEvent::Usage {
                input_tokens: value["input"].as_u64(),
                output_tokens: value["output"].as_u64(),
                cached_input_tokens: value["cached"].as_u64(),
                cost_usd: value["cost_usd"]
                    .as_f64()
                    .filter(|cost| cost.is_finite() && *cost >= 0.0),
            }),
            "fatal" => Some(AgentEvent::Failure {
                code: value["code"].as_str()?.into(),
                message: value["message"].as_str()?.into(),
            }),
            _ => None,
        }
    }
}

pub struct FakeBHarness {
    binary: PathBuf,
}

impl FakeBHarness {
    pub const VERSION: &'static str = "fabric-fake-b 1";

    pub fn new(binary: PathBuf) -> Result<Self, FabricError> {
        if !binary.is_absolute() {
            return Err(crate::execution_error(
                "Harness executable must be an absolute path.",
            ));
        }
        Ok(Self { binary })
    }
}

impl HarnessAdapter for FakeBHarness {
    fn id(&self) -> HarnessId {
        "fake-b".parse().expect("registered harness ID")
    }

    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let version = crate::probe_version_args(
            &self.binary,
            vec!["--protocol-b".into(), "--version".into()],
            Vec::new(),
        )?;
        crate::planning::check_version("fake-b", &version, Self::VERSION)?;
        Ok(HarnessInfo { version })
    }

    fn capabilities(&self, info: &HarnessInfo) -> Result<HarnessCapabilityManifest, FabricError> {
        let mut manifest = crate::planning::local_manifest(
            self.id(),
            info,
            Self::VERSION,
            CapabilitySupport::FabricManaged,
        )?;
        let status = manifest
            .capabilities
            .iter_mut()
            .find(|status| status.capability == Capability::EventsStructured)
            .expect("complete manifest");
        status.support = CapabilitySupport::FabricManaged;
        status.reason.code = "tested_plaintext_protocol".into();
        status.reason.message =
            "Fabric normalizes the fixture's bounded plaintext lines; native output is not JSONL."
                .into();
        Ok(manifest)
    }

    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        crate::profile::revalidate(self, spec, Self::VERSION)?;
        Ok(CommandSpec {
            program: self.binary.clone(),
            args: vec![
                "--protocol-b".into(),
                "--task".into(),
                spec.prompt_file.into(),
            ],
            environment: Vec::new(),
        })
    }

    fn event_format(&self) -> EventFormat {
        EventFormat::TextLines
    }

    fn normalize_event(&self, event: NativeEvent<'_>) -> Option<AgentEvent> {
        let NativeEvent::Text(text) = event else {
            return None;
        };
        if let Some(text) = text.strip_prefix("OUTPUT ") {
            Some(AgentEvent::Output {
                stream: "stdout".into(),
                text: text.into(),
            })
        } else if let Some(tool) = text.strip_prefix("TOOL ") {
            let (phase, name) = tool.split_once(' ')?;
            if !matches!(phase, "start" | "end") || name.is_empty() {
                return None;
            }
            Some(AgentEvent::Tool {
                name: name.into(),
                phase: phase.into(),
            })
        } else {
            text.strip_prefix("FATAL ")
                .map(|message| AgentEvent::Failure {
                    code: "fixture_fatal".into(),
                    message: message.into(),
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocols_have_distinct_argv_exact_versions_and_model_policy() {
        let a = FakeAHarness::new("/fixture".into()).unwrap();
        let b = FakeBHarness::new("/fixture".into()).unwrap();
        let request = HarnessRequest {
            model: Some("literal-model".into()),
            ..Default::default()
        };
        let info = HarnessInfo {
            version: FakeAHarness::VERSION.into(),
        };
        let plan = a.plan(&request, &info).unwrap();
        assert_eq!(plan.model_resolved.as_deref(), Some("literal-model"));
        let spec = AgentSpec {
            profile: &plan.profile,
            prompt_file: std::path::Path::new("/task"),
            request: &request,
            max_cost_usd: None,
        };
        assert_eq!(
            a.command(&spec).unwrap().args,
            [
                "--protocol-a",
                "--input",
                "/task",
                "--model",
                "literal-model"
            ]
            .map(std::ffi::OsString::from)
        );
        assert_eq!(
            b.command(&spec).err().unwrap().kind,
            fabric_domain::ErrorKind::UnsupportedCapability
        );
        let request = HarnessRequest::default();
        let plan = b
            .plan(
                &request,
                &HarnessInfo {
                    version: FakeBHarness::VERSION.into(),
                },
            )
            .unwrap();
        let spec = AgentSpec {
            profile: &plan.profile,
            prompt_file: std::path::Path::new("/task"),
            request: &request,
            max_cost_usd: None,
        };
        assert_eq!(
            b.command(&spec).unwrap().args,
            ["--protocol-b", "--task", "/task"].map(std::ffi::OsString::from)
        );
        for adapter in [&a as &dyn HarnessAdapter, &b as &dyn HarnessAdapter] {
            assert_eq!(
                adapter
                    .capabilities(&HarnessInfo {
                        version: "fabric-fake-harness 1".into()
                    })
                    .unwrap_err()
                    .kind,
                fabric_domain::ErrorKind::UnsupportedVersion
            );
        }
        let request = HarnessRequest {
            resume: Some("session".into()),
            ..Default::default()
        };
        assert_eq!(
            b.plan(
                &request,
                &HarnessInfo {
                    version: FakeBHarness::VERSION.into()
                }
            )
            .unwrap_err()
            .kind,
            fabric_domain::ErrorKind::UnsupportedCapability
        );
    }

    #[test]
    fn native_json_and_plaintext_are_not_interchangeable_and_absent_usage_is_unknown() {
        let a = FakeAHarness::new("/fixture".into()).unwrap();
        let b = FakeBHarness::new("/fixture".into()).unwrap();
        let value = serde_json::json!({"kind":"output","body":"json"});
        assert!(matches!(
            a.normalize_event(NativeEvent::Json(&value)),
            Some(AgentEvent::Output { .. })
        ));
        assert!(b.normalize_event(NativeEvent::Json(&value)).is_none());
        assert!(
            a.normalize_event(NativeEvent::Text("OUTPUT text"))
                .is_none()
        );
        assert!(matches!(
            b.normalize_event(NativeEvent::Text("OUTPUT text")),
            Some(AgentEvent::Output { .. })
        ));
        assert!(matches!(
            b.normalize_event(NativeEvent::Text("TOOL end fixture-write")),
            Some(AgentEvent::Tool { .. })
        ));
        assert!(
            b.normalize_event(NativeEvent::Text("TOOL invalid fixture-write"))
                .is_none()
        );
        assert!(b.normalize_event(NativeEvent::Text("USAGE 0")).is_none());
        assert_eq!(
            a.normalize_event(NativeEvent::Json(&serde_json::json!({"kind":"usage"}))),
            Some(AgentEvent::Usage {
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                cost_usd: None
            })
        );
        assert!(
            a.normalize_event(NativeEvent::Json(
                &serde_json::json!({"kind":"output","body":42})
            ))
            .is_none()
        );
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn probes_select_the_matching_protocol_and_reject_unpinned_versions() {
        let root = tempfile::tempdir().unwrap();
        let binary = root.path().join("fixture");
        std::fs::write(&binary, "#!/bin/sh\ncase \"$1 $2\" in\n'--protocol-a --version') printf 'fabric-fake-a 1\\n';;\n'--protocol-b --version') printf 'fabric-fake-b 1\\n';;\n*) exit 2;;\nesac\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let a = FakeAHarness::new(binary.clone()).unwrap();
        let b = FakeBHarness::new(binary.clone()).unwrap();
        assert_eq!(a.probe().unwrap().version, FakeAHarness::VERSION);
        assert_eq!(b.probe().unwrap().version, FakeBHarness::VERSION);
        std::fs::write(&binary, "#!/bin/sh\nprintf 'fabric-fake-a 2\\n'\n").unwrap();
        assert!(matches!(
            a.probe(),
            Err(FabricError {
                kind: fabric_domain::ErrorKind::UnsupportedVersion,
                ..
            })
        ));
        assert!(b.probe().is_err());
    }
}
