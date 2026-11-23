use fabric_domain::HarnessRequest;
use fabric_runner::plugin::*;
use fabric_runner::{CancellationToken, HarnessAdapter};
use std::path::Path;
use std::time::Duration;

fn manifest(kind: PluginKind, mode: &str, dir: &Path) -> PluginManifest {
    let name = match kind {
        PluginKind::Harness => "harness",
        PluginKind::Target => "target",
        PluginKind::Connector => "connector",
    };
    let executable = "/usr/bin/python3".into();
    let args = vec![
        format!("{}/tests/fixtures/plugin.py", env!("CARGO_MANIFEST_DIR")),
        name.into(),
        mode.into(),
        dir.join("pid").display().to_string(),
    ];
    PluginManifest {
        id: format!("external-{name}").parse().unwrap(),
        version: "1.0.0".into(),
        protocol: ProtocolRange { min: 1, max: 1 },
        kind,
        executable,
        args,
        integrity: None,
        native: None,
        capabilities: vec![
            match kind {
                PluginKind::Harness => "harness.run",
                PluginKind::Target => "target.execute",
                PluginKind::Connector => "connector.evidence",
            }
            .into(),
        ],
    }
}
fn reaped(dir: &Path) {
    let pid: i32 = std::fs::read_to_string(dir.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "child survived host return"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
#[test]
fn target_start_requires_fresh_authority_before_dispatch_and_before_spawn() {
    for scenario in ["cancelled", "owner_invalid", "changed_during_plan"] {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::default();
        let target = PluginHost::default()
            .target(
                &manifest(PluginKind::Target, "concrete", dir.path()),
                &cancel,
            )
            .unwrap();
        if scenario == "cancelled" {
            cancel.cancel();
        }
        let marker = dir.path().join("UNAUTHORIZED");
        let spec = fabric_runner::CommandSpec {
            program: "/usr/bin/touch".into(),
            args: vec![marker.clone().into_os_string()],
            environment: vec![],
        };
        let capture = fabric_runner::capture::Capture::new(
            tempfile::tempfile().unwrap(),
            tempfile::tempfile().unwrap(),
            None,
            4096,
        )
        .unwrap();
        let mut validations = 0;
        let result = target.execute(
            "test",
            fabric_runner::ExecutionRequest {
                spec: &spec,
                cwd: dir.path(),
                capture,
                limits: &fabric_runner::ExecutionLimits::default(),
                cancellation: &cancel,
                owner_lease: None,
            },
            || {
                validations += 1;
                if scenario == "owner_invalid" || validations == 2 {
                    Err(fabric_runner::execution_error("Ownership changed"))
                } else {
                    Ok(())
                }
            },
            |_| panic!("unauthorized process started"),
        );
        assert!(result.is_err());
        assert!(!marker.exists());
        let calls = std::fs::read_to_string(dir.path().join("pid.calls")).unwrap();
        assert_eq!(
            calls.lines().any(|method| method == "target.start"),
            scenario == "changed_during_plan"
        );
        drop(target);
        reaped(dir.path());
    }
}

#[test]
fn target_rejects_acknowledgment_only_and_wrong_grant_without_executing() {
    use fabric_runner::target::Action;
    for mode in ["ack-only", "wrong-grant"] {
        let dir = tempfile::tempdir().unwrap();
        let target = PluginHost::default()
            .target(
                &manifest(PluginKind::Target, mode, dir.path()),
                &CancellationToken::default(),
            )
            .unwrap();
        let result = target.drive(
            "reclaim",
            "test",
            &[Action::Reclaim],
            serde_json::json!({}),
            |_| -> Result<((), serde_json::Value), fabric_domain::FabricError> {
                panic!("invalid target plan executed")
            },
        );
        assert!(result.is_err());
        drop(target);
        reaped(dir.path());
    }
}

#[test]
fn target_timeout_identifies_rpc_and_exchange_phase_without_host_effects() {
    use fabric_runner::target::Action;
    let dir = tempfile::tempdir().unwrap();
    let target = PluginHost::default()
        .target(
            &manifest(PluginKind::Target, "inspect-hang", dir.path()),
            &CancellationToken::default(),
        )
        .unwrap();
    let start = std::time::Instant::now();
    let error = target
        .drive(
            "inspect",
            "test",
            &[Action::Inspect],
            serde_json::json!({}),
            |_| -> Result<((), serde_json::Value), fabric_domain::FabricError> {
                panic!("unresponsive target authorized a host effect")
            },
        )
        .err()
        .unwrap();
    assert!(
        error
            .message
            .contains("RPC target.inspect (request 5, phase response_read,"),
        "{error}"
    );
    assert!(error.message.contains("received 0 bytes"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(3));
    drop(target);
    reaped(dir.path());
}

#[test]
fn external_kinds_and_harness_execution_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::default();
    let cancel = CancellationToken::default();
    assert_eq!(
        host.target(&manifest(PluginKind::Target, "ok", dir.path()), &cancel)
            .err()
            .unwrap()
            .kind,
        fabric_domain::ErrorKind::UnsupportedPlacement
    );
    reaped(dir.path());
    assert!(
        host.connector_evidence(&manifest(PluginKind::Connector, "ok", dir.path()), &cancel)
            .unwrap()
            .available
    );
    reaped(dir.path());
    let adapter = PluginHarness::new(manifest(PluginKind::Harness, "ok", dir.path())).unwrap();
    let info = adapter.probe().unwrap();
    reaped(dir.path());
    let request = HarnessRequest::default();
    let plan = adapter.plan(&request, &info).unwrap();
    let prompt = dir.path().join("prompt");
    std::fs::write(&prompt, "hello").unwrap();
    let command = adapter
        .command(&fabric_runner::AgentSpec {
            profile: &plan.profile,
            prompt_file: &prompt,
            max_cost_usd: None,
            request: &request,
        })
        .unwrap();
    let stdout = std::fs::File::create(dir.path().join("out")).unwrap();
    let stderr = std::fs::File::create(dir.path().join("err")).unwrap();
    let capture = fabric_runner::capture::Capture::new(stdout, stderr, None, 4096).unwrap();
    let result = fabric_runner::execute(
        &command,
        dir.path(),
        capture,
        &fabric_runner::ExecutionLimits::default(),
        &cancel,
        None,
        |_| Ok(()),
    )
    .unwrap();
    assert!(result.status.success());
    assert!(
        std::fs::read_to_string(dir.path().join("out"))
            .unwrap()
            .contains("external fixture ran")
    );
    let request = HarnessRequest {
        model: Some("not-supported".into()),
        ..Default::default()
    };
    assert!(adapter.plan(&request, &info).is_err());
}
#[test]
fn protocol_failures_are_bounded_reaped_and_secret_free() {
    for mode in [
        "protocol",
        "malformed",
        "oversized",
        "hang",
        "crash",
        "error",
        "reclaim",
        "health-hang",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let host = PluginHost {
            timeout: Duration::from_secs(2),
        };
        let start = std::time::Instant::now();
        let error = host
            .target(
                &manifest(PluginKind::Target, mode, dir.path()),
                &CancellationToken::default(),
            )
            .err()
            .unwrap();
        assert!(!format!("{error:?}").contains("SECRET_CANARY"));
        if mode == "health-hang" {
            assert!(
                error
                    .message
                    .contains("RPC health (request 2, phase response_read,"),
                "{error}"
            );
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(dir.path().join("pid").exists(), "{mode}: {error:?}");
        reaped(dir.path());
    }
}
#[test]
fn cancellation_reaps_live_plugin() {
    for kind in [
        PluginKind::Harness,
        PluginKind::Target,
        PluginKind::Connector,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::default();
        std::thread::scope(|scope| {
            let child_cancel = cancel.clone();
            let pid = dir.path().join("pid");
            scope.spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while !pid.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                child_cancel.cancel();
            });
            let host = PluginHost::default();
            let manifest = manifest(kind, "hang", dir.path());
            let error = match kind {
                PluginKind::Harness => host.harness_info(&manifest, &cancel).map(|_| ()),
                PluginKind::Target => host.target(&manifest, &cancel).map(|_| ()),
                PluginKind::Connector => host.connector_evidence(&manifest, &cancel).map(|_| ()),
            }
            .unwrap_err();
            assert!(error.message.contains("cancelled"));
        });
        reaped(dir.path());
    }
}
#[test]
fn registry_install_upgrade_revoke_and_independent_compatibility() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut registry = PluginRegistry::load(&root).unwrap();
    let healthy = manifest(PluginKind::Connector, "ok", &root);
    let manifest = manifest(PluginKind::Target, "ok", &root);
    registry.install(manifest.clone(), false).unwrap();
    assert!(registry.install(manifest.clone(), false).is_err());
    let mut unsupported = manifest.clone();
    unsupported.capabilities.push("workspace.delete".into());
    assert!(registry.install(unsupported, true).is_err());
    let mut next = manifest.clone();
    next.version = "2.0.0".into();
    next.protocol = ProtocolRange { min: 2, max: 3 };
    registry.install(next, true).unwrap();
    assert_eq!(registry.reports()[0].code, "unsupported_protocol");
    assert!(registry.select(&manifest.id, PluginKind::Target).is_err());
    registry.install(healthy.clone(), false).unwrap();
    registry.save(&root).unwrap();
    let mut registry = PluginRegistry::load(&root).unwrap();
    assert_eq!(registry.reports()[0].code, "unsupported_protocol");
    assert_eq!(registry.reports()[1].code, "compatible");
    let selected = registry.select(&healthy.id, PluginKind::Connector).unwrap();
    assert!(
        PluginHost::default()
            .connector_evidence(&selected, &CancellationToken::default())
            .unwrap()
            .available
    );
    reaped(&root);
    registry.install(manifest.clone(), true).unwrap();
    registry.revoke(&manifest.id).unwrap();
    registry.save(&root).unwrap();
    let registry = PluginRegistry::load(&root).unwrap();
    assert_eq!(registry.reports()[0].code, "revoked");
    assert!(registry.select(&manifest.id, PluginKind::Target).is_err());
    assert!(
        registry
            .select(&"missing".parse().unwrap(), PluginKind::Target)
            .is_err()
    );
    let mut duplicate = registry;
    duplicate.plugins.push(duplicate.plugins[0].clone());
    duplicate.save(&root).unwrap();
    assert!(PluginRegistry::load(&root).is_err());
}

#[test]
fn manifest_versions_and_file_inputs_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut plugin = manifest(PluginKind::Target, "ok", dir.path());
    for version in [
        "0.0.0",
        "1.2.3-alpha.1+build.007",
        "1.0.0+000",
        "1.0.0-0",
        "1.0.0-x-y",
    ] {
        plugin.version = version.into();
        plugin.validate().unwrap();
    }
    for version in [
        "+1.0.0",
        "01.0.0",
        "1.0",
        "1.0.0.0",
        "1.0.0-01",
        "1.0.0-",
        "1.0.0+",
        "1.0.0+a+b",
        "1.0.0-a..b",
        "1.0.0-😀",
    ] {
        plugin.version = version.into();
        assert!(plugin.validate().is_err(), "{version}");
    }
    plugin.version = "1.0.0".into();
    let path = dir.path().join("manifest.json");
    std::fs::write(&path, serde_json::to_vec(&plugin).unwrap()).unwrap();
    assert_eq!(PluginManifest::load(&path).unwrap().version, "1.0.0");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(PluginManifest::load(&link).is_err());
    assert!(PluginManifest::load(dir.path()).is_err());
    assert!(PluginManifest::load(Path::new("/dev/zero")).is_err());
    std::fs::write(&path, vec![b'x'; MAX_FRAME + 1]).unwrap();
    assert!(PluginManifest::load(&path).is_err());
}

#[test]
fn package_integrity_is_checked_before_control_and_harness_launch() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut plugin = manifest(PluginKind::Harness, "ok", dir.path());
    let source = std::fs::read(&plugin.args[0]).unwrap();
    let entry = bundle.join("entry.py");
    std::fs::write(&entry, &source).unwrap();
    plugin.args[0] = entry.display().to_string();
    let content = format!("entry.py\0{:x}\n", Sha256::digest(&source));
    plugin.integrity = Some(PluginIntegrity {
        root: bundle.canonicalize().unwrap(),
        sha256: format!("{:x}", Sha256::digest(content)),
    });
    PluginRegistry::default()
        .install(plugin.clone(), false)
        .unwrap();
    let adapter = PluginHarness::new(plugin.clone()).unwrap();
    let info = adapter.probe().unwrap();
    reaped(dir.path());
    let plan = adapter.plan(&HarnessRequest::default(), &info).unwrap();
    std::fs::remove_file(dir.path().join("pid")).unwrap();
    std::fs::write(&entry, "SECRET_CANARY: tampered").unwrap();
    let error = PluginHost::default()
        .harness_info(&plugin, &CancellationToken::default())
        .err()
        .unwrap();
    assert_eq!(error.message, "Plugin: integrity_mismatch.");
    assert!(!dir.path().join("pid").exists());
    assert!(
        PluginRegistry::default()
            .install(plugin.clone(), false)
            .is_err()
    );
    let prompt = dir.path().join("prompt");
    let error = match adapter.command(&fabric_runner::AgentSpec {
        profile: &plan.profile,
        prompt_file: &prompt,
        max_cost_usd: None,
        request: &HarnessRequest::default(),
    }) {
        Ok(_) => panic!("tampered package was admitted"),
        Err(error) => error,
    };
    assert_eq!(error.message, "Plugin: integrity_mismatch.");
    assert!(!dir.path().join("pid").exists());
}

#[test]
fn typescript_scaffolds_use_existing_native_host() {
    let Some(node) = std::env::var_os("FABRIC_PLUGIN_TEST_NODE") else {
        eprintln!("TypeScript host smoke skipped: set FABRIC_PLUGIN_TEST_NODE");
        return;
    };
    let node = std::fs::canonicalize(node).unwrap();
    let cli = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/typescript/cli.mjs");
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    for kind in [
        PluginKind::Harness,
        PluginKind::Target,
        PluginKind::Connector,
    ] {
        let name = match kind {
            PluginKind::Harness => "harness",
            PluginKind::Target => "target",
            PluginKind::Connector => "connector",
        };
        let project = dir.join(name);
        let run = |args: &[&std::ffi::OsStr]| {
            let output = std::process::Command::new(&node)
                .arg(&cli)
                .args(args)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("TMPDIR", &dir)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            output
        };
        run(&["create".as_ref(), name.as_ref(), project.as_os_str()]);
        run(&["test".as_ref(), project.as_os_str()]);
        // The content store contains shipped files, not development-only tsconfig.
        std::fs::remove_file(project.join("tsconfig.json")).unwrap();
        let pin = run(&["hash".as_ref(), project.as_os_str()]);
        let pin: serde_json::Value = serde_json::from_slice(&pin.stdout).unwrap();
        let mut plugin = manifest(kind, "ok", &dir);
        plugin.id = name.parse().unwrap();
        plugin.executable = node.clone();
        plugin.args = vec![project.join("plugin.ts").display().to_string()];
        plugin.integrity = Some(PluginIntegrity {
            root: project,
            sha256: pin["sha256"].as_str().unwrap().into(),
        });
        // Same admission boundary, without saving a registry inside a checkout.
        PluginRegistry::default()
            .install(plugin.clone(), false)
            .unwrap();
        let host = PluginHost::default();
        let cancel = CancellationToken::default();
        match kind {
            PluginKind::Harness => {
                assert_eq!(
                    host.harness_info(&plugin, &cancel).unwrap().version,
                    "1.0.0"
                );
            }
            PluginKind::Target => {
                host.target(&plugin, &cancel).unwrap().finish().unwrap();
            }
            PluginKind::Connector => {
                assert!(!host.connector_evidence(&plugin, &cancel).unwrap().available);
            }
        }
    }
}

fn native_normalizer(mode: &str, dir: &Path) -> PluginHarness {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let package = dir.canonicalize().unwrap().join("package");
    std::fs::create_dir(&package).unwrap();
    std::fs::set_permissions(&package, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = std::fs::read(format!(
        "{}/tests/fixtures/plugin.py",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    std::fs::write(package.join("plugin.py"), &source).unwrap();
    let hash = format!(
        "{:x}",
        Sha256::digest(format!("plugin.py\0{:x}\n", Sha256::digest(source)))
    );
    let mut plugin = manifest(PluginKind::Harness, mode, dir);
    plugin.args[0] = package.join("plugin.py").display().to_string();
    plugin.integrity = Some(PluginIntegrity {
        root: package,
        sha256: hash,
    });
    plugin.native = Some(fabric_runner::native_plugin::NativePluginMetadata {
        executable: "fixture".into(),
        root_environment: Vec::new(),
        fingerprint: fabric_runner::native_plugin::NativeFingerprint {
            domain: "fixture".into(),
            files: Vec::new(),
            roots: Vec::new(),
        },
    });
    PluginHarness::new(plugin).unwrap()
}

#[test]
fn native_normalization_reuses_one_process_and_rejects_lifecycle_or_unknown_fields() {
    for mode in ["ok", "normalize-spoof", "normalize-extra"] {
        let dir = tempfile::tempdir().unwrap();
        let adapter = native_normalizer(mode, dir.path());
        let output = dir.path().join("output");
        let events = dir.path().join("events");
        let capture = fabric_runner::capture::Capture::new(
            std::fs::File::create(&output).unwrap(),
            tempfile::tempfile().unwrap(),
            Some(std::fs::File::create(&events).unwrap()),
            8192,
        )
        .unwrap()
        .with_harness(&adapter);
        let result = fabric_runner::execute(
            &fabric_runner::CommandSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "printf '{\"fixture\":1}\\n{\"fixture\":2}\\n'".into(),
                ],
                environment: Vec::new(),
            },
            dir.path(),
            capture,
            &fabric_runner::ExecutionLimits::default(),
            &CancellationToken::default(),
            None,
            |_| Ok(()),
        )
        .unwrap();
        let metadata = result.capture.unwrap();
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "{\"fixture\":1}\n{\"fixture\":2}\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pid.starts")).unwrap(),
            "start\n"
        );
        let captured = std::fs::read_to_string(events).unwrap();
        assert!(!captured.contains("SECRET_CANARY"));
        if mode == "ok" {
            assert!(metadata.native_failure.is_none());
            assert_eq!(captured.matches("normalized fixture").count(), 2);
        } else {
            assert_eq!(
                metadata.native_failure.unwrap(),
                "external-plugin-normalization"
            );
            assert!(captured.contains("\"success\":false"));
        }
        drop(adapter);
        reaped(dir.path());
    }
}

#[test]
fn cancellation_interrupts_a_live_native_normalizer() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = native_normalizer("normalize-hang", dir.path());
    let cancel = CancellationToken::default();
    let start = std::time::Instant::now();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| {
            adapter.try_normalize_event(fabric_runner::NativeEvent::Text("fixture"), &cancel)
        });
        while !dir.path().join("pid").exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
        cancel.cancel();
        assert!(running.join().unwrap().is_err());
    });
    assert!(start.elapsed() < Duration::from_secs(2));
    reaped(dir.path());
}

#[test]
fn native_capture_respects_the_execution_deadline_and_preserves_cancellation() {
    for cancel_during_normalize in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mode = if cancel_during_normalize {
            "normalize-hang"
        } else {
            "normalize-slow"
        };
        let adapter = native_normalizer(mode, dir.path());
        let output = dir.path().join("output");
        let events = dir.path().join("events");
        let capture = fabric_runner::capture::Capture::new(
            std::fs::File::create(&output).unwrap(),
            tempfile::tempfile().unwrap(),
            Some(std::fs::File::create(&events).unwrap()),
            8192,
        )
        .unwrap()
        .with_harness(&adapter);
        let cancel = CancellationToken::default();
        let start = std::time::Instant::now();
        let mut script = "printf '{\"fixture\":1}\\n{\"fixture\":2}\\n{\"fixture\":3}\\n{\"fixture\":4}\\n{\"fixture\":5}\\n{\"fixture\":6}\\n'".to_string();
        if cancel_during_normalize {
            script.push_str("; exec /bin/sleep 60");
        }
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                if cancel_during_normalize {
                    while !dir.path().join("pid.normalizing").exists() {
                        assert!(
                            start.elapsed() < Duration::from_secs(5),
                            "normalizer did not start"
                        );
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    cancel.cancel();
                }
            });
            fabric_runner::execute(
                &fabric_runner::CommandSpec {
                    program: "/bin/sh".into(),
                    args: vec!["-c".into(), script.into()],
                    environment: Vec::new(),
                },
                dir.path(),
                capture,
                &fabric_runner::ExecutionLimits {
                    timeout: Duration::from_secs(if cancel_during_normalize { 10 } else { 1 }),
                    term_grace: Duration::from_millis(10),
                },
                &cancel,
                None,
                |_| Ok(()),
            )
            .unwrap()
        });
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(
            result.stop_reason,
            Some(if cancel_during_normalize {
                fabric_runner::StopReason::Cancelled
            } else {
                fabric_runner::StopReason::Timeout
            }),
        );
        assert_eq!(cancel.is_cancelled(), cancel_during_normalize);
        assert_eq!(result.status.success(), !cancel_during_normalize);
        if cancel_during_normalize {
            use std::os::unix::process::ExitStatusExt;
            assert!(matches!(
                result.status.signal(),
                Some(libc::SIGTERM | libc::SIGKILL)
            ));
        }
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "{\"fixture\":1}\n{\"fixture\":2}\n{\"fixture\":3}\n{\"fixture\":4}\n{\"fixture\":5}\n{\"fixture\":6}\n",
        );
        let metadata = result.capture.unwrap();
        assert!(metadata.native_failure.is_none());
        assert_eq!(metadata.event_counts.parsed, 6);
        let captured = std::fs::read_to_string(events).unwrap();
        assert!(!captured.contains("external-plugin-normalization"));
        assert!(captured.contains("\"success\":false"));
        reaped(dir.path());
        drop(adapter);
        reaped(dir.path());
    }
}
