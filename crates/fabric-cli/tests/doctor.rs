use std::process::Command;

#[test]
fn doctor_json_without_path_reports_platform_and_missing_tools() {
    let output = Command::new(env!("CARGO_BIN_EXE_fabric"))
        .args(["doctor", "--json"])
        .env_clear()
        .output()
        .expect("fabric doctor must be executable");

    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty());
    let expected = format!(
        "{{\"platform\":{{\"os\":\"{}\",\"arch\":\"{}\",\"supported\":{},\"primary_supported_os\":\"macos\"}},\"tools\":[{{\"name\":\"git\",\"path\":null}},{{\"name\":\"omp\",\"path\":null}}]}}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cfg!(target_os = "macos"),
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
}

#[test]
fn json_argument_errors_are_structured_and_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_fabric"))
        .args(["doctor", "--json", "--unknown"])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "invalid_arguments");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--unknown")
    );
}

#[test]
fn help_version_and_human_doctor_are_available() {
    for argument in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_fabric"))
            .arg(argument)
            .env_clear()
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains("fabric"));
        assert!(output.stderr.is_empty());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_fabric"))
        .arg("doctor")
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("primary supported OS: macOS"));
    assert!(text.contains("git: not found on PATH"));
    assert!(text.contains("omp: not found on PATH"));
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "fabric-doctor-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn discovery_is_deterministic_read_only_and_never_executes_tools() {
        let fixture = Fixture::new();
        for directory in ["first", "bin", "home", "cwd"] {
            fs::create_dir(fixture.0.join(directory)).unwrap();
        }
        let first = fixture.0.join("first");
        let bin = fixture.0.join("bin");
        fs::write(first.join("git"), "not executable").unwrap();
        fs::set_permissions(first.join("git"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::create_dir(first.join("omp")).unwrap();
        for name in ["git", "omp"] {
            let path = bin.join(name);
            fs::write(
                &path,
                "#!/bin/sh\nprintf invoked > \"$HOME/invoked\"\nexit 91\n",
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sentinel = fixture.0.join("home/sentinel");
        fs::write(&sentinel, "user data").unwrap();
        let path = std::env::join_paths([&first, &bin]).unwrap();
        let invoke = || {
            Command::new(env!("CARGO_BIN_EXE_fabric"))
                .args(["doctor", "--json"])
                .env_clear()
                .env("PATH", &path)
                .env("HOME", fixture.0.join("home"))
                .current_dir(fixture.0.join("cwd"))
                .output()
                .unwrap()
        };
        let output = invoke();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, invoke().stdout);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["platform"]["os"], std::env::consts::OS);
        assert_eq!(report["platform"]["supported"], cfg!(target_os = "macos"));
        assert_eq!(report["platform"]["primary_supported_os"], "macos");
        for (index, name) in ["git", "omp"].iter().enumerate() {
            assert_eq!(report["tools"][index]["name"], *name);
            assert_eq!(
                report["tools"][index]["path"],
                bin.join(name).to_str().unwrap()
            );
        }
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "user data");
        assert_eq!(fs::read_dir(fixture.0.join("home")).unwrap().count(), 1);
        assert_eq!(fs::read_dir(fixture.0.join("cwd")).unwrap().count(), 0);
    }

    #[test]
    fn executable_symlinks_and_relative_path_entries_are_discovered() {
        let fixture = Fixture::new();
        let target = fixture.0.join("actual");
        fs::write(&target, "#!/bin/sh\nexit 91\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, fixture.0.join("omp")).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_fabric"))
            .args(["doctor", "--json"])
            .env_clear()
            .env("PATH", ".")
            .current_dir(&fixture.0)
            .output()
            .unwrap();
        assert!(output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(report["tools"][0]["path"].is_null());
        // macOS may canonicalize /var to /private/var when resolving cwd.
        let discovered = PathBuf::from(report["tools"][1]["path"].as_str().unwrap());
        assert_eq!(
            discovered.canonicalize().unwrap(),
            target.canonicalize().unwrap()
        );
    }
}
