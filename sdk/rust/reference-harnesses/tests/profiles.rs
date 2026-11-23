use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use fabric_reference_harnesses::OmpHarness;
use fabric_runner::HarnessAdapter;

#[test]
fn ambient_profile_environment_is_not_a_probe_or_worker_authorization() {
    if let Some(mode) = std::env::var_os("FABRIC_PROFILE_TEST_CHILD") {
        if mode == "probe" {
            OmpHarness::discover().unwrap().probe().unwrap();
        } else {
            let output = tempfile::tempfile().unwrap();
            let result = fabric_runner::execute(
                &fabric_runner::CommandSpec {
                    program: std::env::var_os("FABRIC_PROFILE_TEST_SCRIPT")
                        .unwrap()
                        .into(),
                    args: Vec::new(),
                    environment: Vec::new(),
                },
                std::path::Path::new("/"),
                fabric_runner::capture::Capture::new(
                    output,
                    tempfile::tempfile().unwrap(),
                    None,
                    4096,
                )
                .unwrap(),
                &fabric_runner::ExecutionLimits::default(),
                &fabric_runner::CancellationToken::default(),
                None,
                |_| Ok(()),
            )
            .unwrap();
            assert!(
                result.status.success(),
                "worker must receive a private HOME and no ambient selectors"
            );
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("hostile-home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join("ambient-marker"), "not authorized").unwrap();
    let script = root.path().join("omp");
    std::fs::write(&script, r#"#!/bin/sh
[ -n "$HOME" ] && [ -d "$HOME" ] && [ ! -e "$HOME/ambient-marker" ] || exit 71
for value in "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"; do
  [ -n "$value" ] && [ -d "$value" ] && [ "$value" != /hostile-xdg ] || exit 72
done
[ -z "$OPENAI_API_KEY$OMP_PROFILE$PI_PROFILE$PI_CONFIG_FILES$CLAUDE_CONFIG_DIR$HTTPS_PROXY" ] || exit 73
printf 'omp/19.1.0\n'
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for mode in ["probe", "worker"] {
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ambient_profile_environment_is_not_a_probe_or_worker_authorization",
                "--nocapture",
            ])
            .env("FABRIC_PROFILE_TEST_CHILD", mode)
            .env("FABRIC_PROFILE_TEST_SCRIPT", &script)
            .env("PATH", root.path())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", "/hostile-xdg")
            .env("XDG_DATA_HOME", "/hostile-xdg")
            .env("XDG_STATE_HOME", "/hostile-xdg")
            .env("XDG_CACHE_HOME", "/hostile-xdg")
            .env("OPENAI_API_KEY", "raw-key-sentinel")
            .env("OMP_PROFILE", "hostile-profile")
            .env("PI_PROFILE", "hostile-profile")
            .env("PI_CONFIG_FILES", "/hostile-overlay")
            .env("CLAUDE_CONFIG_DIR", "/hostile-foreign")
            .env("HTTPS_PROXY", "http://hostile-proxy")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{mode} isolation failed: {}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
}

#[test]
fn managed_profile_model_policy_is_enforced_and_revalidated() {
    use fabric_domain::{AgentProfile, CredentialConnectionRef, HarnessRequest, ModelPolicy};
    use fabric_runner::{AgentSpec, FakeAHarness, HarnessInfo};
    let adapter = FakeAHarness::new("/must/not/run".into()).unwrap();
    let info = HarnessInfo {
        version: FakeAHarness::VERSION.into(),
    };
    let mut request = HarnessRequest {
        profile: Some(AgentProfile {
            harness_id: adapter.id(),
            harness_version: FakeAHarness::VERSION.into(),
            native_config_refs: Vec::new(),
            model_policy: ModelPolicy::Exact {
                model: "profile-model".into(),
            },
            skills: Vec::new(),
            plugins: Vec::new(),
            mcp: Vec::new(),
            connection_refs: Vec::new(),
        }),
        ..Default::default()
    };
    let plan = adapter.plan(&request, &info).unwrap();
    assert_eq!(plan.model_resolved.as_deref(), Some("profile-model"));
    assert!(plan.profile.portable);
    let command = adapter
        .command(&AgentSpec {
            profile: &plan.profile,
            request: &request,
            prompt_file: std::path::Path::new("/prompt"),
            max_cost_usd: None,
        })
        .unwrap();
    assert_eq!(command.args.last().unwrap(), "profile-model");
    request.model = Some("conflict".into());
    assert!(adapter.plan(&request, &info).is_err());
    assert!(
        adapter
            .command(&AgentSpec {
                profile: &plan.profile,
                request: &request,
                prompt_file: std::path::Path::new("/prompt"),
                max_cost_usd: None
            })
            .is_err()
    );
    request.model = None;
    request
        .profile
        .as_mut()
        .unwrap()
        .connection_refs
        .push(CredentialConnectionRef {
            kind: "env".into(),
            name: "OPENAI_API_KEY".into(),
        });
    assert!(adapter.plan(&request, &info).is_err());
}

#[test]
fn only_selected_approved_credential_env_reaches_the_worker() {
    use fabric_domain::{AgentProfile, CredentialConnectionRef, HarnessRequest};
    use fabric_runner::AgentSpec;
    if std::env::var_os("FABRIC_CONNECTION_TEST_CHILD").is_some() {
        let adapter = OmpHarness::discover().unwrap();
        let info = adapter.probe().unwrap();
        let mut request = HarnessRequest {
            profile: Some(AgentProfile {
                harness_id: adapter.id(),
                harness_version: "omp/19.1.0".into(),
                native_config_refs: Vec::new(),
                model_policy: Default::default(),
                skills: Vec::new(),
                plugins: Vec::new(),
                mcp: Vec::new(),
                connection_refs: vec![CredentialConnectionRef {
                    kind: "env".into(),
                    name: "OPENAI_API_KEY".into(),
                }],
            }),
            ..Default::default()
        };
        assert!(adapter.plan(&HarnessRequest::default(), &info).is_err());
        let plan = adapter.plan(&request, &info).unwrap();
        assert!(
            !serde_json::to_string(&plan)
                .unwrap()
                .contains("raw-key-sentinel")
        );
        let prompt = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(prompt.path(), "prompt").unwrap();
        let command = adapter
            .command(&AgentSpec {
                profile: &plan.profile,
                request: &request,
                prompt_file: prompt.path(),
                max_cost_usd: None,
            })
            .unwrap();
        assert!(
            command
                .args
                .iter()
                .all(|arg| !arg.to_string_lossy().contains("raw-key-sentinel"))
        );
        let output = fabric_runner::execute(
            &command,
            std::path::Path::new("/"),
            fabric_runner::capture::Capture::new(
                tempfile::tempfile().unwrap(),
                tempfile::tempfile().unwrap(),
                None,
                4096,
            )
            .unwrap(),
            &fabric_runner::ExecutionLimits::default(),
            &fabric_runner::CancellationToken::default(),
            None,
            |_| Ok(()),
        )
        .unwrap();
        assert!(output.status.success());
        request.profile.as_mut().unwrap().connection_refs[0].name = "GROQ_API_KEY".into();
        assert!(adapter.plan(&request, &info).is_err());
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("omp");
    std::fs::write(
        &script,
        r#"#!/bin/sh
[ -z "$ANTHROPIC_API_KEY$OMP_PROFILE$PI_PROFILE$PI_CONFIG_FILES$HTTPS_PROXY" ] || exit 71
[ -n "$HOME" ] && [ -d "$HOME" ] && [ ! -e "$HOME/ambient-marker" ] || exit 72
if [ "$1" = --version ]; then
  [ -z "$OPENAI_API_KEY" ] || exit 73
  printf 'omp/19.1.0\n'
else
  [ "$OPENAI_API_KEY" = raw-key-sentinel ] || exit 74
fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "only_selected_approved_credential_env_reaches_the_worker",
            "--nocapture",
        ])
        .env("FABRIC_CONNECTION_TEST_CHILD", "1")
        .env("PATH", root.path())
        .env("OPENAI_API_KEY", "raw-key-sentinel")
        .env("ANTHROPIC_API_KEY", "unauthorized-key")
        .env_remove("GROQ_API_KEY")
        .env("OMP_PROFILE", "hostile")
        .env("PI_PROFILE", "hostile")
        .env("PI_CONFIG_FILES", "/hostile")
        .env("HTTPS_PROXY", "http://hostile")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "credential isolation failed: {}",
        String::from_utf8_lossy(&result.stdout)
    );
}
