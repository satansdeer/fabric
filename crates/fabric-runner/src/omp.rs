//! Pinned OMP print-mode adapter with explicit trusted-local profile authorization.
use std::path::PathBuf;

use fabric_domain::{
    AgentEvent, Capability, CapabilitySupport, FabricError, HarnessCapabilityManifest, HarnessId,
    HarnessPlan, HarnessRequest,
};

use crate::{AgentSpec, CommandSpec, HarnessAdapter, HarnessInfo, NativeEvent, execution_error};

const SUPPORTED_VERSION: &str = "omp/19.1.0";

#[path = "omp_profile.rs"]
mod profile;

pub struct OmpHarness {
    binary: PathBuf,
}

impl OmpHarness {
    pub fn discover() -> Result<Self, FabricError> {
        let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
        let binary = crate::discover_executable("omp", &path).ok_or_else(|| {
            execution_error("OMP executable not found on PATH. Install OMP and add its executable directory to PATH; use fabric doctor to inspect discovery.")
        })?;
        Ok(Self { binary })
    }
}

impl HarnessAdapter for OmpHarness {
    fn id(&self) -> HarnessId {
        "omp".parse().expect("registered harness ID")
    }

    fn probe(&self) -> Result<HarnessInfo, FabricError> {
        let version = crate::probe_version(
            &self.binary,
            crate::profile::runtime_environment(&self.binary)?,
        )?;
        crate::planning::check_version("omp", &version, SUPPORTED_VERSION)?;
        Ok(HarnessInfo { version })
    }

    fn capabilities(&self, info: &HarnessInfo) -> Result<HarnessCapabilityManifest, FabricError> {
        let mut manifest = crate::planning::local_manifest(
            self.id(),
            info,
            SUPPORTED_VERSION,
            CapabilitySupport::Native,
        )?;
        let usage = manifest
            .capabilities
            .iter_mut()
            .find(|status| status.capability == Capability::UsageReport)
            .expect("complete manifest");
        usage.support = CapabilitySupport::Native;
        usage.reason.code = "tested_message_end_usage".into();
        usage.reason.message = "Pinned print-mode assistant message_end reports token usage when available; absent usage/pricing remains unknown.".into();
        usage.minimum_version = Some(SUPPORTED_VERSION.into());
        let profile = manifest
            .capabilities
            .iter_mut()
            .find(|status| status.capability == Capability::ProfileImport)
            .expect("complete manifest");
        profile.support = CapabilitySupport::RequiresConfiguration;
        profile.reason.code = "explicit_native_profile_required".into();
        profile.reason.message = "Only explicit supported trusted-local native roots are importable; doctor does not inspect config or credentials.".into();
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
            status.reason.message =
                "Explicit supported native roots selected; trusted-local, not a portable snapshot."
                    .into();
        }
        crate::planning::negotiate_profile(&request, manifest, true, profile)
    }

    fn command(&self, spec: &AgentSpec<'_>) -> Result<CommandSpec, FabricError> {
        let plan = crate::profile::revalidate(self, spec, SUPPORTED_VERSION)?;
        let (profile, mut environment) = profile::resolve(&plan.request, &plan.manifest, true)?;
        if profile.digest != spec.profile.digest {
            return Err(crate::profile::invalid(
                "OMP native config changed while constructing the command; plan again.",
            ));
        }
        environment.extend(crate::profile::runtime_environment(&self.binary)?);
        let prompt = std::fs::read_to_string(spec.prompt_file)
            .map_err(|_| execution_error("OMP prompt must be a readable UTF-8 file."))?;
        if prompt.contains('\0') {
            return Err(execution_error(
                "OMP prompt cannot contain NUL in an argv value.",
            ));
        }
        let mut args = vec![
            "-p".into(),
            "--mode".into(),
            "json".into(),
            "--no-session".into(),
            "--no-title".into(),
        ];
        if let Some(cost) = spec.max_cost_usd {
            args.push(format!("--max-cost={cost}").into());
        }
        args.extend(["--".into(), prompt.into()]);
        Ok(CommandSpec {
            program: self.binary.clone(),
            args,
            environment,
        })
    }

    fn normalize_event(&self, event: NativeEvent<'_>) -> Option<AgentEvent> {
        // v19.1.0 print-mode.ts printableEvent and agent/src/types.ts:
        // message_update drops snapshots; authoritative usage is on message_end.
        let NativeEvent::Json(value) = event else {
            return None;
        };
        match value["type"].as_str()? {
            "message_update" => {
                let update = &value["assistantMessageEvent"];
                match update["type"].as_str()? {
                    "text_delta" => Some(AgentEvent::Output {
                        stream: "stdout".into(),
                        text: update["delta"].as_str()?.into(),
                    }),
                    _ => None,
                }
            }
            "tool_execution_start" | "tool_execution_end" => Some(AgentEvent::Tool {
                name: value["toolName"].as_str()?.into(),
                phase: if value["type"] == "tool_execution_start" {
                    "start"
                } else {
                    "end"
                }
                .into(),
            }),
            "message_end" if value["message"]["role"] == "assistant" => {
                let message = &value["message"];
                let usage = message.get("usage")?.as_object()?;
                Some(AgentEvent::Usage {
                    input_tokens: usage.get("input").and_then(serde_json::Value::as_u64),
                    output_tokens: usage.get("output").and_then(serde_json::Value::as_u64),
                    cached_input_tokens: usage.get("cacheRead").and_then(serde_json::Value::as_u64),
                    // The native cost field carries no pricing provenance;
                    // zero cannot distinguish missing pricing from free billing.
                    cost_usd: usage
                        .get("cost")
                        .and_then(|cost| cost["total"].as_f64())
                        .filter(|cost| cost.is_finite() && *cost > 0.0),
                })
            }
            "agent_end" if value["isTerminal"] == true => {
                let message = value["messages"]
                    .as_array()?
                    .iter()
                    .rev()
                    .find(|message| message["role"] == "assistant")?;
                if message["stopReason"] != "error" {
                    return None;
                }
                Some(AgentEvent::Failure {
                    code: "omp_terminal_error".into(),
                    message: message["errorMessage"]
                        .as_str()
                        .unwrap_or("Terminal assistant turn failed")
                        .into(),
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn probe_rejects_unsupported_omp_version_with_typed_failure() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("omp");
        fs::write(&binary, "#!/bin/sh\nprintf 'omp/99.0.0\\n'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let harness = OmpHarness { binary };
        assert!(matches!(
            harness.probe(),
            Err(FabricError {
                kind: fabric_domain::ErrorKind::UnsupportedVersion,
                ..
            })
        ));
    }

    #[test]
    fn exact_version_plan_preserves_native_cost_argv_and_rejects_unverified_features() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("omp");
        fs::write(&binary, "#!/bin/sh\nprintf 'omp/19.1.0\\n'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let harness = OmpHarness { binary };
        let info = harness.probe().unwrap();
        assert_eq!(info.version, "omp/19.1.0");
        let request = HarnessRequest {
            max_cost_usd: Some(0.25),
            profile: Some(fabric_domain::AgentProfile {
                harness_id: harness.id(),
                harness_version: SUPPORTED_VERSION.into(),
                native_config_refs: vec![fabric_domain::NativeConfigRef {
                    kind: "native-home".into(),
                    path: temp.path().to_owned(),
                }],
                model_policy: Default::default(),
                skills: Vec::new(),
                plugins: Vec::new(),
                mcp: Vec::new(),
                connection_refs: vec![fabric_domain::CredentialConnectionRef {
                    kind: "native-auth".into(),
                    name: "native-home".into(),
                }],
            }),
            ..HarnessRequest::default()
        };
        fs::create_dir_all(temp.path().join(".omp/agent")).unwrap();
        fs::write(
            temp.path().join(".omp/agent/agent.db"),
            "opaque-native-auth",
        )
        .unwrap();
        let plan = harness.plan(&request, &info).unwrap();
        assert_eq!(plan.manifest.capabilities.len(), 13);
        assert_eq!(plan.manifest.supported_versions, "omp/19.1.0");
        assert!(plan.model_resolved.is_none());
        let prompt = temp.path().join("prompt");
        fs::write(&prompt, "literal --prompt").unwrap();
        let command = harness
            .command(&AgentSpec {
                profile: &plan.profile,
                prompt_file: &prompt,
                request: &request,
                max_cost_usd: plan.request.max_cost_usd,
            })
            .unwrap();
        assert_eq!(
            command.args,
            [
                "-p",
                "--mode",
                "json",
                "--no-session",
                "--no-title",
                "--max-cost=0.25",
                "--",
                "literal --prompt",
            ]
            .map(OsString::from)
        );
        assert!(
            command
                .environment
                .iter()
                .any(|(name, value)| name == "HOME" && value == temp.path().as_os_str())
        );
        for request in [
            HarnessRequest {
                model: Some("requested".into()),
                ..request.clone()
            },
            HarnessRequest {
                resume: Some("session-ref".into()),
                ..request.clone()
            },
        ] {
            assert_eq!(
                harness.plan(&request, &info).unwrap_err().kind,
                fabric_domain::ErrorKind::UnsupportedCapability
            );
        }
        for cost in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let request = HarnessRequest {
                max_cost_usd: Some(cost),
                ..HarnessRequest::default()
            };
            assert_eq!(
                harness.plan(&request, &info).unwrap_err().kind,
                fabric_domain::ErrorKind::InvalidArguments
            );
        }
        for version in ["omp/19.1.1", "omp/19.1.0+build", "omp/19.1.0-fixture+exact"] {
            assert_eq!(
                harness
                    .capabilities(&HarnessInfo {
                        version: version.into(),
                    })
                    .unwrap_err()
                    .kind,
                fabric_domain::ErrorKind::UnsupportedVersion
            );
        }
    }

    #[test]
    fn probe_rejects_failed_non_omp_multiline_and_oversized_versions() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("omp");
        let harness = OmpHarness {
            binary: binary.clone(),
        };
        for body in [
            "printf 'omp/19.1.0\\n'; exit 7",
            "printf 'other/19.1.0\\n'",
            "printf 'omp/19.1.0\\nextra\\n'",
        ] {
            fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(harness.probe().is_err(), "{body}");
        }
        fs::write(
            &binary,
            format!("#!/bin/sh\nprintf '{}\\n'\n", "x".repeat(5000)),
        )
        .unwrap();
        assert!(harness.probe().is_err());
    }

    #[test]
    fn command_rejects_non_utf8_and_nul_without_starting_an_executable() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = temp.path().join("prompt");
        let harness = OmpHarness {
            binary: "/must/not/run".into(),
        };
        fs::create_dir_all(temp.path().join(".omp/agent")).unwrap();
        fs::write(temp.path().join(".omp/agent/agent.db"), "opaque-auth").unwrap();
        let request = HarnessRequest {
            profile: Some(fabric_domain::AgentProfile {
                harness_id: harness.id(),
                harness_version: SUPPORTED_VERSION.into(),
                native_config_refs: vec![fabric_domain::NativeConfigRef {
                    kind: "native-home".into(),
                    path: temp.path().to_owned(),
                }],
                model_policy: Default::default(),
                skills: Vec::new(),
                plugins: Vec::new(),
                mcp: Vec::new(),
                connection_refs: vec![fabric_domain::CredentialConnectionRef {
                    kind: "native-auth".into(),
                    name: "native-home".into(),
                }],
            }),
            ..Default::default()
        };
        let plan = harness
            .plan(
                &request,
                &HarnessInfo {
                    version: SUPPORTED_VERSION.into(),
                },
            )
            .unwrap();
        for bytes in [b"invalid \xff".as_slice(), b"nul \0".as_slice()] {
            fs::write(&prompt, bytes).unwrap();
            assert!(
                harness
                    .command(&AgentSpec {
                        profile: &plan.profile,
                        max_cost_usd: None,
                        prompt_file: &prompt,
                        request: &request,
                    })
                    .is_err()
            );
        }
    }

    #[test]
    fn pinned_print_events_preserve_unknown_usage_and_only_terminal_errors_are_fatal() {
        let harness = OmpHarness {
            binary: "/unused".into(),
        };
        let normalize =
            |value: serde_json::Value| harness.normalize_event(NativeEvent::Json(&value));
        assert_eq!(
            normalize(serde_json::json!({
                "type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hello"}
            })),
            Some(AgentEvent::Output {
                stream: "stdout".into(),
                text: "hello".into()
            })
        );
        assert!(matches!(
            normalize(
                serde_json::json!({"type":"tool_execution_end","toolName":"bash","isError":true})
            ),
            Some(AgentEvent::Tool { .. })
        ));
        assert!(normalize(serde_json::json!({"type":"message_end","message":{"role":"assistant","stopReason":"error"}})).is_none());
        assert_eq!(
            normalize(serde_json::json!({"type":"message_end","message":{
                "role":"assistant","usage":{"input":9,"cost":{"total":0}}
            }})),
            Some(AgentEvent::Usage {
                input_tokens: Some(9),
                output_tokens: None,
                cached_input_tokens: None,
                cost_usd: None
            })
        );
        assert_eq!(
            normalize(serde_json::json!({"type":"message_end","message":{
                "role":"assistant","usage":{"output":2,"cacheRead":3,"cost":{"total":0.2}}
            }})),
            Some(AgentEvent::Usage {
                input_tokens: None,
                output_tokens: Some(2),
                cached_input_tokens: Some(3),
                cost_usd: Some(0.2)
            })
        );
        for terminal in [false, true] {
            let event = normalize(serde_json::json!({"type":"agent_end","isTerminal":terminal,
                "messages":[{"role":"assistant","stopReason":"error","errorMessage":"request failed"}]}));
            assert_eq!(matches!(event, Some(AgentEvent::Failure { .. })), terminal);
        }
        assert!(normalize(serde_json::json!({"type":"agent_end","isTerminal":true,
            "messages":[{"role":"assistant","stopReason":"error"},{"role":"assistant","stopReason":"stop"}]})).is_none());
        assert!(
            normalize(serde_json::json!({"type":"unrecognized","error":"not a fatal event"}))
                .is_none()
        );
    }

    #[test]
    fn profile_binding_fingerprints_and_authorization_are_enforced_before_commands() {
        let root = tempfile::tempdir().unwrap();
        let agent = root.path().join(".omp/agent");
        fs::create_dir_all(&agent).unwrap();
        fs::write(agent.join("agent.db"), "secret-auth-sentinel").unwrap();
        fs::write(agent.join("config.yml"), "model: native\n").unwrap();
        let harness = OmpHarness {
            binary: "/must/not/run".into(),
        };
        let info = HarnessInfo {
            version: SUPPORTED_VERSION.into(),
        };
        assert_eq!(
            harness
                .plan(&HarnessRequest::default(), &info)
                .unwrap_err()
                .kind,
            fabric_domain::ErrorKind::AuthorizationRequired
        );
        let mut request = HarnessRequest {
            profile: Some(fabric_domain::AgentProfile {
                harness_id: harness.id(),
                harness_version: SUPPORTED_VERSION.into(),
                native_config_refs: vec![fabric_domain::NativeConfigRef {
                    kind: "native-home".into(),
                    path: root.path().to_owned(),
                }],
                model_policy: Default::default(),
                skills: Vec::new(),
                plugins: Vec::new(),
                mcp: Vec::new(),
                connection_refs: vec![fabric_domain::CredentialConnectionRef {
                    kind: "native-auth".into(),
                    name: "native-home".into(),
                }],
            }),
            ..Default::default()
        };
        let plan = harness.plan(&request, &info).unwrap();
        assert!(!plan.profile.portable);
        assert!(
            !serde_json::to_string(&plan.profile)
                .unwrap()
                .contains("secret-auth-sentinel")
        );
        fs::write(agent.join("agent.db"), "different-secret-auth").unwrap();
        assert_eq!(
            harness.plan(&request, &info).unwrap().profile.digest,
            plan.profile.digest
        );
        let prompt = root.path().join("prompt");
        fs::write(&prompt, "test").unwrap();
        let spec = AgentSpec {
            request: &request,
            profile: &plan.profile,
            prompt_file: &prompt,
            max_cost_usd: None,
        };
        assert!(harness.command(&spec).is_ok());
        fs::write(agent.join("config.yml"), "model: changed\n").unwrap();
        assert_ne!(
            harness
                .plan(&request, &info)
                .unwrap()
                .profile
                .native_config_fingerprint,
            plan.profile.native_config_fingerprint
        );
        assert!(harness.command(&spec).is_err());
        for mutation in 0..7 {
            let mut rejected = request.clone();
            let profile = rejected.profile.as_mut().unwrap();
            match mutation {
                0 => profile.harness_id = "fake-a".parse().unwrap(),
                1 => profile.harness_version = "omp/19.1.1".into(),
                2 => profile.native_config_refs[0].kind = "unknown-native-ref".into(),
                3 => profile.connection_refs[0].name = "unknown-connection".into(),
                4 => profile.skills.push("required-skill".into()),
                5 => profile.mcp.push("required-server".into()),
                _ => profile.plugins.push("required-plugin".into()),
            }
            assert!(harness.plan(&rejected, &info).is_err());
        }
        request.profile.as_mut().unwrap().connection_refs.clear();
        assert_eq!(
            harness.plan(&request, &info).unwrap_err().kind,
            fabric_domain::ErrorKind::AuthorizationRequired
        );
        request.profile.as_mut().unwrap().connection_refs.push(
            fabric_domain::CredentialConnectionRef {
                kind: "env".into(),
                name: "FABRIC_UNSUPPORTED_SECRET_ENV".into(),
            },
        );
        assert!(harness.plan(&request, &info).is_err());
        fs::write(agent.join("models.yml"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        request.profile.as_mut().unwrap().connection_refs =
            vec![fabric_domain::CredentialConnectionRef {
                kind: "native-auth".into(),
                name: "native-home".into(),
            }];
        assert!(harness.plan(&request, &info).is_err());
    }
}
