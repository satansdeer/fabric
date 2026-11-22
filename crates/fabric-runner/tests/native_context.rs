use fabric_domain::{HarnessId, HarnessRequest, ModelPolicy};
use fabric_runner::{HarnessRegistry, RegistryOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
#[path = "support/reference_plugins.rs"]
mod reference_plugins;

#[test]
fn discovery_is_metadata_only_exact_and_uses_private_worker_home() {
    if std::env::var_os("FABRIC_NATIVE_CONTEXT_CHILD").is_some() {
        for (name, variable, default, auth, kind) in [
            (
                "omp",
                "PI_CODING_AGENT_DIR",
                ".omp/agent",
                "agent.db",
                "native-agent-dir",
            ),
            (
                "codex",
                "CODEX_HOME",
                ".codex",
                "auth.json",
                "native-config-dir",
            ),
        ] {
            let id: HarnessId = name.parse().unwrap();
            let adapter = HarnessRegistry::default()
                .resolve(&id, &RegistryOptions::default())
                .unwrap();
            let context = adapter.native_context().unwrap();
            let expected = std::env::var_os(variable)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(default));
            assert_eq!(context.profile.native_config_refs.len(), 1);
            assert_eq!(
                context.profile.native_config_refs[0].path,
                expected.canonicalize().unwrap()
            );
            assert_eq!(context.profile.native_config_refs[0].kind, kind);
            assert!(matches!(
                context.profile.model_policy,
                ModelPolicy::NativeDefault {}
            ));
            assert_eq!(
                context.credential_paths,
                vec![expected.canonicalize().unwrap().join(auth)]
            );
            assert!(context.credential_available);
            assert!(
                context
                    .profile
                    .connection_refs
                    .iter()
                    .all(|reference| reference.kind == "native-auth")
            );
            let request = HarnessRequest {
                profile: Some(context.profile.clone()),
                ..HarnessRequest::default()
            };
            let plan = adapter.plan(&request, &adapter.probe().unwrap()).unwrap();
            let prompt = expected.join("fixture-prompt");
            std::fs::write(&prompt, "fixture only").unwrap();
            let command = adapter
                .command(&fabric_runner::AgentSpec {
                    profile: &plan.profile,
                    prompt_file: &prompt,
                    max_cost_usd: None,
                    request: &plan.request,
                })
                .unwrap();
            assert!(
                command
                    .environment
                    .iter()
                    .all(|(key, _)| key != "OPENAI_API_KEY"
                        && key != "CODEX_API_KEY"
                        && key != "HOME")
            );
            assert!(context.exposure.contains("not a sandbox"));
            let config = expected.join(if name == "omp" {
                "settings.json"
            } else {
                "config.toml"
            });
            let info = adapter.probe().unwrap();
            std::os::unix::fs::symlink(expected.join(auth), &config).unwrap();
            assert!(
                adapter.plan(&request, &info).is_err(),
                "auth symlink must never be fingerprinted"
            );
            std::fs::remove_file(&config).unwrap();
            std::fs::hard_link(expected.join(auth), &config).unwrap();
            assert!(
                adapter.plan(&request, &info).is_err(),
                "auth hardlink must never be fingerprinted"
            );
            std::fs::remove_file(&config).unwrap();
            std::fs::remove_file(expected.join(auth)).unwrap();
            assert!(!adapter.native_context().unwrap().credential_available);
        }
        return;
    }
    for overrides in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let binaries = temporary.path().join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&binaries).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "discovery_is_metadata_only_exact_and_uses_private_worker_home",
                "--nocapture",
            ])
            .env_clear()
            .env("FABRIC_NATIVE_CONTEXT_CHILD", "1")
            .env(
                "FABRIC_PLUGIN_REGISTRY",
                reference_plugins::install(temporary.path()),
            )
            .env("HOME", &home)
            .env("PATH", &binaries)
            .env("OPENAI_API_KEY", "unapproved-fixture")
            .env("CODEX_API_KEY", "unapproved-fixture");
        for (name, variable, default, auth, version) in [
            (
                "omp",
                "PI_CODING_AGENT_DIR",
                ".omp/agent",
                "agent.db",
                "omp/19.1.0",
            ),
            (
                "codex",
                "CODEX_HOME",
                ".codex",
                "auth.json",
                "codex-cli 0.171.0",
            ),
        ] {
            let binary = binaries.join(name);
            std::fs::write(&binary, format!("#!/bin/sh\n[ \"$1\" = --version ] || exit 93\n[ -z \"$OPENAI_API_KEY$CODEX_API_KEY$PI_CODING_AGENT_DIR$CODEX_HOME\" ] || exit 94\nprintf '%s\\n' '{version}'\n")).unwrap();
            std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let directory = if overrides {
                temporary
                    .path()
                    .join(format!("Library/Application Support/{name}"))
            } else {
                home.join(default)
            };
            std::fs::create_dir_all(&directory).unwrap();
            let credential = directory.join(auth);
            std::fs::write(&credential, "opaque fixture authentication; do not inspect").unwrap();
            std::fs::set_permissions(credential, std::fs::Permissions::from_mode(0o000)).unwrap();
            if overrides {
                child.env(variable, directory);
            }
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
