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
    PluginManifest {
        id: format!("external-{name}").parse().unwrap(),
        version: "1.0.0".into(),
        protocol: ProtocolRange { min: 1, max: 1 },
        kind,
        executable: "/usr/bin/python3".into(),
        args: vec![
            format!("{}/tests/fixtures/plugin.py", env!("CARGO_MANIFEST_DIR")),
            name.into(),
            mode.into(),
            dir.join("pid").display().to_string(),
        ],
        capabilities: vec![
            match kind {
                PluginKind::Harness => "harness.run",
                PluginKind::Target => "target.plan",
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
fn external_kinds_and_harness_execution_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::default();
    let cancel = CancellationToken::default();
    assert_eq!(
        host.target_plan(&manifest(PluginKind::Target, "ok", dir.path()), &cancel)
            .unwrap()
            .placement,
        "local"
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
            .target_plan(
                &manifest(PluginKind::Target, mode, dir.path()),
                &CancellationToken::default(),
            )
            .unwrap_err();
        assert!(!format!("{error:?}").contains("SECRET_CANARY"));
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
                PluginKind::Target => host.target_plan(&manifest, &cancel).map(|_| ()),
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
