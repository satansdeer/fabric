use fabric_domain::ErrorKind;
use fabric_runner::native_plugin::{NativeFingerprint, NativePluginMetadata};
use fabric_runner::plugin::{
    PluginHarness, PluginHost, PluginIntegrity, PluginKind, PluginManifest, ProtocolRange,
};
use fabric_runner::{CancellationToken, HarnessAdapter};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn reference_probe_composes_command_budgets_and_reaps_timeout() {
    // Re-exec isolates PATH/HOME changes from the rest of the test suite.
    const CHILD: &str = "FABRIC_PROBE_BUDGET_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "reference_probe_composes_command_budgets_and_reaps_timeout",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let home = root.join("home");
    let pin = root.join("pin");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&pin).unwrap();
    fs::set_permissions(&pin, fs::Permissions::from_mode(0o700)).unwrap();
    let cli = root.join("omp");
    fs::write(
        &cli,
        r#"#!/bin/sh
base=${0%/*}
printf '%s %s %s\n' "$1" "$$" "$PPID" >> "$base/pids"
if [ -f "$base/hang" ] && [ "$1" = "--help" ]; then
    trap '' TERM
    exec /bin/sleep 30
fi
/bin/sleep 3
case "$1" in
    --version) printf 'omp/99.2.3\n' ;;
    --help) printf '%s\n' '-p' '--mode json' '--no-session' '--no-title' '--max-cost' ;;
    *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    unsafe {
        std::env::set_var("PATH", &root);
        std::env::set_var("HOME", &home);
    }
    let manifest = PluginManifest {
        id: "omp".parse().unwrap(),
        version: "0.1.0".into(),
        protocol: ProtocolRange { min: 1, max: 1 },
        kind: PluginKind::Harness,
        executable: Path::new(env!("CARGO_BIN_EXE_fabric-reference-harnesses"))
            .canonicalize()
            .unwrap(),
        args: vec!["omp".into()],
        capabilities: vec!["harness.run".into()],
        // This private empty fixture root has the SHA-256 of an empty inventory.
        integrity: Some(PluginIntegrity {
            root: pin,
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        }),
        native: Some(NativePluginMetadata {
            executable: "omp".into(),
            root_environment: vec![],
            fingerprint: NativeFingerprint {
                domain: "probe-budget-fixture".into(),
                files: vec![],
                roots: vec![],
            },
        }),
    };
    let host = PluginHost::default();
    let start = Instant::now();
    let info = host
        .harness_info(&manifest, &CancellationToken::default())
        .unwrap();
    assert_eq!(info.version, "omp/99.2.3");
    assert!(start.elapsed() >= Duration::from_secs(6));
    assert_reaped(&root);

    // Onboarding probes too; it must use the same composed boundary.
    let harness = PluginHarness::new(manifest.clone()).unwrap();
    let context = harness.native_context().unwrap();
    assert_eq!(context.profile.harness_version, "omp/99.2.3");
    assert!(!context.credential_available);
    assert_reaped(&root);

    fs::write(root.join("hang"), b"").unwrap();
    let error = host
        .harness_info(&manifest, &CancellationToken::default())
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_reaped(&root);
}

fn assert_reaped(root: &Path) {
    let records = fs::read_to_string(root.join("pids")).unwrap();
    let mut commands = records.lines();
    for expected in ["--version", "--help"] {
        let mut fields = commands.next().unwrap().split_whitespace();
        assert_eq!(fields.next(), Some(expected));
        for _ in 0..2 {
            let pid: libc::pid_t = fields.next().unwrap().parse().unwrap();
            assert!(pid > 1);
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "owned PID {pid} survived"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        assert!(fields.next().is_none());
    }
    assert!(commands.next().is_none());
    fs::remove_file(root.join("pids")).unwrap();
}
