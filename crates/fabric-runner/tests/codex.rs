use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use fabric_domain::{
    AgentEvent, AgentProfile, Capability, CapabilitySupport, CredentialConnectionRef, ErrorKind,
    HarnessRequest, ModelPolicy, NativeConfigRef,
};
use fabric_runner::{
    AgentSpec, CodexHarness, HarnessAdapter, HarnessRegistry, NativeEvent, RegistryOptions,
    capture::Capture,
};

#[test]
fn codex_is_registered_with_auth_free_doctor_discovery() {
    let registry = HarnessRegistry::default();
    assert!(registry.ids().iter().any(|id| id.as_str() == "codex"));
    assert!(registry.doctor_executables().contains(&"codex"));
}

const VERSION: &str = "codex-cli 0.171.0";
const SECRET: &str = "raw-codex-key-sentinel";
const PROMPT: &str = "--literal '$HOME'; $(touch not-a-command)\nsecond line";

fn env_request() -> HarnessRequest {
    HarnessRequest {
        profile: Some(AgentProfile {
            harness_id: "codex".parse().unwrap(),
            harness_version: VERSION.into(),
            native_config_refs: Vec::new(),
            model_policy: ModelPolicy::NativeDefault {},
            skills: Vec::new(),
            plugins: Vec::new(),
            mcp: Vec::new(),
            connection_refs: vec![CredentialConnectionRef {
                kind: "env".into(),
                name: "CODEX_API_KEY".into(),
            }],
        }),
        ..Default::default()
    }
}

#[test]
fn codex_fixture_conformance_through_public_runner_api() {
    if let Some(mode) = std::env::var_os("FABRIC_CODEX_TEST_CHILD") {
        let harness = CodexHarness::discover().unwrap();
        let info = harness.probe().unwrap();
        assert_eq!(info.version, VERSION);
        let manifest = harness.capabilities(&info).unwrap();
        assert_eq!(manifest.capabilities.len(), 13);
        let status = |capability| {
            manifest
                .capabilities
                .iter()
                .find(|status| status.capability == capability)
                .unwrap()
        };
        assert_eq!(
            status(Capability::ModelOverride).support,
            CapabilitySupport::Unsupported
        );
        assert_eq!(
            status(Capability::ModelOverride).reason.code,
            "MODEL_UNVERIFIED"
        );
        assert_eq!(
            status(Capability::ProfileImport).support,
            CapabilitySupport::RequiresConfiguration
        );
        assert_eq!(
            status(Capability::ToolApprovalNoninteractive).support,
            CapabilitySupport::Native
        );
        let mut request = env_request();
        if mode != "available" {
            let error = harness.plan(&request, &info).unwrap_err();
            assert_eq!(error.kind, ErrorKind::AuthorizationRequired);
            assert!(!format!("{error:?}").contains(SECRET));
            return;
        }
        assert_eq!(
            harness
                .plan(&HarnessRequest::default(), &info)
                .unwrap_err()
                .kind,
            ErrorKind::AuthorizationRequired
        );
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("owned-workspace"), "").unwrap();
        let prompt = root.path().join("prompt");
        fs::write(&prompt, PROMPT).unwrap();
        let plan = harness.plan(&request, &info).unwrap();
        assert!(!format!("{plan:?}").contains(SECRET));
        assert!(!serde_json::to_string(&plan).unwrap().contains(SECRET));
        assert_eq!(plan.profile.digest.len(), 64);
        assert!(plan.model_resolved.is_none());
        let command = harness
            .command(&AgentSpec {
                profile: &plan.profile,
                prompt_file: &prompt,
                max_cost_usd: None,
                request: &request,
            })
            .unwrap();
        assert_eq!(
            command.args,
            [
                "exec",
                "--json",
                "--ephemeral",
                "--color",
                "never",
                "--sandbox",
                "workspace-write",
                "--ignore-user-config",
                "--",
                PROMPT,
            ]
            .map(std::ffi::OsString::from)
        );
        assert!(
            command
                .args
                .iter()
                .all(|value| !value.to_string_lossy().contains(SECRET))
        );
        assert_eq!(
            command
                .environment
                .iter()
                .map(|(key, _)| key.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["CODEX_API_KEY", "PATH"]
        );
        let (metadata, events) = capture(&harness, &command, &workspace, 100_000);
        assert_eq!(
            fs::read_to_string(workspace.join("change.txt")).unwrap(),
            "fixture change\n"
        );
        assert_eq!(
            metadata.native_failure.as_deref(),
            Some("codex_terminal_error")
        );
        let usage = &events
            .iter()
            .find(|value| value["event"]["type"] == "usage")
            .unwrap()["event"];
        assert_eq!(usage["input_tokens"], 21);
        assert_eq!(usage["output_tokens"], 8);
        assert_eq!(usage["cached_input_tokens"], 5);
        assert!(usage["cost_usd"].is_null());
        assert_eq!(
            events
                .iter()
                .filter(|value| value["event"]["type"] == "output")
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .any(|value| value["event"]["text"] == "final answer")
        );
        assert_eq!(
            events
                .iter()
                .filter(|value| value["event"]["type"] == "failure")
                .count(),
            1
        );
        assert_eq!(events.last().unwrap()["event"]["success"], false);
        assert_eq!(events.last().unwrap()["event"]["exit_code"], 0);
        assert!(events.iter().all(|record| {
            !serde_json::to_string(&record["event"])
                .unwrap()
                .contains(SECRET)
        }));
        assert!(events.iter().any(|record| {
            serde_json::to_string(&record["value"])
                .unwrap()
                .contains(SECRET)
        }));
        assert!(!serde_json::to_string(&metadata).unwrap().contains(SECRET));
        for mut unsupported in [
            HarnessRequest {
                model: Some("requested".into()),
                ..request.clone()
            },
            HarnessRequest {
                resume: Some("native-session".into()),
                ..request.clone()
            },
            HarnessRequest {
                max_cost_usd: Some(0.1),
                ..request.clone()
            },
            HarnessRequest {
                required_capabilities: vec![Capability::SessionFollowup],
                ..request.clone()
            },
            HarnessRequest {
                required_capabilities: vec![Capability::ProfileImport],
                ..request.clone()
            },
            HarnessRequest {
                hard_read_only: true,
                ..request.clone()
            },
            HarnessRequest {
                access: "inspect".into(),
                ..request.clone()
            },
            HarnessRequest {
                access: "publish".into(),
                ..request.clone()
            },
        ] {
            assert_eq!(
                harness.plan(&unsupported, &info).unwrap_err().kind,
                ErrorKind::UnsupportedCapability
            );
            unsupported.placement = "remote".into();
            assert_eq!(
                harness.plan(&unsupported, &info).unwrap_err().kind,
                ErrorKind::UnsupportedPlacement
            );
        }
        for placement in ["remote", "byoc"] {
            assert_eq!(
                harness
                    .plan(
                        &HarnessRequest {
                            placement: placement.into(),
                            ..request.clone()
                        },
                        &info
                    )
                    .unwrap_err()
                    .kind,
                ErrorKind::UnsupportedPlacement
            );
        }
        for import in ["skills", "plugins", "mcp"] {
            let mut invalid = request.clone();
            let profile = invalid.profile.as_mut().unwrap();
            match import {
                "skills" => profile.skills.push("unsupported".into()),
                "plugins" => profile.plugins.push("unsupported".into()),
                _ => profile.mcp.push("unsupported".into()),
            }
            assert_eq!(
                harness.plan(&invalid, &info).unwrap_err().kind,
                ErrorKind::UnsupportedCapability
            );
        }
        let mut exact = request.clone();
        exact.profile.as_mut().unwrap().model_policy = ModelPolicy::Exact {
            model: "requested".into(),
        };
        assert_eq!(
            harness.plan(&exact, &info).unwrap_err().kind,
            ErrorKind::UnsupportedCapability
        );
        for (kind, name) in [
            ("env", "OPENAI_API_KEY"),
            ("env", "CODEX_ACCESS_TOKEN"),
            ("env", "MISSING"),
            ("native-auth", "native-config-dir"),
            ("native-auth", "native-home"),
        ] {
            let mut invalid = request.clone();
            invalid.profile.as_mut().unwrap().connection_refs = vec![CredentialConnectionRef {
                kind: kind.into(),
                name: name.into(),
            }];
            assert_eq!(
                harness.plan(&invalid, &info).unwrap_err().kind,
                ErrorKind::AuthorizationRequired
            );
        }
        let config = root.path().join("native-config");
        fs::create_dir(&config).unwrap();
        fs::write(config.join("config.toml"), "# nonsecret config v1\n").unwrap();
        request
            .profile
            .as_mut()
            .unwrap()
            .native_config_refs
            .push(NativeConfigRef {
                kind: "native-config-dir".into(),
                path: config.clone(),
            });
        let config_env_plan = harness.plan(&request, &info).unwrap();
        assert!(!config_env_plan.profile.portable);
        assert_ne!(config_env_plan.profile.digest, plan.profile.digest);
        fs::write(config.join("auth.json"), "opaque-auth-secret").unwrap();
        assert_eq!(
            harness.plan(&request, &info).unwrap_err().kind,
            ErrorKind::AuthorizationRequired,
            "an env connection must not unlock a selected native auth store"
        );
        assert!(
            harness
                .command(&AgentSpec {
                    profile: &config_env_plan.profile,
                    prompt_file: &prompt,
                    max_cost_usd: None,
                    request: &request,
                })
                .is_err(),
            "auth appearing after plan must invalidate env-only launch"
        );
        request.profile.as_mut().unwrap().connection_refs = vec![CredentialConnectionRef {
            kind: "native-auth".into(),
            name: "native-config-dir".into(),
        }];
        let native = harness.plan(&request, &info).unwrap();
        fs::write(
            config.join("auth.json"),
            "changed-auth-secret-not-fingerprinted",
        )
        .unwrap();
        assert_eq!(
            harness.plan(&request, &info).unwrap().profile.digest,
            native.profile.digest
        );
        assert_eq!(
            native
                .manifest
                .capabilities
                .iter()
                .find(|status| status.capability == Capability::ProfileImport)
                .unwrap()
                .support,
            CapabilitySupport::Native
        );
        let native_command = harness
            .command(&AgentSpec {
                profile: &native.profile,
                prompt_file: &prompt,
                max_cost_usd: None,
                request: &request,
            })
            .unwrap();
        assert!(
            !native_command
                .args
                .iter()
                .any(|arg| arg == "--ignore-user-config")
        );
        assert_eq!(
            native_command
                .environment
                .iter()
                .map(|(key, _)| key.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["CODEX_HOME", "PATH"]
        );
        capture(&harness, &native_command, &workspace, 100_000);
        fs::write(config.join("config.toml"), "# nonsecret config v2\n").unwrap();
        let changed = harness.plan(&request, &info).unwrap();
        assert_ne!(
            native.profile.native_config_fingerprint,
            changed.profile.native_config_fingerprint
        );
        let error = harness
            .command(&AgentSpec {
                profile: &native.profile,
                prompt_file: &prompt,
                max_cost_usd: None,
                request: &request,
            })
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::InvalidProfile);
        assert!(!format!("{error:?}").contains(&config.to_string_lossy().to_string()));
        assert!(!format!("{changed:?}").contains("changed-auth-secret"));
        fs::remove_file(config.join("auth.json")).unwrap();
        assert_eq!(
            harness.plan(&request, &info).unwrap_err().kind,
            ErrorKind::AuthorizationRequired
        );
        fs::create_dir(config.join("auth.json")).unwrap();
        assert_eq!(
            harness.plan(&request, &info).unwrap_err().kind,
            ErrorKind::AuthorizationRequired
        );
        fs::remove_dir(config.join("auth.json")).unwrap();
        request.profile.as_mut().unwrap().connection_refs =
            env_request().profile.unwrap().connection_refs;
        fs::write(config.join("config.toml"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        let error = harness.plan(&request, &info).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidProfile);
        assert!(!format!("{error:?}").contains(&config.to_string_lossy().to_string()));
        fs::remove_file(config.join("config.toml")).unwrap();
        std::os::unix::fs::symlink(config.join("missing"), config.join("config.toml")).unwrap();
        assert_eq!(
            harness.plan(&request, &info).unwrap_err().kind,
            ErrorKind::InvalidProfile
        );
        fs::remove_file(config.join("config.toml")).unwrap();
        let duplicate = request.profile.as_ref().unwrap().native_config_refs[0].clone();
        request
            .profile
            .as_mut()
            .unwrap()
            .native_config_refs
            .push(duplicate);
        assert_eq!(
            harness.plan(&request, &info).unwrap_err().kind,
            ErrorKind::InvalidProfile
        );
        request.profile.as_mut().unwrap().native_config_refs.pop();
        request.profile.as_mut().unwrap().native_config_refs[0].kind = "native-home".into();
        assert_eq!(
            harness.plan(&request, &info).unwrap_err().kind,
            ErrorKind::InvalidProfile
        );
        normalized_schema(&harness);
        // Native failure observation survives a full persisted capture budget.
        let cap_command = fabric_runner::CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "head -c 100000 /dev/zero; printf '\\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"private\"}}\\n'".into()],
            environment: Vec::new(),
        };
        let (metadata, _) = capture(&harness, &cap_command, &workspace, 256);
        assert!(metadata.truncated);
        assert_eq!(
            metadata.native_failure.as_deref(),
            Some("codex_terminal_error")
        );
        assert!(metadata.persisted_bytes <= 256);
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("codex");
    fs::write(&script, r#"#!/bin/sh
set -eu
[ -n "$HOME" ] && [ -d "$HOME" ] && [ ! -e "$HOME/ambient-marker" ] || exit 71
for value in "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"; do
  [ -d "$value" ] && [ "$value" != /hostile-xdg ] || exit 72
done
[ -z "${OPENAI_API_KEY-}${CODEX_ACCESS_TOKEN-}${HTTPS_PROXY-}${HTTP_PROXY-}${ALL_PROXY-}" ] || exit 73
if [ "$1" = --version ]; then
  [ "$#" = 1 ] && [ -z "${CODEX_API_KEY-}${CODEX_HOME-}" ] || exit 74
  printf 'codex-cli 0.171.0\n'
  exit 0
fi
[ -f owned-workspace ] || exit 75
for expected in exec --json --ephemeral --color never --sandbox workspace-write; do
  [ "$1" = "$expected" ] || exit 76
  shift
done
if [ -n "${CODEX_HOME-}" ]; then
  [ -f "$CODEX_HOME/config.toml" ] && [ -z "${CODEX_API_KEY-}" ] || exit 77
  case "$CODEX_HOME" in "$HOME"/*) exit 78;; esac
else
  [ "$1" = --ignore-user-config ] && [ "${CODEX_API_KEY-}" = raw-codex-key-sentinel ] || exit 79
  shift
fi
[ "$1" = -- ] && [ "$#" = 2 ] || exit 80
expected="--literal '\$HOME'; \$(touch not-a-command)
second line"
[ "$2" = "$expected" ] || exit 81
if IFS= read -r extra; then exit 82; fi
[ ! -e not-a-command ] || exit 83
printf 'fixture change\n' > change.txt
cat <<'EVENTS'
{"type":"thread.started","thread_id":"fixture"}
{"type":"turn.started"}
{"type":"item.updated","item":{"id":"a","type":"agent_message","text":"partial"}}
{"type":"item.completed","item":{"id":"a","type":"agent_message","text":"final answer"}}
{"type":"item.started","item":{"id":"c","type":"command_execution","command":"private shell text","status":"in_progress"}}
{"type":"item.completed","item":{"id":"c","type":"command_execution","command":"private shell text","status":"failed","aggregated_output":"private output","exit_code":1}}
{"type":"item.completed","item":{"id":"m","type":"mcp_tool_call","server":"private server","tool":"private tool","status":"failed","error":{"message":"raw-codex-key-sentinel"}}}
{"type":"item.completed","item":{"id":"f","type":"file_change","changes":[],"status":"failed"}}
{"type":"item.completed","item":{"id":"e","type":"error","message":"nonterminal private error"}}
{"type":"turn.completed","usage":{"input_tokens":21,"cached_input_tokens":5,"cache_write_input_tokens":2,"output_tokens":8,"reasoning_output_tokens":3}}
{"type":"error","message":"raw-codex-key-sentinel"}
EVENTS
"#).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.path().join("hostile-home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("ambient-marker"), "").unwrap();
    for mode in ["available", "missing", "empty"] {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "codex_fixture_conformance_through_public_runner_api",
                "--nocapture",
            ])
            .env("FABRIC_CODEX_TEST_CHILD", mode)
            .env("PATH", root.path())
            .env("HOME", &home)
            .env("CODEX_HOME", "/ambient-codex")
            .env("CODEX_API_KEY", SECRET)
            .env("OPENAI_API_KEY", "foreign-secret")
            .env("CODEX_ACCESS_TOKEN", "foreign-token")
            .env("HTTPS_PROXY", "http://hostile")
            .env("HTTP_PROXY", "http://hostile")
            .env("ALL_PROXY", "http://hostile");
        for name in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            child.env(name, "/hostile-xdg");
        }
        if mode == "missing" {
            child.env_remove("CODEX_API_KEY");
        }
        if mode == "empty" {
            child.env("CODEX_API_KEY", "");
        }
        let result = child.output().unwrap();
        assert!(
            result.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

fn capture(
    harness: &dyn HarnessAdapter,
    command: &fabric_runner::CommandSpec,
    workspace: &Path,
    limit: u64,
) -> (fabric_domain::CaptureMetadata, Vec<serde_json::Value>) {
    let events = tempfile::NamedTempFile::new().unwrap();
    let result = fabric_runner::execute(
        command,
        workspace,
        Capture::new(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            Some(File::create(events.path()).unwrap()),
            limit,
        )
        .unwrap()
        .with_harness(harness),
        &fabric_runner::ExecutionLimits::default(),
        &fabric_runner::CancellationToken::default(),
        None,
        |_| Ok(()),
    )
    .unwrap();
    assert!(
        result.status.success(),
        "fixture exited {:?}",
        result.status
    );
    let records = fs::read_to_string(events.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (result.capture.unwrap(), records)
}

fn normalized_schema(harness: &dyn HarnessAdapter) {
    let normalize = |value: serde_json::Value| harness.normalize_event(NativeEvent::Json(&value));
    for value in [
        serde_json::json!({"type":"thread.started","thread_id":"not-a-fabric-session"}),
        serde_json::json!({"type":"turn.started"}),
        serde_json::json!({"type":"item.started","item":{"type":"agent_message","text":"partial"}}),
        serde_json::json!({"type":"item.updated","item":{"type":"agent_message","text":"partial"}}),
        serde_json::json!({"type":"item.completed","item":{"type":"error","message":"nonterminal"}}),
        serde_json::json!({"type":"future.event","usage":{"input_tokens":999}}),
        serde_json::json!({"type":"turn.completed"}),
    ] {
        assert!(normalize(value).is_none());
    }
    assert!(
        harness
            .normalize_event(NativeEvent::Text("unknown"))
            .is_none()
    );
    assert!(matches!(
        normalize(serde_json::json!({"type":"turn.completed","usage":{}})),
        Some(AgentEvent::Usage {
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cost_usd: None
        })
    ));
    assert!(matches!(
        normalize(
            serde_json::json!({"type":"turn.completed","usage":{"input_tokens":-1,"output_tokens":0}})
        ),
        Some(AgentEvent::Usage {
            input_tokens: None,
            output_tokens: Some(0),
            cached_input_tokens: None,
            cost_usd: None
        })
    ));
    for name in ["command_execution", "mcp_tool_call", "file_change"] {
        for (kind, phase) in [
            ("item.started", "start"),
            ("item.updated", "update"),
            ("item.completed", "end"),
        ] {
            let event = normalize(serde_json::json!({"type":kind,"item":{"type":name,"status":"failed","error":{"message":SECRET},"command":SECRET,"tool":SECRET}})).unwrap();
            assert!(
                matches!(&event, AgentEvent::Tool { name: observed, phase: observed_phase } if observed == name && observed_phase == phase)
            );
            assert!(!format!("{event:?}").contains(SECRET));
        }
    }
    for kind in ["turn.failed", "error"] {
        let event =
            normalize(serde_json::json!({"type":kind,"error":{"message":SECRET},"message":SECRET}))
                .unwrap();
        assert!(
            matches!(&event, AgentEvent::Failure { code, .. } if code == "codex_terminal_error")
        );
        assert!(!format!("{event:?}").contains(SECRET));
    }
}

#[test]
fn registry_does_not_accept_fake_executable_override_for_codex() {
    let error = HarnessRegistry::default()
        .resolve(
            &"codex".parse().unwrap(),
            &RegistryOptions {
                fake_binary: Some("/must/not/execute".into()),
            },
        )
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorKind::InvalidArguments);
}
