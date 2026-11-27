#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
#[path = "../../fabric-runner/tests/support/reference_plugins.rs"]
mod reference_plugins;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fabric-onboarding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let path = path.canonicalize().unwrap();
        reference_plugins::install(&path);
        for dir in ["home", "bin", "source", "native"] {
            fs::create_dir(path.join(dir)).unwrap();
        }
        let stub = path.join("bin/codex");
        fs::write(&stub, "#!/bin/sh\nif [ \"$#\" = 1 ] && [ \"$1\" = --version ]; then printf 'codex-cli 0.171.0\\n'; exit 0; fi\nexit 91\n").unwrap();
        fs::set_permissions(stub, fs::Permissions::from_mode(0o700)).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "fixture",
            ],
        ] {
            assert!(
                Command::new("/usr/bin/git")
                    .args(args)
                    .current_dir(path.join("source"))
                    .env_clear()
                    .status()
                    .unwrap()
                    .success()
            );
        }
        fs::write(
            path.join("source/Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        Self(path)
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fabric"))
            .arg("--root")
            .arg(self.0.join("managed"))
            .args(args)
            .current_dir(self.0.join("source"))
            .env_clear()
            .env("HOME", self.0.join("home"))
            .env("FABRIC_PLUGIN_REGISTRY", self.0.join("plugin-registry"))
            .env("CODEX_HOME", self.0.join("native"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn init_check_reports_authless_context_without_mutation() {
    let fixture = Fixture::new();
    let output = fixture.cli(&["init", "--harness", "codex", "--check", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["harness_id"], "codex");
    assert_eq!(report["credential_available"], false);
    assert_eq!(report["connected"], false);
    assert_eq!(report["ready"], false);
    assert_eq!(report["dirty"], true);
    assert!(!fixture.0.join("managed").exists());
    assert_eq!(fs::read_dir(fixture.0.join("native")).unwrap().count(), 0);
    assert_eq!(
        fs::read_to_string(fixture.0.join("source/Cargo.toml")).unwrap(),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n"
    );
}

#[test]
fn connect_requires_consent_and_revoke_is_idempotent() {
    let fixture = Fixture::new();
    let output = fixture.cli(&["harness", "connect", "codex", "--json"]);
    assert!(
        !output.status.success(),
        "implicit consent must not succeed: {output:?}"
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("consent"),
        "{error}"
    );
    assert!(!fixture.0.join("managed").exists());
    for _ in 0..2 {
        let output = fixture.cli(&["harness", "revoke", "codex", "--json"]);
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn explicit_connection_is_private_selected_and_revocable() {
    let fixture = Fixture::new();
    // Opaque fixture data: the CLI must inspect metadata, never parse this.
    fs::write(fixture.0.join("native/auth.json"), "not JSON; fixture only").unwrap();
    let output = fixture.cli(&["harness", "connect", "codex", "--yes", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["connected"], true);
    assert_eq!(
        report["context"]["profile"]["native_config_refs"][0]["path"],
        fixture.0.join("native").to_str().unwrap()
    );
    fn private_tree(path: &std::path::Path) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert_eq!(
            metadata.permissions().mode() & 0o077,
            0,
            "{}",
            path.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                private_tree(&entry.unwrap().path());
            }
        }
    }
    private_tree(&fixture.0.join("managed"));
    let output = fixture.cli(&["init", "--check", "--harness", "codex", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["connected"], true);
    assert_eq!(report["credential_available"], true);
    assert!(
        fixture
            .cli(&["harness", "revoke", "codex", "--json"])
            .status
            .success()
    );
    let output = fixture.cli(&["init", "--check", "--harness", "codex", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["connected"], false);
    assert_eq!(fs::read_dir(fixture.0.join("source")).unwrap().count(), 2);
}

#[test]
fn up_requires_consent_before_probes_and_reuses_the_exact_ready() {
    let fixture = Fixture::new();
    let marker = fixture.0.join("probed");
    fs::write(
        fixture.0.join("bin/codex"),
        format!(
            "#!/bin/sh\ntouch '{}'\nprintf 'codex-cli 0.171.0\\n'\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::write(
        fixture.0.join("native/auth.json"),
        "opaque fixture auth, never copied",
    )
    .unwrap();
    let denied = fixture.cli(&["up", "--harness", "codex", "--source-only", "--json"]);
    assert!(!denied.status.success(), "{denied:?}");
    assert!(!marker.exists(), "unconsented setup executed a harness");
    assert!(!fixture.0.join("managed").exists());
    let args = [
        "up",
        "--harness",
        "codex",
        "--source-only",
        "--yes",
        "--json",
    ];
    let first = fixture.cli(&args);
    assert!(first.status.success(), "{first:?}");
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let grant = fs::read(fixture.0.join("managed/.native-context-codex.json")).unwrap();
    let second = fixture.cli(&args);
    assert!(second.status.success(), "{second:?}");
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(first["prepared_state_id"], second["prepared_state_id"]);
    assert_eq!(first["preparation_key"], second["preparation_key"]);
    assert_eq!(first["project"], "source");
    assert_eq!(
        grant,
        fs::read(fixture.0.join("managed/.native-context-codex.json")).unwrap()
    );
    assert!(!String::from_utf8_lossy(&grant).contains("opaque fixture auth"));
    assert!(
        second["next_command"]
            .as_str()
            .unwrap()
            .contains("workers spawn")
    );
}

#[test]
fn up_accepts_an_empty_directory_without_git_or_a_recipe() {
    let fixture = Fixture::new();
    let source = fixture.0.join("source");
    fs::remove_dir_all(source.join(".git")).unwrap();
    fs::remove_file(source.join("Cargo.toml")).unwrap();
    fs::write(fixture.0.join("native/auth.json"), "opaque fixture auth").unwrap();
    let args = ["up", "--harness", "codex", "--yes", "--json"];
    let first = fixture.cli(&args);
    assert!(first.status.success(), "{first:?}");
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert!(
        first["source_commit_sha"]
            .as_str()
            .unwrap()
            .starts_with("directory-sha256:")
    );
    assert_eq!(fs::read_dir(&source).unwrap().count(), 0);
    let second = fixture.cli(&args);
    assert!(second.status.success(), "{second:?}");
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(first["prepared_state_id"], second["prepared_state_id"]);
    fs::write(source.join("notes.txt"), "include current files").unwrap();
    let changed = fixture.cli(&args);
    assert!(changed.status.success(), "{changed:?}");
    let changed: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_ne!(first["prepared_state_id"], changed["prepared_state_id"]);
    assert_ne!(first["source_commit_sha"], changed["source_commit_sha"]);
    assert!(!source.join(".git").exists());
    assert_eq!(
        fs::read_to_string(source.join("notes.txt")).unwrap(),
        "include current files"
    );
}
